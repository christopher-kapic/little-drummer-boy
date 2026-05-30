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

/// Side-conversation indicator (`/side`, GOALS §1a). Rendered **only**
/// while a throwaway side conversation is open — additive to the fixed
/// chrome (cwd + branch), never displacing a slot, the same pattern as the
/// `☕` caffeinate glyph. Magenta reads as "you're somewhere temporary"
/// without competing with the yellow branch badge; the trailing space keeps
/// it off the cwd text. Returns an empty vec in the main session.
pub fn side_glyph_spans(active: bool) -> Vec<Span<'static>> {
    if !active {
        return Vec::new();
    }
    vec![Span::styled(
        "⑃ side ".to_string(),
        Style::default().fg(Color::Magenta),
    )]
}

/// Plan-yellow (`#f8d749`) used by the plan-status chrome slot. Distinct from
/// the branch pill's xterm-220 *filled* badge: this slot is unfilled colored
/// glyph+number text, so hue alone never has to disambiguate the two.
const PLAN_YELLOW: Color = Color::Rgb(0xf8, 0xd7, 0x49);

/// Additive plan-status indicator (`plan-status-chrome-and-resolver.md`).
/// Rendered **only** when this project has something unfinished — additive to
/// the fixed chrome (cwd + branch + context + active agent, GOALS §1a), never
/// displacing a slot, the same pattern as the `☕` caffeinate glyph. Up to
/// three segments, each omitted when its count is zero; an all-zero state
/// returns an empty vec so a normal coding session stays uncluttered.
///
///   - **ready** `⧖N` — queued (`Pending`) plans.
///   - **in-progress** `▶N` — the executing plan (≤1 per project).
///   - **interruptions** `?N` — open `needs_attention` items blocking
///     progress; the actionable, attention-grabbing segment (rendered last so
///     it reads as the thing to act on, and bold to stand out).
///
/// Driven by daemon-broadcast state, so a reconnecting / late-opened TUI shows
/// the correct counts. Returns the spans to prepend to the right-hand status
/// line (a trailing space separates the slot from what follows), or an empty
/// vec when nothing is unfinished.
pub fn plan_status_spans(counts: crate::db::plans::PlanStatusCounts) -> Vec<Span<'static>> {
    if counts.is_empty() {
        return Vec::new();
    }
    let plain = Style::default().fg(PLAN_YELLOW);
    let actionable = Style::default()
        .fg(PLAN_YELLOW)
        .add_modifier(ratatui::style::Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let push = |text: String, style: Style, spans: &mut Vec<Span<'static>>| {
        if !spans.is_empty() {
            spans.push(Span::styled(" ".to_string(), plain));
        }
        spans.push(Span::styled(text, style));
    };
    if counts.ready > 0 {
        push(format!("⧖{}", counts.ready), plain, &mut spans);
    }
    if counts.in_progress > 0 {
        push(format!("▶{}", counts.in_progress), plain, &mut spans);
    }
    if counts.interruptions > 0 {
        push(format!("?{}", counts.interruptions), actionable, &mut spans);
    }
    spans.push(Span::styled(" ".to_string(), plain));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_status_absent_when_all_zero_and_omits_zero_segments() {
        use crate::db::plans::PlanStatusCounts;
        // Nothing unfinished → the whole slot is absent.
        assert!(plan_status_spans(PlanStatusCounts::default()).is_empty());

        // Only in-progress + interruptions: the ready segment is omitted, and
        // the interruptions glyph is present (bold/actionable).
        let counts = PlanStatusCounts {
            ready: 0,
            in_progress: 1,
            interruptions: 2,
        };
        let text: String = plan_status_spans(counts)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("▶1"), "in-progress segment: {text}");
        assert!(text.contains("?2"), "interruptions segment: {text}");
        assert!(!text.contains("⧖"), "zero ready segment omitted: {text}");
    }

    #[test]
    fn plan_status_uses_plan_yellow_unfilled() {
        use crate::db::plans::PlanStatusCounts;
        let spans = plan_status_spans(PlanStatusCounts {
            ready: 3,
            in_progress: 0,
            interruptions: 0,
        });
        // Plan-yellow foreground, no background fill (distinct from the
        // branch pill's filled badge).
        let glyph = spans
            .iter()
            .find(|s| s.content.contains("⧖"))
            .expect("ready segment present");
        assert_eq!(glyph.style.fg, Some(PLAN_YELLOW));
        assert_eq!(glyph.style.bg, None, "unfilled — no background");
    }

    #[test]
    fn side_glyph_present_only_when_active() {
        // Off in the main session; an additive indicator while a `/side`
        // side conversation is open (never a permanent slot).
        assert!(side_glyph_spans(false).is_empty());
        let spans = side_glyph_spans(true);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].content.contains("side"));
    }
}
