//! Diff rendering for `edit` / `editunlock` tool calls.
//!
//! Three modes (config `tui.diff_style`):
//!
//! - [`DiffStyle::SideBySide`] — old on the left, new on the right.
//!   Degrades to [`DiffStyle::Inline`] when the terminal is narrower
//!   than [`SIDE_BY_SIDE_MIN_WIDTH`].
//! - [`DiffStyle::Inline`] — unified diff. Removed lines prefixed
//!   `-` in red; added lines prefixed `+` in green; context lines
//!   prefixed ` `.
//! - [`DiffStyle::Hidden`] — one-line summary
//!   (`edited <path> (+N −M)`).
//!
//! Diffing is line-granular via [`similar::TextDiff::from_lines`].
//! Context lines outside hunks are emitted with a `…` separator so
//! large unchanged regions don't drown out the meaningful changes
//! (the limit is [`CONTEXT_LINES`]).
//!
//! `write` / `writeunlock` diffs are deferred — the tool doesn't
//! currently surface the pre-write file content to the TUI. See
//! `flagged-for-christopher.md`.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

use crate::config::extended::DiffStyle;

/// Minimum terminal width (in columns) for [`DiffStyle::SideBySide`].
/// Below this, [`render_diff`] falls back to [`DiffStyle::Inline`].
pub const SIDE_BY_SIDE_MIN_WIDTH: u16 = 80;

/// Context lines kept on either side of an edit hunk (matches the
/// default for `git diff -U`). Anything past that is collapsed into a
/// single `…` separator line.
const CONTEXT_LINES: usize = 3;

const COL_REMOVED: Color = Color::Red;
const COL_ADDED: Color = Color::Green;
const COL_HEADER: Color = Color::Cyan;
const COL_SEP: Color = Color::Indexed(244);
const COL_ELLIPSIS: Color = Color::Indexed(244);

/// Inline render mode prefixes (one column per character).
const PREFIX_REM: &str = "- ";
const PREFIX_ADD: &str = "+ ";
const PREFIX_CTX: &str = "  ";

/// Side-by-side separator. Spaces on either side absorb the column
/// gap so individual lines line up cleanly.
const COL_SEPARATOR: &str = " │ ";
/// Left indent applied to every diff line, matching the tool-output
/// indent the existing `Plain` history entries use.
const LEFT_INDENT: &str = "  ";

/// Render an `edit` / `editunlock` tool call as a diff.
///
/// `width` is the chat-pane width in terminal columns; the side-by-side
/// renderer uses it to size the two columns. `path` is the edited
/// file's path (displayed in the header).
pub fn render_diff(
    tool: &str,
    path: &str,
    old: &str,
    new: &str,
    style: DiffStyle,
    width: u16,
) -> Vec<Line<'static>> {
    let diff = TextDiff::from_lines(old, new);
    let (added, removed) = count_changes(&diff);

    match style {
        DiffStyle::Hidden => vec![summary_line(tool, path, added, removed)],
        DiffStyle::Inline => {
            let mut out = vec![header_line(tool, path, added, removed)];
            out.extend(render_inline(&diff));
            out
        }
        DiffStyle::SideBySide if width >= SIDE_BY_SIDE_MIN_WIDTH => {
            let mut out = vec![header_line(tool, path, added, removed)];
            out.extend(render_side_by_side(&diff, width));
            out
        }
        DiffStyle::SideBySide => {
            // Degrade to inline at narrow widths. Two-column layout
            // with anything less than ~30 cells per side is unreadable.
            let mut out = vec![header_line(tool, path, added, removed)];
            out.extend(render_inline(&diff));
            out
        }
    }
}

fn header_line(tool: &str, path: &str, added: usize, removed: usize) -> Line<'static> {
    Line::from(vec![
        Span::raw(LEFT_INDENT.to_string()),
        Span::styled("✓ ", Style::default().fg(COL_HEADER)),
        Span::styled(format!("{tool}: "), Style::default().fg(COL_HEADER)),
        Span::raw(path.to_string()),
        Span::raw(" "),
        Span::styled(
            format!("(+{added} −{removed})"),
            Style::default().fg(COL_SEP),
        ),
    ])
}

fn summary_line(tool: &str, path: &str, added: usize, removed: usize) -> Line<'static> {
    Line::from(vec![
        Span::raw(LEFT_INDENT.to_string()),
        Span::styled("✓ ", Style::default().fg(COL_HEADER)),
        Span::raw(format!("{tool}: {path} ")),
        Span::styled(
            format!("(+{added} −{removed})"),
            Style::default().fg(COL_SEP),
        ),
    ])
}

fn count_changes<'a>(diff: &TextDiff<'a, 'a, str>) -> (usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

// ---- inline ---------------------------------------------------------------

fn render_inline<'a>(diff: &TextDiff<'a, 'a, str>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for group in diff.grouped_ops(CONTEXT_LINES) {
        if !out.is_empty() {
            out.push(ellipsis_line());
        }
        for op in group {
            for change in diff.iter_changes(&op) {
                let value = strip_trailing_newline(change.value());
                let (prefix, style) = match change.tag() {
                    ChangeTag::Delete => (PREFIX_REM, Style::default().fg(COL_REMOVED)),
                    ChangeTag::Insert => (PREFIX_ADD, Style::default().fg(COL_ADDED)),
                    ChangeTag::Equal => (PREFIX_CTX, Style::default()),
                };
                out.push(Line::from(vec![
                    Span::raw(LEFT_INDENT.to_string()),
                    Span::styled(prefix.to_string(), style),
                    Span::styled(value.to_string(), style),
                ]));
            }
        }
    }
    out
}

// ---- side-by-side ---------------------------------------------------------

fn render_side_by_side<'a>(diff: &TextDiff<'a, 'a, str>, width: u16) -> Vec<Line<'static>> {
    let col_width = side_by_side_column_width(width);
    let mut out = Vec::new();

    for group in diff.grouped_ops(CONTEXT_LINES) {
        if !out.is_empty() {
            out.push(ellipsis_line());
        }
        // Within each group we re-pair removed/added lines: a 3-line
        // delete followed by a 3-line insert renders as three rows of
        // (red, green) instead of three rows of (red, blank) then
        // three rows of (blank, green). That's what `git diff
        // --color-words`'s line variant would do, and it matches what
        // people expect "side by side" to mean.
        let mut left_pending: Vec<String> = Vec::new();
        let mut right_pending: Vec<String> = Vec::new();
        for op in group {
            for change in diff.iter_changes(&op) {
                let value = strip_trailing_newline(change.value()).to_string();
                match change.tag() {
                    ChangeTag::Delete => left_pending.push(value),
                    ChangeTag::Insert => right_pending.push(value),
                    ChangeTag::Equal => {
                        flush_pair(&mut left_pending, &mut right_pending, col_width, &mut out);
                        // Equal lines mirror across both columns.
                        let l = pad_to_width(&value, col_width);
                        let r = pad_to_width(&value, col_width);
                        out.push(side_by_side_row(l, None, r, None));
                    }
                }
            }
        }
        flush_pair(&mut left_pending, &mut right_pending, col_width, &mut out);
    }
    out
}

fn flush_pair(
    left: &mut Vec<String>,
    right: &mut Vec<String>,
    col_width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let n = left.len().max(right.len());
    for i in 0..n {
        let l = left.get(i).cloned().unwrap_or_default();
        let r = right.get(i).cloned().unwrap_or_default();
        let l_text = pad_to_width(&l, col_width);
        let r_text = pad_to_width(&r, col_width);
        let l_style = if left.get(i).is_some() {
            Some(Style::default().fg(COL_REMOVED))
        } else {
            None
        };
        let r_style = if right.get(i).is_some() {
            Some(Style::default().fg(COL_ADDED))
        } else {
            None
        };
        out.push(side_by_side_row(l_text, l_style, r_text, r_style));
    }
    left.clear();
    right.clear();
}

fn side_by_side_row(
    left: String,
    left_style: Option<Style>,
    right: String,
    right_style: Option<Style>,
) -> Line<'static> {
    Line::from(vec![
        Span::raw(LEFT_INDENT.to_string()),
        Span::styled(left, left_style.unwrap_or_default()),
        Span::styled(COL_SEPARATOR.to_string(), Style::default().fg(COL_SEP)),
        Span::styled(right, right_style.unwrap_or_default()),
    ])
}

/// How many cells of usable text fit in each diff column. Subtract:
/// LEFT_INDENT (2), the COL_SEPARATOR (3), and floor-divide the rest
/// by 2. Falls back to a tiny minimum so an absurdly narrow terminal
/// still produces *something* instead of a panic.
fn side_by_side_column_width(width: u16) -> usize {
    let usable = (width as usize)
        .saturating_sub(LEFT_INDENT.chars().count())
        .saturating_sub(COL_SEPARATOR.chars().count());
    (usable / 2).max(4)
}

fn pad_to_width(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len > width {
        // Truncate with an ellipsis. Chars are 1-cell here (assumed);
        // wide-grapheme handling can come later if it becomes a real
        // issue in practice.
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        let mut out = s.to_string();
        for _ in 0..(width - len) {
            out.push(' ');
        }
        out
    }
}

fn ellipsis_line() -> Line<'static> {
    Line::from(vec![
        Span::raw(LEFT_INDENT.to_string()),
        Span::styled(
            "…",
            Style::default()
                .fg(COL_ELLIPSIS)
                .add_modifier(Modifier::DIM),
        ),
    ])
}

fn strip_trailing_newline(s: &str) -> &str {
    s.strip_suffix('\n').unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_to_strings(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn hidden_returns_one_line() {
        let lines = render_diff(
            "edit",
            "src/foo.rs",
            "a\nb\nc\n",
            "a\nB\nc\n",
            DiffStyle::Hidden,
            120,
        );
        assert_eq!(lines.len(), 1);
        let s = &lines_to_strings(&lines)[0];
        assert!(s.contains("src/foo.rs"), "{s:?}");
        assert!(s.contains("(+1 −1)"), "{s:?}");
    }

    #[test]
    fn inline_renders_with_plus_minus_prefixes() {
        let lines = render_diff(
            "edit",
            "src/foo.rs",
            "alpha\nbeta\ngamma\n",
            "alpha\nBETA\ngamma\n",
            DiffStyle::Inline,
            120,
        );
        let rendered = lines_to_strings(&lines);
        assert!(rendered[0].contains("(+1 −1)"));
        let body = rendered[1..].join("\n");
        assert!(body.contains("- beta"));
        assert!(body.contains("+ BETA"));
        assert!(body.contains("  alpha"));
    }

    #[test]
    fn side_by_side_falls_back_to_inline_when_narrow() {
        let narrow = render_diff(
            "edit",
            "x.rs",
            "a\nb\n",
            "a\nB\n",
            DiffStyle::SideBySide,
            40,
        );
        // Narrow mode should look like the inline render (uses `- ` /
        // `+ ` prefixes rather than the side-by-side `│` separator).
        let rendered = lines_to_strings(&narrow).join("\n");
        assert!(rendered.contains("- b"));
        assert!(rendered.contains("+ B"));
        assert!(!rendered.contains(COL_SEPARATOR));
    }

    #[test]
    fn side_by_side_uses_separator_when_wide() {
        let wide = render_diff(
            "edit",
            "x.rs",
            "alpha\nbeta\n",
            "alpha\nBETA\n",
            DiffStyle::SideBySide,
            120,
        );
        let rendered = lines_to_strings(&wide).join("\n");
        // Header doesn't carry the column separator; body rows do.
        assert!(rendered.contains(COL_SEPARATOR));
    }

    #[test]
    fn pad_to_width_truncates_with_ellipsis() {
        assert_eq!(pad_to_width("abcdef", 4), "abc…");
    }

    #[test]
    fn pad_to_width_pads_short_strings() {
        assert_eq!(pad_to_width("ab", 5), "ab   ");
    }

    #[test]
    fn count_changes_matches_visible_summary() {
        let diff = TextDiff::from_lines("a\nb\nc\n", "a\nB\nC\n");
        let (added, removed) = count_changes(&diff);
        assert_eq!(added, 2);
        assert_eq!(removed, 2);
    }
}
