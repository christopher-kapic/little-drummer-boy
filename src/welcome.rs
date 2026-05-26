//! Startup text shown when launching the interactive TUI via bare `cockpit`.
//!
//! The real ratatui interface is still being wired, but both the fallback
//! text output and the interactive TUI share the same launch metadata.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::git::{self, RepoStatus};

pub const APP_NAME: &str = "Cockpit CLI";
pub const DEFAULT_AGENT: &str = "orchestrator-build";
pub const ICON_WIDTH: u16 = 11;
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
    let title = format!("{BOLD}{APP_NAME}{RESET} {GREY}v{}{RESET}", info.version);

    println!("{}  {}", P51_ANSI_LINES[0], title);
    println!("{}  {GREY}{}{RESET}", P51_ANSI_LINES[1], info.provider_line);
    println!("{}  {}", P51_ANSI_LINES[2], path_line_ansi(&info));
    println!("{}", P51_ANSI_LINES[3]);
    println!();
    println!("{INPUT_PREFIX}");
    println!("{}", info.agent_name);
}

pub fn p51_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::raw("    "),
            fg("█", 255),
            Span::raw("   "),
            fg_bg("▖", 196, 16),
            fg_bg("▖", 250, 244),
        ]),
        Line::from(vec![
            Span::raw("    "),
            fg("▐", 255),
            fg_bg("▌", 255, 250),
            fg("▄", 250),
            fg_bg("▛", 250, 244),
            fg_bg("▘", 250, 244),
            Span::raw(" "),
        ]),
        Line::from(vec![
            Span::raw("    "),
            fg_bg("▖", 33, 45),
            fg_bg("▛", 250, 255),
            fg_bg("▀", 250, 255),
            Span::raw("   "),
        ]),
        Line::from(vec![
            Span::raw("  "),
            fg_bg("▖", 220, 208),
            fg_bg("▘", 220, 208),
            fg_bg("▘", 208, 250),
            fg("▘", 244),
            fg("▜", 255),
            fg("▖", 255),
            Span::raw("  "),
        ]),
    ]
}

pub fn path_line_spans(info: &LaunchInfo) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(git_segment_spans(info));
    spans
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

fn fg(text: &'static str, color: u8) -> Span<'static> {
    Span::styled(text, Style::default().fg(Color::Indexed(color)))
}

fn fg_bg(text: &'static str, fg_color: u8, bg_color: u8) -> Span<'static> {
    Span::styled(
        text,
        Style::default()
            .fg(Color::Indexed(fg_color))
            .bg(Color::Indexed(bg_color)),
    )
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
