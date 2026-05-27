//! Startup splash shown when launching the interactive TUI via bare
//! `cockpit`.
//!
//! Only the raw-stdout splash and the `LaunchInfo` struct that the TUI
//! reads on boot live here. Config-directory discovery lives in
//! `config::dirs`; provider/model detection lives in `config::provider`;
//! the ratatui-side chrome lives in `tui::chrome`.

use std::env;
use std::path::{Path, PathBuf};

use crate::config::provider::detect_provider_model;
use crate::git::{self, RepoStatus};
use crate::tui::chrome::repo_counts;
use crate::tui::composer::INPUT_PREFIX;

pub const APP_NAME: &str = "Cockpit CLI";
pub const DEFAULT_AGENT: &str = "orchestrator-build";

const P51_ANSI_LINES: [&str; 4] = [
    "    \x1b[38;5;255m█\x1b[0m   \x1b[38;5;196;48;5;16m▖\x1b[0m\x1b[38;5;250;48;5;244m▖\x1b[0m",
    "    \x1b[38;5;255m▐\x1b[0m\x1b[38;5;255;48;5;250m▌\x1b[0m\x1b[38;5;250m▄\x1b[0m\x1b[38;5;250;48;5;244m▛\x1b[0m\x1b[38;5;250;48;5;244m▘\x1b[0m ",
    "    \x1b[38;5;33;48;5;45m▖\x1b[0m\x1b[38;5;250;48;5;255m▛\x1b[0m\x1b[38;5;250;48;5;255m▀\x1b[0m   ",
    "  \x1b[38;5;220;48;5;208m▖\x1b[0m\x1b[38;5;220;48;5;208m▘\x1b[0m\x1b[38;5;208;48;5;250m▘\x1b[0m\x1b[38;5;244m▘\x1b[0m\x1b[38;5;255m▜\x1b[0m\x1b[38;5;255m▖\x1b[0m  ",
];
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const GREY: &str = "\x1b[38;5;250m";
const BRANCH_BADGE: &str = "\x1b[30;48;5;220m";
/// Right half-block (▐) in yellow-220 foreground on terminal default.
/// Painted as the left edge of the branch pill so the badge fades from
/// the surrounding terminal background instead of slamming into it.
const BADGE_LEFT_EDGE: &str = "\x1b[38;5;220m▐\x1b[0m";
/// Left half-block (▌) in yellow-220 foreground — right edge of the
/// pill, same fade behavior as `BADGE_LEFT_EDGE`.
const BADGE_RIGHT_EDGE: &str = "\x1b[38;5;220m▌\x1b[0m";

#[derive(Debug, Clone)]
pub struct LaunchInfo {
    pub version: &'static str,
    pub provider_line: String,
    /// Currently selected (provider_id, model_id). None when nothing
    /// has been picked yet.
    pub active_model: Option<(String, String)>,
    /// True when the active model has `favorite: true` in config.
    pub active_model_is_favorite: bool,
    /// Max context window of the active model, in tokens, when the
    /// config carries it. Drives the `(max Nk)` part of the chrome's
    /// context indicator.
    pub active_model_max_context: Option<u32>,
    pub cwd: PathBuf,
    pub cwd_display: String,
    pub repo_status: Option<RepoStatus>,
    pub agent_name: String,
    /// User's configured display name from `extended-config.json`.
    /// When `Some`, the splash renders `Welcome, {name}` between the
    /// title and provider lines.
    pub user_name: Option<String>,
    /// Whether the pixel-banner splash (GOALS §1g) is enabled. Read
    /// from `tui.banner.enabled` in `extended-config.json`. Even when
    /// `true`, the banner suppresses itself on `NO_COLOR`, non-TTY
    /// stdout, narrow terminals, or `COCKPIT_ROOSTER=1`.
    pub banner_enabled: bool,
}

pub fn load(project: Option<&Path>) -> LaunchInfo {
    let cwd = resolve_launch_dir(project);
    let active_model = detect_provider_model(&cwd);
    let provider_line = active_model
        .clone()
        .map(|(provider, model)| format!("{provider} / {model}"))
        .unwrap_or_else(|| "No providers configured - run /settings to edit".to_string());
    let active_model_is_favorite = active_model
        .as_ref()
        .map(|(p, m)| is_favorite_model(&cwd, p, m))
        .unwrap_or(false);
    let active_model_max_context = active_model
        .as_ref()
        .and_then(|(p, m)| lookup_model_context(&cwd, p, m));
    let repo_status = git::repo_status(&cwd).ok().flatten();
    let user_name = load_user_name(&cwd);
    let banner_enabled = load_banner_enabled(&cwd);

    LaunchInfo {
        version: env!("CARGO_PKG_VERSION"),
        provider_line,
        active_model,
        active_model_is_favorite,
        active_model_max_context,
        cwd_display: display_path(&cwd),
        cwd,
        repo_status,
        agent_name: DEFAULT_AGENT.to_string(),
        user_name,
        banner_enabled,
    }
}

/// Resolve the effective `tui.banner.enabled` from layered config.
/// Defaults to `true` when no layer specifies it.
fn load_banner_enabled(cwd: &Path) -> bool {
    use crate::config::dirs::discover_config_dirs;
    use crate::config::extended::ExtendedConfigDoc;
    for dir in discover_config_dirs(cwd) {
        let path = dir.path.join("extended-config.json");
        if path.exists()
            && let Ok(doc) = ExtendedConfigDoc::load(&path)
        {
            return doc.config().tui.banner.enabled;
        }
    }
    true
}

/// Walk the layered-config discovery and return the `name` field from
/// the first `extended-config.json` we find with one set. `None` falls
/// through to the splash omitting the welcome line.
fn load_user_name(cwd: &Path) -> Option<String> {
    use crate::config::dirs::discover_config_dirs;
    use crate::config::extended::ExtendedConfigDoc;
    for dir in discover_config_dirs(cwd) {
        let path = dir.path.join("extended-config.json");
        if let Ok(doc) = ExtendedConfigDoc::load(&path) {
            let cfg = doc.config();
            if let Some(name) = cfg.name.as_deref()
                && !name.trim().is_empty()
            {
                return Some(name.trim().to_string());
            }
        }
    }
    None
}

/// Walk the discovered config layers looking for the active model's
/// `context_length`. Returns `None` if it isn't recorded — the chrome
/// then omits the `(max Nk)` suffix.
fn lookup_model_context(cwd: &Path, provider_id: &str, model_id: &str) -> Option<u32> {
    use crate::config::dirs::discover_config_dirs;
    use crate::config::providers::ConfigDoc;
    for dir in discover_config_dirs(cwd) {
        let path = dir.path.join("config.json");
        let Ok(doc) = ConfigDoc::load(&path) else {
            continue;
        };
        let cfg = doc.providers();
        if let Some(entry) = cfg.providers.get(provider_id)
            && let Some(model) = entry.models.iter().find(|m| m.id == model_id)
            && let Some(n) = model.context_length
        {
            return Some(n);
        }
    }
    None
}

/// Look up `<provider>/<model>` in the first config.json on the
/// discovered config path and return whether it carries
/// `favorite: true`. Returns false on any error (missing file,
/// missing provider, etc.).
fn is_favorite_model(cwd: &Path, provider_id: &str, model_id: &str) -> bool {
    use crate::config::dirs::discover_config_dirs;
    use crate::config::providers::ConfigDoc;
    for dir in discover_config_dirs(cwd) {
        let path = dir.path.join("config.json");
        let Ok(doc) = ConfigDoc::load(&path) else {
            continue;
        };
        let cfg = doc.providers();
        if let Some(entry) = cfg.providers.get(provider_id)
            && let Some(model) = entry.models.iter().find(|m| m.id == model_id)
        {
            return model.favorite;
        }
    }
    false
}

pub fn print(project: Option<&Path>) {
    let info = load(project);
    print_header(&info);
    println!();
    println!("{INPUT_PREFIX}");
    println!("{}", info.agent_name);
}

/// The 4-line launch header as ANSI-styled strings (logo + title,
/// logo + provider, logo + path, logo bottom). Shared by `print_header`
/// (startup, raw `println!`) and the TUI's `/new` path (mid-session,
/// piped through `insert_above_viewport`).
///
/// Spacing: P51 art renders 10 columns wide; the 3-space separator lines
/// content up at column 13, matching the TUI's 11-wide icon column +
/// 2-space text indent.
pub fn header_lines(info: &LaunchInfo) -> Vec<String> {
    let title = format!("{BOLD}{APP_NAME}{RESET} {GREY}v{}{RESET}", info.version);
    match info.user_name.as_deref() {
        Some(name) if !name.is_empty() => {
            // Shift the existing content down by one row so the welcome
            // line slots in between the title and the provider line.
            // All four logo rows carry text; the trailing empty-art row
            // is sacrificed since the logo is only four lines tall.
            vec![
                format!("{}   {}", P51_ANSI_LINES[0], title),
                format!("{}   {GREY}Welcome, {BOLD}{name}{RESET}", P51_ANSI_LINES[1]),
                format!(
                    "{}   {GREY}{}{RESET}",
                    P51_ANSI_LINES[2], info.provider_line
                ),
                format!("{}   {}", P51_ANSI_LINES[3], path_line_ansi(info)),
            ]
        }
        _ => vec![
            format!("{}   {}", P51_ANSI_LINES[0], title),
            format!(
                "{}   {GREY}{}{RESET}",
                P51_ANSI_LINES[1], info.provider_line
            ),
            format!("{}   {}", P51_ANSI_LINES[2], path_line_ansi(info)),
            P51_ANSI_LINES[3].to_string(),
        ],
    }
}

/// Print just the launch header. Used by the TUI at startup so the
/// header lands in normal terminal output — it scrolls naturally with
/// the chat and ends up in scrollback once enough messages arrive.
///
/// When the §1g banner is enabled and the terminal supports it, the
/// banner renders above the standard header block.
pub fn print_header(info: &LaunchInfo) {
    if let Some(lines) = crate::banner::render_lines(info.banner_enabled) {
        for line in lines {
            println!("{line}");
        }
        println!();
    }
    for line in header_lines(info) {
        println!("{line}");
    }
}

fn resolve_launch_dir(project: Option<&Path>) -> PathBuf {
    let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    match project {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => base.join(path),
        None => base,
    }
}

pub fn display_path(path: &Path) -> String {
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
        line.push_str(BADGE_LEFT_EDGE);
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
        line.push_str(BADGE_RIGHT_EDGE);
    }
    line
}
