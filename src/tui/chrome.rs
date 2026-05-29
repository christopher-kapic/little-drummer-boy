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
        // Pill-shaped badge: `▐ branch counts ▌` where the edge
        // glyphs (▐ ▌) are yellow-on-terminal-default and the body is
        // black-on-yellow. The half-block edges produce a "rounded"
        // visual without needing Nerd Fonts (which a true Powerline
        // semicircle would require).
        let badge = Style::default().fg(Color::Black).bg(Color::Indexed(220));
        let edge = Style::default().fg(Color::Indexed(220));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("▐", edge));
        spans.push(Span::styled(format!(" {} ", repo.branch), badge));
        let counts = repo_counts(repo);
        if !counts.is_empty() {
            spans.push(Span::styled(format!("{counts} "), badge));
        }
        spans.push(Span::styled("▌", edge));
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

/// Transient async-jobs strip (GOALS §22). Rendered **only** when ≥1 job
/// is active — additive to the fixed chrome, never a permanent slot. Each
/// job gets a glyph by kind: `⟳` loop, `⏲` timer, `⤓` background. The
/// caller passes `(job_id, kind, label, iteration)` tuples; this returns
/// the spans to append to the bottom-left status line, prefixed with a
/// separator. Returns an empty vec when there are no jobs.
pub fn jobs_strip_spans(jobs: &[(String, String, u64)]) -> Vec<Span<'static>> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let active = Style::default().fg(Color::Cyan);
    let mut spans: Vec<Span<'static>> = vec![Span::styled("  ".to_string(), muted)];
    for (i, (kind, label, iteration)) in jobs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ".to_string(), muted));
        }
        let glyph = match kind.as_str() {
            "timer" => "⏲",
            "background" => "⤓",
            _ => "⟳",
        };
        let detail = if kind == "background" {
            label.clone()
        } else {
            format!("{label} {iteration}")
        };
        spans.push(Span::styled(format!("{glyph} {detail}"), active));
    }
    spans
}

/// Persistent caffeination indicator (`/caffeinate`, GOALS §1a). Rendered
/// **only** while caffeination is active — additive to the fixed chrome,
/// never a permanent slot. Driven by the daemon-broadcast state so the
/// glyph appears (and clears) on every connected client in lockstep.
/// Returns the spans to prepend to the right-hand status line (`☕` plus a
/// trailing space separating it from the cwd), or an empty vec when off.
pub fn caffeinate_glyph_spans(active: bool) -> Vec<Span<'static>> {
    if !active {
        return Vec::new();
    }
    // Cyan reads as "kept awake" without competing with the yellow branch
    // badge; the trailing space keeps it off the cwd text.
    vec![Span::styled(
        "☕ ".to_string(),
        Style::default().fg(Color::Cyan),
    )]
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
