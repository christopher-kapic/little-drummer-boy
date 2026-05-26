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

#[derive(Debug, Clone)]
pub struct LaunchInfo {
    pub version: &'static str,
    pub provider_line: String,
    /// Currently selected (provider_id, model_id). None when nothing
    /// has been picked yet.
    pub active_model: Option<(String, String)>,
    /// True when the active model has `favorite: true` in config.
    pub active_model_is_favorite: bool,
    pub cwd: PathBuf,
    pub cwd_display: String,
    pub repo_status: Option<RepoStatus>,
    pub agent_name: String,
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
    let repo_status = git::repo_status(&cwd).ok().flatten();

    LaunchInfo {
        version: env!("CARGO_PKG_VERSION"),
        provider_line,
        active_model,
        active_model_is_favorite,
        cwd_display: display_path(&cwd),
        cwd,
        repo_status,
        agent_name: DEFAULT_AGENT.to_string(),
    }
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
    println!(
        "{}   {GREY}{}{RESET}",
        P51_ANSI_LINES[1], info.provider_line
    );
    println!("{}   {}", P51_ANSI_LINES[2], path_line_ansi(info));
    println!("{}", P51_ANSI_LINES[3]);
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
