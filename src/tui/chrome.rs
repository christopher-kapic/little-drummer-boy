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

/// Bottom-left status: `provider/model · agent`.
///
///   - The model glyph is dark yellow when the active model is
///     marked favorite, light grey otherwise.
///   - Agents will eventually each carry a color; the default is blue.
pub fn left_status_spans(info: &LaunchInfo) -> Vec<Span<'static>> {
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let mut spans: Vec<Span<'static>> = Vec::new();

    if let Some((provider, model)) = &info.active_model {
        let model_style = if info.active_model_is_favorite {
            // Dark yellow / amber (xterm 220 is the bright shade we use
            // for the branch badge — 178 reads as "dark yellow" alongside
            // the light grey).
            Style::default().fg(Color::Indexed(178))
        } else {
            muted
        };
        spans.push(Span::styled(format!("{provider}/{model}"), model_style));
        spans.push(Span::styled(" · ".to_string(), muted));
    }

    // Default agent color is blue. Per-agent overrides can replace this
    // when agent definitions grow a `color:` field.
    spans.push(Span::styled(
        info.agent_name.clone(),
        Style::default().fg(Color::Blue),
    ));
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
