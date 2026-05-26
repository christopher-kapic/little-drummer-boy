//! TUI status line / chrome.
//!
//! Per `GOALS.md` §1a, the chrome **always** shows:
//!   - The current working directory (abbreviated if it overflows).
//!   - The git branch (with a leading `` glyph) when the cwd is in a
//!     git repo. When not in a repo, no slot — no placeholder text.
//!
//! Other slots (active agent, model, token count, …) compose around
//! these two.

use ratatui::style::{Color, Style};
use ratatui::text::Span;

use crate::git::RepoStatus;
use crate::tui::theme::MUTED_COLOR_INDEX;
use crate::welcome::LaunchInfo;

pub fn status_line_spans(info: &LaunchInfo) -> Vec<Span<'static>> {
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

pub fn repo_counts(repo: &RepoStatus) -> String {
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
