//! Startup text shown when launching the interactive TUI via bare `cockpit`.
//!
//! The real ratatui interface is still being wired, but both the fallback
//! text output and the interactive TUI share the same launch metadata.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use serde_json::Value;

use crate::git::{self, RepoStatus};

pub const APP_NAME: &str = "Cockpit CLI";
pub const DEFAULT_AGENT: &str = "orchestrator-build";
pub const MUTED_COLOR_INDEX: u8 = 250;
pub const INPUT_PREFIX: &str = "❯ ";
const P51_ANSI_LINES: [&str; 4] = [
    "    [38;5;255m█[0m   [38;5;196;48;5;16m▖[0m[38;5;250;48;5;244m▖[0m",
    "    [38;5;255m▐[0m[38;5;255;48;5;250m▌[0m[38;5;250m▄[0m[38;5;250;48;5;244m▛[0m[38;5;250;48;5;244m▘[0m ",
    "    [38;5;33;48;5;45m▖[0m[38;5;250;48;5;255m▛[0m[38;5;250;48;5;255m▀[0m   ",
    "  [38;5;220;48;5;208m▖[0m[38;5;220;48;5;208m▘[0m[38;5;208;48;5;250m▘[0m[38;5;244m▘[0m[38;5;255m▜[0m[38;5;255m▖[0m  ",
];
const RESET: &str = "[0m";
const BOLD: &str = "[1m";
const GREY: &str = "[38;5;250m";
const YELLOW: &str = "[33m";
const BRANCH_BADGE: &str = "[30;48;5;220m";

#[derive(Debug, Clone)]
pub struct LaunchInfo {
    pub version: &'static str,
    pub provider_line: String,
    pub cwd: PathBuf,
    pub cwd_display: String,
    pub repo_status: Option<RepoStatus>,
    pub agent_name: String,
}

pub fn load(project: Option<&Path>) -> LaunchInfo {
    let cwd = resolve_launch_dir(project);
    let provider_line = detect_provider_model(&cwd)
        .map(|(provider, model)| format!("{provider} / {model}"))
        .unwrap_or_else(|| "No providers configured - run /settings to edit".to_string());
    let repo_status = git::repo_status(&cwd).ok().flatten();

    LaunchInfo {
        version: env!("CARGO_PKG_VERSION"),
        provider_line,
        cwd_display: display_path(&cwd),
        cwd,
        repo_status,
        agent_name: DEFAULT_AGENT.to_string(),
    }
}

pub fn print(project: Option<&Path>) {
    let info = load(project);
    print_header(&info);
    println!();
    println!("{INPUT_PREFIX}");
    println!("{}", info.agent_name);
}

/// Print just the launch header (4 lines: logo + title, logo + provider,
/// logo + path, logo bottom). Used by the TUI at startup so the header
/// lands in normal terminal output — it scrolls naturally with the chat
/// and ends up in scrollback once enough messages arrive.
///
/// Spacing: P51 art renders 10 columns wide; the 3-space separator lines
/// content up at column 13, matching the TUI's 11-wide icon column +
/// 2-space text indent.
pub fn print_header(info: &LaunchInfo) {
    let title = format!("{BOLD}{APP_NAME}{RESET} {GREY}v{}{RESET}", info.version);
    println!("{}   {}", P51_ANSI_LINES[0], title);
    println!("{}   {GREY}{}{RESET}", P51_ANSI_LINES[1], info.provider_line);
    println!("{}   {}", P51_ANSI_LINES[2], path_line_ansi(info));
    println!("{}", P51_ANSI_LINES[3]);
}

pub fn status_line_spans(info: &LaunchInfo) -> Vec<Span<'static>> {
    git_segment_spans(info)
}

fn git_segment_spans(info: &LaunchInfo) -> Vec<Span<'static>> {
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let mut spans = vec![Span::styled(info.cwd_display.clone(), muted)];

    if let Some(repo) = &info.repo_status {
        let badge = Style::default().fg(Color::Black).bg(Color::Indexed(220));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!(" {} ", repo.branch), badge));
        let counts = repo_counts(repo);
        if !counts.is_empty() {
            spans.push(Span::styled(format!("{counts} "), badge));
        }
    }

    spans
}

fn resolve_launch_dir(project: Option<&Path>) -> PathBuf {
    let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    match project {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => base.join(path),
        None => base,
    }
}

fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(relative) = path.strip_prefix(&home) {
            if relative.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", relative.display());
        }
    }
    path.display().to_string()
}

fn path_line_ansi(info: &LaunchInfo) -> String {
    let mut line = format!("{GREY}{}{RESET}", info.cwd_display);
    if let Some(repo) = &info.repo_status {
        line.push(' ');
        line.push_str(BRANCH_BADGE);
        line.push(' ');
        line.push_str(&repo.branch);
        let counts = repo_counts(repo);
        if !counts.is_empty() {
            line.push(' ');
            line.push_str(&counts);
        }
        line.push(' ');
        line.push_str(RESET);
    }
    line
}

fn repo_counts(repo: &RepoStatus) -> String {
    let mut parts = Vec::new();
    if repo.staged > 0 {
        parts.push(format!("+{}", repo.staged));
    }
    if repo.unstaged > 0 {
        parts.push(format!("~{}", repo.unstaged));
    }
    if repo.unpushed > 0 {
        parts.push(format!("^{}", repo.unpushed));
    }
    parts.join(" ")
}

/// Where a cockpit config directory was discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDirKind {
    /// `~/.config/cockpit/`
    HomeXdg,
    /// `~/.cockpit/`
    HomeDot,
    /// An ancestor of cwd containing `.cockpit/` (project-scoped layer).
    Project,
}

#[derive(Debug, Clone)]
pub struct ConfigDir {
    pub kind: ConfigDirKind,
    pub path: PathBuf,
}

/// All cockpit config directories that exist on disk and apply to `cwd`.
///
/// Walk order: home-scoped first (XDG, then dotfile), then every ancestor
/// of `cwd` containing `.cockpit/`, walking from `cwd` upward and stopping
/// at the `{$HOME, /srv, /opt}` stop set (matches the layered-config plan).
pub fn discover_config_dirs(cwd: &Path) -> Vec<ConfigDir> {
    let mut out = Vec::new();

    if let Some(home) = dirs::home_dir() {
        let xdg = home.join(".config/cockpit");
        if xdg.is_dir() {
            out.push(ConfigDir {
                kind: ConfigDirKind::HomeXdg,
                path: xdg,
            });
        }
        let dot = home.join(".cockpit");
        if dot.is_dir() {
            out.push(ConfigDir {
                kind: ConfigDirKind::HomeDot,
                path: dot,
            });
        }
    }

    let stops: Vec<PathBuf> = [
        dirs::home_dir(),
        Some(PathBuf::from("/srv")),
        Some(PathBuf::from("/opt")),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut cursor = Some(cwd);
    while let Some(dir) = cursor {
        if stops.iter().any(|s| dir == s) {
            break;
        }
        let candidate = dir.join(".cockpit");
        if candidate.is_dir() {
            out.push(ConfigDir {
                kind: ConfigDirKind::Project,
                path: candidate,
            });
        }
        cursor = dir.parent();
    }

    out
}

/// Default places `/settings` will offer when no config exists yet.
pub fn creatable_config_dirs() -> Vec<ConfigDir> {
    let mut out = Vec::new();
    if let Some(home) = dirs::home_dir() {
        out.push(ConfigDir {
            kind: ConfigDirKind::HomeXdg,
            path: home.join(".config/cockpit"),
        });
        out.push(ConfigDir {
            kind: ConfigDirKind::HomeDot,
            path: home.join(".cockpit"),
        });
    }
    out
}

/// Create `dir` (and parents) and write a minimal `config.json` if one
/// isn't already present. Returns the path of the config file.
pub fn scaffold_config_dir(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let config_path = dir.join("config.json");
    if !config_path.exists() {
        let default = "{\n  \"providers\": {},\n  \"agents\": {},\n  \"tools\": {}\n}\n";
        std::fs::write(&config_path, default)?;
    }
    Ok(config_path)
}

fn detect_provider_model(cwd: &Path) -> Option<(String, String)> {
    detect_provider_model_from_env().or_else(|| detect_provider_model_from_configs(cwd))
}

fn detect_provider_model_from_env() -> Option<(String, String)> {
    let provider = env::var("COCKPIT_PROVIDER")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let model = env::var("COCKPIT_MODEL").ok().filter(|s| !s.trim().is_empty());

    match (provider, model) {
        (Some(provider), Some(model)) => Some((provider, model)),
        (None, Some(model)) => split_provider_model(&model),
        _ => None,
    }
}

fn detect_provider_model_from_configs(cwd: &Path) -> Option<(String, String)> {
    let mut selected = None;

    for path in config_candidates(cwd) {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(pair) = extract_provider_model(&json) {
            selected = Some(pair);
        }
    }

    selected
}

fn config_candidates(cwd: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".cockpit/config.json"));
        candidates.push(home.join(".config/cockpit/config.json"));

        let mut layered_dirs = Vec::new();
        let mut cursor = Some(cwd);
        while let Some(dir) = cursor {
            layered_dirs.push(dir.to_path_buf());
            if dir == home {
                break;
            }
            cursor = dir.parent();
        }
        layered_dirs.reverse();
        for dir in layered_dirs {
            candidates.push(dir.join(".cockpit/config.json"));
        }
    } else {
        let mut layered_dirs = cwd.ancestors().map(Path::to_path_buf).collect::<Vec<_>>();
        layered_dirs.reverse();
        for dir in layered_dirs {
            candidates.push(dir.join(".cockpit/config.json"));
        }
    }

    candidates
}

fn extract_provider_model(json: &Value) -> Option<(String, String)> {
    let default_provider = read_string(json.pointer("/models/categories/default/provider"));
    let default_model = read_string(json.pointer("/models/categories/default/model"));
    if let (Some(provider), Some(model)) = (default_provider, default_model) {
        return Some((provider, model));
    }

    let top_level_provider = read_string(json.pointer("/provider"));
    let top_level_model = read_string(json.pointer("/model"));
    if let (Some(provider), Some(model)) = (top_level_provider, top_level_model) {
        return Some((provider, model));
    }

    for pointer in ["/default_model", "/models/default_model", "/model"] {
        if let Some(model) = read_string(json.pointer(pointer)) {
            if let Some(pair) = split_provider_model(&model) {
                return Some(pair);
            }
        }
    }

    None
}

fn read_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn split_provider_model(value: &str) -> Option<(String, String)> {
    let (provider, model) = value.split_once('/')?;
    let provider = provider.trim();
    let model = model.trim();
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some((provider.to_string(), model.to_string()))
}
