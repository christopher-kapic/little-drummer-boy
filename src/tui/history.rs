//! Typed entries that live in `App.history` plus the renderers that
//! turn them into `ratatui::text::Line` for display.
//!
//! Why a typed model rather than `Vec<String>`: the chrome needs to
//! style entries differently (user messages get bg color + padding,
//! thinking blocks get a "Thinking…" placeholder with a chip,
//! timestamps land right-aligned on the first wrapped line, …). All of
//! that needs structured data; a flat `Vec<String>` would force string
//! parsing tricks at render time.

use std::time::Duration;

use chrono::{DateTime, Local};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::config::extended::ThinkingDisplay;
use crate::tui::markdown;

/// Markdown render preferences, threaded from `App` to each
/// per-entry renderer. Cheap to copy, so we pass by value.
#[derive(Debug, Clone, Copy, Default)]
pub struct MarkdownOpts {
    pub agent: bool,
    pub user: bool,
}

/// The user's own message and the assistant's response carry
/// timestamps; engine events (tool calls, errors, subagent
/// spawn/report) don't — they're scoped within the surrounding
/// assistant turn so a per-event timestamp would clutter.
#[derive(Debug, Clone)]
pub enum HistoryEntry {
    User {
        text: String,
        timestamp: DateTime<Local>,
    },
    Plain {
        line: String,
    },
    /// Assistant turn with text. `reasoning` is captured but only
    /// rendered when `expanded` is true (see [`crate::tui::app`]).
    /// `think_duration` is the wall-clock time between
    /// `ThinkingStarted` and the first `AssistantTextDelta` — used to
    /// render `Agent thought for X seconds` once the turn finalizes.
    /// `None` when no reasoning content was captured.
    Agent {
        name: String,
        text: String,
        reasoning: String,
        timestamp: DateTime<Local>,
        expanded: bool,
        think_duration: Option<Duration>,
    },
    /// Completed `edit` / `editunlock` tool call. Rendered as a diff
    /// per `tui.diff_style` (side-by-side / inline / hidden). Stored
    /// instead of a `Plain` line so the renderer can re-flow if the
    /// pane width changes mid-session and re-pick side-by-side vs.
    /// inline.
    Diff {
        tool: String,
        path: String,
        old: String,
        new: String,
    },
    /// A run of consecutive boxable tool calls (read, readlock, unlock,
    /// bash, webfetch, …) rendered inside a light-grey rounded sidebar.
    /// Diff tools (edit/editunlock), write tools, and subagent calls
    /// break the run, so a box never holds them. The collapsed view
    /// shows at most [`TOOLBOX_VISIBLE`] calls with an internal scroll;
    /// a click expands it to show every call in full.
    ToolBox {
        calls: Vec<ToolCall>,
        /// Topmost visible call in collapsed mode. Ignored while
        /// `follow` or `expanded`.
        view_offset: usize,
        /// Collapsed viewport auto-pins to the newest call as calls
        /// stream in. Cleared when the user scrolls up; restored when
        /// they scroll back to the end.
        follow: bool,
        /// Click-expanded: render every call in full (input + output for
        /// output-bearing tools) and disable the internal scroll.
        expanded: bool,
    },
    /// A standalone tool call rendered as one styled line outside any
    /// box. Used for `write` / `writeunlock`: conceptually diffs that
    /// break the box, but the engine doesn't surface pre-write file
    /// content yet (see [`crate::tui::diff`]), so they render as a
    /// one-liner until that lands.
    ToolLine {
        call_id: String,
        tool: String,
        summary: String,
        state: ToolCallState,
    },
    /// A locally-run command and its captured (display-capped) output,
    /// shown in chat (GOALS §1k/§1l). `!` shell runs are local-only;
    /// `/git` runs also buffer a `<git>` block onto the next user
    /// message. Either way the displayed copy is **not** sent to the
    /// agent and `estimate_context_tokens` ignores it.
    LocalCommand {
        /// Display label, e.g. `! ls -la` or `/git status`.
        label: String,
        /// Captured, ANSI-stripped, display-capped output.
        output: String,
        /// True when the command exited non-zero — tints the label red.
        failed: bool,
    },
}

/// Lifecycle state of one tool call. Drives the line color: yellow
/// while the model waits, white on success, red when the tool failed,
/// bold red when the model built the call badly (unrecoverable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallState {
    /// The model is waiting on the tool — yellow.
    Processing,
    /// Completed successfully — white.
    Success,
    /// The tool ran but failed for an environmental reason — red.
    Failed,
    /// The model constructed the call badly; unrecoverable — bold red.
    BadCall,
}

/// One tool call inside a [`HistoryEntry::ToolBox`].
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub call_id: String,
    pub tool: String,
    /// One-line collapsed summary: a path, the first line of a bash
    /// command, a URL, … Truncated to the pane width at render time.
    pub summary: String,
    /// Full invocation text for the expanded view (e.g. a multi-line
    /// bash command). Equal to `summary` for single-line calls.
    pub full_input: String,
    /// Full tool output, shown only when the box is expanded *and* the
    /// tool is output-bearing. Empty for input-only tools.
    pub output: String,
    pub state: ToolCallState,
}

/// Max tool-call rows a collapsed [`HistoryEntry::ToolBox`] shows
/// before it scrolls internally.
pub const TOOLBOX_VISIBLE: usize = 6;

/// Light grey for the tool-box sidebar (xterm 256-color).
const SIDEBAR_FG: Color = Color::Indexed(244);
/// Dim grey for expanded tool output lines.
const TOOL_OUTPUT_FG: Color = Color::Indexed(245);

/// In-flight assistant turn. Lives in `App.pending` from
/// `ThinkingStarted` to `AssistantText`; once finalized it gets pushed
/// to `App.history` as [`HistoryEntry::Agent`].
#[derive(Debug, Clone)]
pub struct PendingMsg {
    pub name: String,
    /// Accumulated streaming text **with `<think>` blocks stripped**.
    /// Empty while we're still in the "Thinking…" phase.
    pub text: String,
    /// Accumulated reasoning content. Hidden by default; surfaced when
    /// the user expands the eventual history entry. Populated from
    /// both rig's `ReasoningDelta` events *and* inline `<think>…
    /// </think>` blocks in the text stream.
    pub reasoning: String,
    pub timestamp: DateTime<Local>,
    /// `Instant` the turn started — used for the `think_duration`
    /// stamp on the finalized [`HistoryEntry::Agent`]. Wall-clock
    /// `timestamp` above is for the right-aligned `HH:MM` chip.
    pub started_at: std::time::Instant,
    /// Set to `Some(_)` the first time a *non-thinking* text delta
    /// (i.e., text outside any `<think>` block) arrives. Until then
    /// the agent is considered "still thinking."
    pub text_started_at: Option<std::time::Instant>,
    /// True if we're currently inside a `<think>...</think>` block
    /// straddling delta boundaries.
    pub inside_think: bool,
    /// Buffered tail of the latest delta that *might* be the start of
    /// a `<think>` or `</think>` tag — held until the next delta lets
    /// us disambiguate.
    pub tag_partial: String,
}

const USER_BG: Color = Color::Indexed(17); // dark blue (xterm 256-color)
const USER_BORDER_FG: Color = Color::Indexed(33); // brighter blue for the rounded outline
const TIMESTAMP_FG: Color = Color::Indexed(250); // light grey (xterm 256-color)
const REASONING_FG: Color = Color::Indexed(244); // mid-grey, italicized
const THINKING_FG: Color = Color::Yellow;
/// Width of an `HH:MM` timestamp string.
pub const TIMESTAMP_WIDTH: usize = 5;

/// Deterministic color assignment for an agent's bullet point. The
/// bundled cast gets stable hand-picked hues; user-authored agents
/// get a hash-based pick from the same palette so a project's agents
/// stay visually distinct even when their names collide on a prefix.
pub fn agent_color(name: &str) -> Color {
    match name {
        "orchestrator-build" => Color::Cyan,
        "orchestrator-plan" => Color::Magenta,
        "coder" => Color::Green,
        "explore" => Color::Yellow,
        "docs" => Color::Blue,
        _ => {
            const PALETTE: &[Color] = &[
                Color::Cyan,
                Color::Magenta,
                Color::Green,
                Color::Yellow,
                Color::Red,
                Color::LightCyan,
                Color::LightMagenta,
                Color::LightGreen,
                Color::LightYellow,
                Color::LightRed,
            ];
            let h: u32 = name
                .bytes()
                .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
            PALETTE[(h as usize) % PALETTE.len()]
        }
    }
}

/// Outer gutter on either side of a user-message bubble (cells of
/// terminal-default bg outside the rounded box).
const USER_GUTTER: usize = 1;
/// Inner padding between the bubble's vertical border and the text.
const USER_INNER_PAD: usize = 1;

/// Agent messages render with no leading marker — the active-agent
/// indicator in the chrome and the thinking-chip (when present)
/// already signal who's talking, and the bullet was visual noise that
/// accumulated as the conversation grew. Kept as an empty constant so
/// callers don't sprinkle string literals.
const AGENT_BULLET: &str = "";

/// Left-side horizontal padding applied to every agent message line, so
/// the text doesn't sit flush against the terminal edge now that the
/// bullet is gone. Continuation lines inherit this indent; the first
/// line gets it too, with the timestamp reserve on the right side.
/// Public so the copy path can strip exactly this much from each
/// row of an agent-message selection.
pub const AGENT_INDENT: usize = 2;

/// One rendered history entry. The chrome assembles a flat list of
/// `Rendered` for the chat pane, then uses each entry's `chip_row` to
/// build a click-targeting map: a click on row N of the pane resolves
/// to whichever entry has `chip_row == Some(row_within_entry)`.
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// Index of the row within `lines` that is the clickable "thinking"
    /// chip. `None` for entries without one (everything except a
    /// `HistoryEntry::Agent` with non-empty reasoning).
    pub chip_row: Option<usize>,
    /// One bool per row in `lines`. `true` for rows that are a
    /// soft-wrap continuation of the prior logical line — the copy
    /// path joins these with a space instead of a newline so pasted
    /// agent text reconstructs the original paragraph rather than
    /// preserving the screen-level wraps.
    pub continuations: Vec<bool>,
}

/// Render one history entry. The renderer receives the area's `width`
/// so it can right-align timestamps and pad the user-message
/// background to the full width.
///
/// `thinking` controls how reasoning is surfaced:
/// - [`ThinkingDisplay::Condensed`] (default) — chip, expands on `Ctrl+J`
/// - [`ThinkingDisplay::Hidden`] — drop the chip and reasoning entirely
/// - [`ThinkingDisplay::Verbose`] — force expanded regardless of the stored flag
pub fn render_entry(
    entry: &HistoryEntry,
    width: u16,
    thinking: ThinkingDisplay,
    md: MarkdownOpts,
    diff_style: crate::config::extended::DiffStyle,
    emojis: bool,
) -> Rendered {
    match entry {
        HistoryEntry::User { text, timestamp } => {
            let lines = render_user(text, *timestamp, width, md.user);
            let continuations = vec![false; lines.len()];
            Rendered {
                lines,
                chip_row: None,
                continuations,
            }
        }
        HistoryEntry::Plain { line } => Rendered {
            lines: vec![Line::from(line.clone())],
            chip_row: None,
            continuations: vec![false],
        },
        HistoryEntry::Diff {
            tool,
            path,
            old,
            new,
        } => {
            let lines =
                crate::tui::diff::render_diff(tool, path, old, new, diff_style, width, emojis);
            let continuations = vec![false; lines.len()];
            Rendered {
                lines,
                chip_row: None,
                continuations,
            }
        }
        HistoryEntry::ToolBox {
            calls,
            view_offset,
            follow,
            expanded,
        } => render_toolbox(calls, *view_offset, *follow, *expanded, width, emojis),
        HistoryEntry::ToolLine {
            tool,
            summary,
            state,
            ..
        } => {
            // Standalone styled one-liner, indented to align with box
            // content (the box's sidebar+space is 2 cells wide).
            let avail = tool_summary_budget(tool, width as usize, 2, emojis);
            let mut spans = vec![Span::raw("  ".to_string())];
            spans.extend(tool_call_spans(
                tool,
                &truncate(summary, avail),
                *state,
                emojis,
            ));
            Rendered {
                lines: vec![Line::from(spans)],
                chip_row: None,
                continuations: vec![false],
            }
        }
        HistoryEntry::LocalCommand {
            label,
            output,
            failed,
        } => {
            let label_color = if *failed { Color::Red } else { Color::Cyan };
            let mut lines: Vec<Line<'static>> = Vec::new();
            lines.push(Line::from(vec![Span::styled(
                label.clone(),
                Style::default()
                    .fg(label_color)
                    .add_modifier(Modifier::BOLD),
            )]));
            for raw in output.lines() {
                lines.push(Line::from(vec![
                    Span::raw("  ".to_string()),
                    Span::styled(raw.to_string(), Style::default().fg(TOOL_OUTPUT_FG)),
                ]));
            }
            let continuations = vec![false; lines.len()];
            Rendered {
                lines,
                chip_row: None,
                continuations,
            }
        }
        HistoryEntry::Agent {
            name,
            text,
            reasoning,
            timestamp,
            expanded,
            think_duration,
        } => {
            let effective_reasoning: &str = match thinking {
                ThinkingDisplay::Hidden => "",
                ThinkingDisplay::Condensed | ThinkingDisplay::Verbose => reasoning,
            };
            let effective_expanded = match thinking {
                ThinkingDisplay::Verbose => true,
                ThinkingDisplay::Condensed => *expanded,
                ThinkingDisplay::Hidden => false,
            };
            render_agent(
                name,
                text,
                effective_reasoning,
                *timestamp,
                effective_expanded,
                *think_duration,
                width,
                md.agent,
            )
        }
    }
}

/// Render an in-flight pending message. Text is whatever's accumulated
/// so far; if empty we render `Thinking <dots>` with `dots` driven by
/// the animation phase. Reasoning is captured but not displayed live
/// (the user can expand once the turn finalizes).
pub fn render_pending(msg: &PendingMsg, dots: &str, width: u16) -> Vec<Line<'static>> {
    if msg.text.trim().is_empty() {
        // Pure "thinking" state — animated placeholder, no agent name.
        // Matches the horizontal padding of agent body text so the
        // placeholder doesn't jump when the first text delta arrives.
        let mut spans: Vec<Span<'static>> = vec![Span::raw(" ".repeat(AGENT_INDENT))];
        if !AGENT_BULLET.is_empty() {
            spans.push(Span::styled(
                format!("{AGENT_BULLET} "),
                Style::default().fg(agent_color(&msg.name)),
            ));
        }
        spans.push(Span::styled(
            format!("Thinking{dots}"),
            Style::default()
                .fg(THINKING_FG)
                .add_modifier(Modifier::ITALIC),
        ));
        let line = render_with_timestamp(spans, msg.timestamp, width);
        return line;
    }
    // Text streaming in — same rendering as Agent (no expansion in
    // live state; reasoning shown after finalization). Markdown is
    // disabled mid-stream: partial markdown (`**` without its closer)
    // emboldens the rest of the buffer until the next chunk arrives —
    // the visual jitter isn't worth the partial-render win.
    render_agent(
        &msg.name,
        &msg.text,
        &msg.reasoning,
        msg.timestamp,
        false,
        None,
        width,
        false,
    )
    .lines
}

/// User message: outline-only rounded box drawn with `╭ ╮ ╰ ╯ ─ │`.
/// Text and interior cells sit on the terminal-default bg — just the
/// border characters carry color. Padding cells inside the box are
/// kept (so text doesn't slam into the border) but render as plain
/// spaces.
///
/// When `markdown` is on, the bubble is dropped and we render the text
/// through the markdown emitter with a left-edge `│` marker — wrapping
/// styled markdown spans inside a bubble is more trouble than it's
/// worth for the small visual win.
fn render_user(
    text: &str,
    timestamp: DateTime<Local>,
    width: u16,
    markdown: bool,
) -> Vec<Line<'static>> {
    if markdown {
        return render_user_markdown(text, timestamp, width);
    }
    let area = width as usize;
    let bubble_w = area.saturating_sub(USER_GUTTER * 2).max(4);
    let interior_w = bubble_w.saturating_sub(2);
    let text_w = interior_w.saturating_sub(USER_INNER_PAD * 2);

    let ts = format_timestamp(timestamp);
    let border_style = Style::default().fg(USER_BORDER_FG);
    let gutter = Span::raw(" ".repeat(USER_GUTTER));
    let inner_pad = || Span::raw(" ".repeat(USER_INNER_PAD));

    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(vec![
        gutter.clone(),
        Span::styled(format!("╭{}╮", "─".repeat(interior_w)), border_style),
        gutter.clone(),
    ]));

    let wrapped = wrap_with_reserved_first_line(text, text_w, TIMESTAMP_WIDTH + 1);
    for (i, chunk) in wrapped.iter().enumerate() {
        let chunk_w = chunk.chars().count();
        let mut spans = vec![gutter.clone(), Span::styled("│", border_style), inner_pad()];
        if i == 0 {
            let used = chunk_w + TIMESTAMP_WIDTH + 1;
            let middle = text_w.saturating_sub(used);
            spans.push(Span::raw(chunk.clone()));
            spans.push(Span::raw(" ".repeat(middle)));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(ts.clone(), Style::default().fg(TIMESTAMP_FG)));
        } else {
            let middle = text_w.saturating_sub(chunk_w);
            spans.push(Span::raw(chunk.clone()));
            spans.push(Span::raw(" ".repeat(middle)));
        }
        spans.push(inner_pad());
        spans.push(Span::styled("│", border_style));
        spans.push(gutter.clone());
        out.push(Line::from(spans));
    }

    out.push(Line::from(vec![
        gutter.clone(),
        Span::styled(format!("╰{}╯", "─".repeat(interior_w)), border_style),
        gutter,
    ]));

    out
}

/// Markdown-styled user message: no bubble, left-edge `│` marker in
/// the user-message border color, timestamp right-aligned on row 1.
fn render_user_markdown(text: &str, timestamp: DateTime<Local>, width: u16) -> Vec<Line<'static>> {
    let bar_style = Style::default().fg(USER_BORDER_FG);
    let body = markdown::render(text);
    let ts = format_timestamp(timestamp);
    let area = width as usize;

    let mut out: Vec<Line<'static>> = Vec::with_capacity(body.len());
    for (i, line) in body.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
        spans.push(Span::styled("│ ".to_string(), bar_style));
        let body_width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        spans.extend(line.spans);
        if i == 0 {
            let used = 2 + body_width;
            let pad = area
                .saturating_sub(used + TIMESTAMP_WIDTH + 1)
                .saturating_add(1);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(ts.clone(), Style::default().fg(TIMESTAMP_FG)));
        }
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        let mut spans: Vec<Span<'static>> = vec![Span::styled("│ ".to_string(), bar_style)];
        let pad = area
            .saturating_sub(2 + TIMESTAMP_WIDTH + 1)
            .saturating_add(1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(ts, Style::default().fg(TIMESTAMP_FG)));
        out.push(Line::from(spans));
    }
    out
}

/// Agent reply: `• text...` with timestamp right-aligned, optional
/// indented reasoning trailing when expanded. The agent name is *not*
/// rendered per-line — the active-agent indicator in the chrome is the
/// canonical place. Returns the row-index of the clickable thinking
/// chip (if any) so callers can build a hit map.
fn render_agent(
    name: &str,
    text: &str,
    reasoning: &str,
    timestamp: DateTime<Local>,
    expanded: bool,
    think_duration: Option<Duration>,
    width: u16,
    markdown: bool,
) -> Rendered {
    let _ = name;
    let bullet_width: usize = AGENT_INDENT
        + if AGENT_BULLET.is_empty() {
            0
        } else {
            AGENT_BULLET.chars().count() + 1 // bullet + space
        };
    let indent_span = || Span::raw(" ".repeat(AGENT_INDENT));
    let has_reasoning = !reasoning.trim().is_empty();
    let reserve_first = TIMESTAMP_WIDTH + 1;

    let mut out: Vec<Line<'static>> = Vec::new();
    // Parallel to `out`: `conts[i]` is `true` when row `i` is a
    // soft-wrap continuation of the previous logical line. The copy
    // path uses this to rejoin soft-wraps with a space instead of a
    // newline.
    let mut conts: Vec<bool> = Vec::new();
    let mut chip_row = None;

    // When the agent produced reasoning, the *first* row of this entry
    // is the bullet + chip line — replacing the "Thinking…" placeholder
    // that lived there during streaming.  The timestamp lands on the
    // first actual text line (render_first_line_with_timestamp handles
    // that naturally for the first wrapped text chunk).
    if has_reasoning {
        let arrow = if expanded { "▼" } else { "▶" };
        let action_hint = if expanded {
            "ctrl+j to collapse"
        } else {
            "ctrl+j to expand"
        };
        let label = match think_duration {
            Some(d) => format!(
                "{arrow} thought for {} ({action_hint})",
                format_think_duration(d)
            ),
            None => format!("{arrow} thinking ({action_hint})"),
        };
        chip_row = Some(out.len());
        let indent = " ".repeat(bullet_width);
        // Wrap to width minus left indent (bullet_width == AGENT_INDENT
        // since the bullet is empty) minus a matching right pad
        // (AGENT_INDENT) so body lines have symmetric breathing room.
        let text_width = (width as usize)
            .saturating_sub(bullet_width + AGENT_INDENT)
            .max(1);
        let label_width = label.chars().count();
        // Default wrap (used for the expanded body and for wrapped[1..]
        // continuation lines in the collapsed case). The collapsed-no-
        // markdown branch will re-wrap with extra reserve so the first
        // chunk can sit beside the chip without pushing the timestamp.
        let wrapped: Vec<String> = wrap_with_reserved_first_line(text, text_width, 0);

        let mut chip_spans: Vec<Span<'static>> = vec![indent_span()];
        if !AGENT_BULLET.is_empty() {
            chip_spans.push(Span::styled(
                format!("{AGENT_BULLET} "),
                Style::default().fg(agent_color(name)),
            ));
        }
        chip_spans.push(Span::styled(
            label,
            Style::default()
                .fg(THINKING_FG)
                .add_modifier(Modifier::DIM | Modifier::UNDERLINED),
        ));

        // Body content target width: full width minus left indent
        // (AGENT_INDENT) and a matching right pad (AGENT_INDENT) so
        // wrapped continuations don't go all the way to the right
        // edge.
        let body_content_w = (width as usize).saturating_sub(2 * AGENT_INDENT).max(1);
        let (body_lines, body_conts): (Vec<Line<'static>>, Vec<bool>) = if markdown {
            // Pre-wrap the markdown lines ourselves so ratatui's
            // Paragraph::wrap doesn't strip the indent on
            // continuation rows.
            let (wrapped_md, md_conts) =
                wrap_lines_to_width(markdown::render(text), body_content_w);
            (indent_lines(wrapped_md, AGENT_INDENT), md_conts)
        } else {
            let lines = wrapped
                .iter()
                .map(|chunk| Line::from(vec![Span::raw(format!("{indent}{chunk}"))]))
                .collect::<Vec<_>>();
            // wrapped[0] starts a fresh logical line; the rest are
            // soft-wrap continuations of the agent's text.
            let conts = (0..lines.len()).map(|i| i > 0).collect();
            (lines, conts)
        };

        if expanded {
            // Chip alone on row 1; reasoning lines under it, nested
            // under the chip's text (column ≈ AGENT_INDENT + 2 to land
            // right after "▼ "); then the agent's text. The user reads
            // the reasoning *before* the conclusion. Long reasoning
            // lines wrap explicitly so the continuation keeps the same
            // left indent — otherwise ratatui's auto-wrap drops them
            // to column 0 and the block looks ragged.
            out.push(render_first_line_timestamped(
                chip_spans, timestamp, width, true,
            ));
            conts.push(false);
            let reasoning_indent = AGENT_INDENT + 2;
            let reasoning_w = (width as usize).saturating_sub(reasoning_indent).max(1);
            for raw_line in reasoning.lines() {
                let chunks = if raw_line.is_empty() {
                    vec![String::new()]
                } else {
                    wrap_with_reserved_first_line_and_prefix(raw_line, reasoning_w, 0, 0)
                };
                for (i, chunk) in chunks.into_iter().enumerate() {
                    out.push(Line::from(vec![
                        Span::raw(" ".repeat(reasoning_indent)),
                        Span::styled(chunk, Style::default().fg(REASONING_FG)),
                    ]));
                    conts.push(i > 0);
                }
            }
            out.extend(body_lines);
            conts.extend(body_conts);
        } else if markdown {
            // Collapsed + markdown: chip on its own row (folding
            // markdown spans onto the chip line is more visual jank than
            // it's worth), body markdown lines follow.
            out.push(render_first_line_timestamped(
                chip_spans, timestamp, width, true,
            ));
            conts.push(false);
            out.extend(body_lines);
            conts.extend(body_conts);
        } else {
            // Collapsed: chip + first text chunk on the same line so
            // there's no visual blank between the chip and the answer.
            // The first chunk shares row 1 with `chip + " "` and the
            // right-edge timestamp, so re-wrap with both reserved —
            // otherwise the chunk pushes the timestamp onto row 2.
            let collapsed_first_reserve = label_width + 1 + TIMESTAMP_WIDTH + 1;
            let collapsed_wrapped: Vec<String> =
                wrap_with_reserved_first_line(text, text_width, collapsed_first_reserve);
            let mut first_line_spans = chip_spans;
            if !collapsed_wrapped.is_empty() {
                first_line_spans.push(Span::raw(" "));
                first_line_spans.push(Span::raw(collapsed_wrapped[0].clone()));
            }
            out.push(render_first_line_timestamped(
                first_line_spans,
                timestamp,
                width,
                true,
            ));
            conts.push(false);
            for chunk in collapsed_wrapped.iter().skip(1) {
                out.push(Line::from(vec![Span::raw(format!("{indent}{chunk}"))]));
                conts.push(true);
            }
        }
    } else if markdown {
        // No reasoning + markdown: emit markdown lines, attaching the
        // timestamp to the first line via right-edge padding. Every
        // line carries AGENT_INDENT on the left AND a matching right
        // pad. Pre-wrap with `wrap_lines_to_width` so ratatui's
        // Paragraph::wrap can't strip the indent from continuation
        // rows by re-wrapping at the full pane width.
        let body_content_w = (width as usize).saturating_sub(2 * AGENT_INDENT).max(1);
        let (wrapped_md, md_conts) = wrap_lines_to_width(markdown::render(text), body_content_w);
        let body = indent_lines(wrapped_md, AGENT_INDENT);
        if body.is_empty() {
            out.extend(render_with_timestamp(vec![], timestamp, width));
            conts.push(false);
        } else {
            // First line additionally reserves TIMESTAMP_WIDTH+1 for
            // the right-edge timestamp — already includes the left
            // indent because `body` was indent_lines'd above. If the
            // first line's content overlaps the timestamp column,
            // slice it (preserving span styles) and flow the tail
            // onto row 2 (which IS a soft-wrap continuation).
            let first_line_budget = (width as usize).saturating_sub(TIMESTAMP_WIDTH + 1).max(1);
            let mut iter = body.into_iter().zip(md_conts.into_iter());
            let (first, first_cont) = iter.next().expect("body non-empty");
            let (head, tail) = slice_spans_at_width(first.spans, first_line_budget);
            out.push(render_first_line_with_timestamp(head, timestamp, width));
            conts.push(first_cont);
            if let Some(tail_spans) = tail {
                let mut spans = vec![Span::raw(" ".repeat(AGENT_INDENT))];
                spans.extend(tail_spans);
                out.push(Line::from(spans));
                // Tail is a hard-wrap (from the timestamp slice) of
                // the *same* logical line, so mark as continuation.
                conts.push(true);
            }
            for (line, cont) in iter {
                out.push(line);
                conts.push(cont);
            }
        }
    } else {
        // No reasoning, no markdown — text gets the standard left
        // indent and a matching right pad; the timestamp is right-
        // aligned on the first wrapped line. Wrap area is `width -
        // 2*AGENT_INDENT` so continuations leave breathing room on
        // both sides.
        let chunks = wrap_with_reserved_first_line_and_prefix(
            text,
            (width as usize)
                .saturating_sub(bullet_width + AGENT_INDENT)
                .max(1),
            reserve_first,
            0,
        );
        if chunks.is_empty() {
            out.extend(render_with_timestamp(vec![], timestamp, width));
            conts.push(false);
        } else {
            for (i, chunk) in chunks.iter().enumerate() {
                if i == 0 {
                    let mut spans: Vec<Span<'static>> = vec![indent_span()];
                    if !AGENT_BULLET.is_empty() {
                        spans.push(Span::styled(
                            format!("{AGENT_BULLET} "),
                            Style::default().fg(agent_color(name)),
                        ));
                    }
                    spans.push(Span::raw(chunk.clone()));
                    out.push(render_first_line_with_timestamp(spans, timestamp, width));
                    conts.push(false);
                } else {
                    let indent = " ".repeat(bullet_width);
                    out.push(Line::from(vec![Span::raw(format!("{indent}{chunk}"))]));
                    conts.push(true);
                }
            }
        }
    }

    Rendered {
        lines: out,
        chip_row,
        continuations: conts,
    }
}

/// `(glyph, label)` for a tool's rendered line. `glyph` is an emoji
/// with a trailing space when `emojis` is on, empty otherwise; `label`
/// is the verb shown bold before the `:`. With emojis on, the lock /
/// unlock emoji conveys the lock state so the lock variants collapse to
/// the base verb (`readlock` → `read`); with emojis off the full tool
/// name is kept so the lock state stays legible.
pub fn tool_glyph_label(tool: &str, emojis: bool) -> (String, String) {
    let (glyph, label): (&str, &str) = match tool {
        "bash" => ("⚙️", "bash"),
        "read" => ("📖", "read"),
        "readlock" => ("🔒", if emojis { "read" } else { "readlock" }),
        "unlock" => ("🔓", "unlock"),
        "write" => ("🖋️", "write"),
        "writeunlock" => ("🔓", if emojis { "write" } else { "writeunlock" }),
        "edit" => ("🖋️", "edit"),
        "editunlock" => ("🔓", if emojis { "edit" } else { "editunlock" }),
        other => ("", other),
    };
    let glyph = if emojis && !glyph.is_empty() {
        format!("{glyph} ")
    } else {
        String::new()
    };
    (glyph, label.to_string())
}

fn tool_state_style(state: ToolCallState) -> Style {
    match state {
        ToolCallState::Processing => Style::default().fg(Color::Yellow),
        ToolCallState::Success => Style::default().fg(Color::White),
        ToolCallState::Failed => Style::default().fg(Color::Red),
        ToolCallState::BadCall => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

/// Tools whose output is worth showing when a box is expanded. Input-
/// only tools (read/readlock/unlock) never show output — the user can
/// open the file themselves. Public so the event handler can avoid
/// storing large outputs it will never display.
pub fn tool_shows_output(tool: &str) -> bool {
    !matches!(tool, "read" | "readlock" | "unlock")
}

/// Spans for one tool-call line: `[glyph] label: summary`, the label
/// bold and the whole line tinted by `state`.
fn tool_call_spans(
    tool: &str,
    text: &str,
    state: ToolCallState,
    emojis: bool,
) -> Vec<Span<'static>> {
    let (glyph, label) = tool_glyph_label(tool, emojis);
    let style = tool_state_style(state);
    let mut spans = Vec::new();
    if !glyph.is_empty() {
        spans.push(Span::raw(glyph));
    }
    spans.push(Span::styled(
        format!("{label}:"),
        style.add_modifier(Modifier::BOLD),
    ));
    if !text.is_empty() {
        spans.push(Span::raw(" ".to_string()));
        spans.push(Span::styled(text.to_string(), style));
    }
    spans
}

/// Chars available for a collapsed summary after the left `indent`, the
/// glyph, the bold `label`, and the `": "` separator.
fn tool_summary_budget(tool: &str, width: usize, indent: usize, emojis: bool) -> usize {
    let (glyph, label) = tool_glyph_label(tool, emojis);
    let prefix = indent + glyph.chars().count() + label.chars().count() + 2;
    width.saturating_sub(prefix).max(8)
}

/// Truncate `s` to `max` columns with a trailing `…` when it overflows.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Topmost visible call index for a collapsed [`HistoryEntry::ToolBox`].
/// `follow` pins to the last [`TOOLBOX_VISIBLE`] calls; otherwise the
/// stored `view_offset` (clamped) wins. Public so the scroll handler
/// can compute the same window.
pub fn toolbox_top(len: usize, view_offset: usize, follow: bool) -> usize {
    if len <= TOOLBOX_VISIBLE {
        return 0;
    }
    let max_offset = len - TOOLBOX_VISIBLE;
    if follow {
        max_offset
    } else {
        view_offset.min(max_offset)
    }
}

/// Left sidebar glyph for row `i` of an `n`-row box: rounded caps top
/// and bottom, a plain rule in between, a single rule for a 1-row box.
fn sidebar_glyph(i: usize, n: usize) -> char {
    if n <= 1 {
        '│'
    } else if i == 0 {
        '╭'
    } else if i + 1 == n {
        '╰'
    } else {
        '│'
    }
}

/// Render a [`HistoryEntry::ToolBox`]: a light-grey rounded sidebar with
/// the tool-call lines inside it. Collapsed shows up to
/// [`TOOLBOX_VISIBLE`] calls (windowed by scroll/follow); expanded shows
/// every call in full, including input + output for output-bearing
/// tools.
fn render_toolbox(
    calls: &[ToolCall],
    view_offset: usize,
    follow: bool,
    expanded: bool,
    width: u16,
    emojis: bool,
) -> Rendered {
    let mut content: Vec<Vec<Span<'static>>> = Vec::new();

    if expanded {
        for call in calls {
            let input_lines: Vec<&str> = call.full_input.split('\n').collect();
            let first = input_lines.first().copied().unwrap_or("");
            content.push(tool_call_spans(&call.tool, first, call.state, emojis));
            for cont in input_lines.iter().skip(1) {
                content.push(vec![Span::styled(
                    (*cont).to_string(),
                    tool_state_style(call.state),
                )]);
            }
            if tool_shows_output(&call.tool) && !call.output.is_empty() {
                for out_line in call.output.split('\n') {
                    content.push(vec![Span::styled(
                        format!("    {out_line}"),
                        Style::default().fg(TOOL_OUTPUT_FG),
                    )]);
                }
            }
        }
    } else {
        let top = toolbox_top(calls.len(), view_offset, follow);
        for call in calls.iter().skip(top).take(TOOLBOX_VISIBLE) {
            let budget = tool_summary_budget(&call.tool, width as usize, 2, emojis);
            content.push(tool_call_spans(
                &call.tool,
                &truncate(&call.summary, budget),
                call.state,
                emojis,
            ));
        }
    }

    if content.is_empty() {
        content.push(Vec::new());
    }

    let n = content.len();
    let mut out: Vec<Line<'static>> = Vec::with_capacity(n);
    for (i, mut spans) in content.into_iter().enumerate() {
        let mut row = vec![
            Span::styled(
                sidebar_glyph(i, n).to_string(),
                Style::default().fg(SIDEBAR_FG),
            ),
            Span::raw(" ".to_string()),
        ];
        row.append(&mut spans);
        out.push(Line::from(row));
    }
    let continuations = vec![false; out.len()];
    Rendered {
        lines: out,
        chip_row: None,
        continuations,
    }
}

/// Build a one-line span vec with an HH:MM timestamp right-aligned at
/// the area edge. The leading spans fill from the left; padding spaces
/// take up the slack.
fn render_with_timestamp(
    spans: Vec<Span<'static>>,
    timestamp: DateTime<Local>,
    width: u16,
) -> Vec<Line<'static>> {
    vec![render_first_line_timestamped(spans, timestamp, width, true)]
}

fn render_first_line_timestamped(
    mut spans: Vec<Span<'static>>,
    timestamp: DateTime<Local>,
    width: u16,
    add_timestamp: bool,
) -> Line<'static> {
    if !add_timestamp {
        return Line::from(spans);
    }
    let area = width as usize;
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let ts = format_timestamp(timestamp);
    let needed = used + TIMESTAMP_WIDTH + 1;
    let pad = area.saturating_sub(needed);
    spans.push(Span::raw(" ".repeat(pad + 1)));
    spans.push(Span::styled(ts, Style::default().fg(TIMESTAMP_FG)));
    Line::from(spans)
}

fn render_first_line_with_timestamp(
    spans: Vec<Span<'static>>,
    timestamp: DateTime<Local>,
    width: u16,
) -> Line<'static> {
    render_first_line_timestamped(spans, timestamp, width, true)
}

/// Re-wrap a `Vec<Line>` so every emitted line's content fits within
/// `max_width` cells. Uses `slice_spans_at_width` repeatedly to split
/// long lines on whitespace boundaries (or hard-cut for unbroken
/// tokens), preserving each span's style across the splits.
///
/// Returns `(wrapped_lines, continuations)` — `continuations[i]` is
/// `true` when row `i` is a soft-wrap continuation of the previous
/// row (i.e., it came from the same input Line), `false` for rows
/// that start a fresh input Line. The copy path uses this to join
/// continuations with a space and starts-of-line with a newline.
///
/// Used to pre-wrap markdown-rendered agent bodies so ratatui's
/// `Paragraph::wrap` doesn't drop continuation rows to column 0 and
/// destroy the indent we added with [`indent_lines`].
fn wrap_lines_to_width(
    lines: Vec<Line<'static>>,
    max_width: usize,
) -> (Vec<Line<'static>>, Vec<bool>) {
    if max_width == 0 {
        let conts = vec![false; lines.len()];
        return (lines, conts);
    }
    let mut out = Vec::with_capacity(lines.len());
    let mut conts = Vec::with_capacity(lines.len());
    for line in lines {
        let mut remaining = line.spans;
        let mut first = true;
        loop {
            let (head, tail) = slice_spans_at_width(remaining, max_width);
            out.push(Line::from(head));
            conts.push(!first);
            first = false;
            match tail {
                Some(t) => remaining = t,
                None => break,
            }
        }
    }
    (out, conts)
}

/// Prepend `n` cells of left padding to every line. Used to apply
/// `AGENT_INDENT` to markdown-rendered agent bodies whose lines come
/// back without any leading indent.
fn indent_lines(lines: Vec<Line<'static>>, n: usize) -> Vec<Line<'static>> {
    if n == 0 {
        return lines;
    }
    let prefix = " ".repeat(n);
    lines
        .into_iter()
        .map(|mut l| {
            let mut spans = vec![Span::raw(prefix.clone())];
            spans.append(&mut l.spans);
            Line::from(spans)
        })
        .collect()
}

/// Slice a styled span sequence so the head totals at most `max_width`
/// columns. If the spans already fit, returns `(spans, None)`. Otherwise
/// breaks on the last whitespace boundary inside the budget (or at the
/// hard limit if no whitespace exists), preserving each span's style on
/// both halves. Used by the markdown agent renderer so the right-edge
/// timestamp stays anchored on row 1 when the agent's first line would
/// otherwise overflow into the timestamp's reserved column.
fn slice_spans_at_width(
    spans: Vec<Span<'static>>,
    max_width: usize,
) -> (Vec<Span<'static>>, Option<Vec<Span<'static>>>) {
    let total: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if total <= max_width || max_width == 0 {
        return (spans, None);
    }
    let flat: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    // Prefer breaking right after the last whitespace that lands inside
    // the budget; fall back to a hard cut at max_width if the head has
    // no whitespace at all (e.g. a single very long token).
    let split_at = (0..max_width)
        .rev()
        .find(|&i| flat[i].0.is_whitespace())
        .map(|i| i + 1)
        .unwrap_or(max_width);
    let head = group_into_spans(&flat[..split_at]);
    let tail = group_into_spans(&flat[split_at..]);
    let tail = if tail.is_empty() { None } else { Some(tail) };
    (head, tail)
}

fn group_into_spans(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut cur_style: Option<Style> = None;
    let mut cur_text = String::new();
    for &(c, style) in chars {
        match cur_style {
            Some(s) if s == style => cur_text.push(c),
            _ => {
                if let Some(s) = cur_style.take() {
                    out.push(Span::styled(std::mem::take(&mut cur_text), s));
                }
                cur_style = Some(style);
                cur_text.push(c);
            }
        }
    }
    if let Some(s) = cur_style {
        if !cur_text.is_empty() {
            out.push(Span::styled(cur_text, s));
        }
    }
    out
}

fn blank_bg_line(width: usize, bg: Color) -> Line<'static> {
    Line::from(vec![Span::styled(
        " ".repeat(width),
        Style::default().bg(bg),
    )])
}

fn format_timestamp(t: DateTime<Local>) -> String {
    t.format("%H:%M").to_string()
}

/// Split `text` into chunks that fit within `area_width`, reserving
/// `reserve_first` extra columns on the *first* line (so a timestamp
/// can land at the right edge without overlapping the text). Greedy
/// word-wrap on whitespace boundaries; falls back to hard char-break
/// for single words longer than the wrap width.
fn wrap_with_reserved_first_line(
    text: &str,
    area_width: usize,
    reserve_first: usize,
) -> Vec<String> {
    wrap_with_reserved_first_line_and_prefix(text, area_width, reserve_first, 0)
}

/// Like [`wrap_with_reserved_first_line`] but the first line is
/// further shortened by `prefix_width` (because an agent-name prefix
/// will be prepended to it before display).
fn wrap_with_reserved_first_line_and_prefix(
    text: &str,
    area_width: usize,
    reserve_first: usize,
    prefix_width: usize,
) -> Vec<String> {
    if area_width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() && out.is_empty() {
            // preserve leading blank lines as empty chunks
            out.push(String::new());
            continue;
        }
        let first_width = area_width
            .saturating_sub(reserve_first)
            .saturating_sub(prefix_width.saturating_mul(out.is_empty() as usize));
        let mut budget = if out.is_empty() {
            first_width.max(1)
        } else {
            area_width.max(1)
        };

        let mut current = String::new();
        let mut current_width = 0usize;
        for word in line.split_inclusive(|c: char| c == ' ' || c == '\t') {
            let w = word.chars().count();
            if w + current_width <= budget {
                current.push_str(word);
                current_width += w;
            } else if current_width == 0 {
                // Single word longer than budget — emit a hard slice.
                let mut remaining = word;
                while !remaining.is_empty() {
                    let take = remaining.chars().take(budget).collect::<String>();
                    let n = take.chars().count();
                    out.push(take);
                    remaining = &remaining[remaining
                        .char_indices()
                        .nth(n)
                        .map(|(i, _)| i)
                        .unwrap_or(remaining.len())..];
                    budget = area_width.max(1);
                }
            } else {
                out.push(std::mem::take(&mut current));
                current_width = 0;
                budget = area_width.max(1);
                if w <= budget {
                    current.push_str(word);
                    current_width = w;
                } else {
                    let mut remaining = word;
                    while !remaining.is_empty() {
                        let take = remaining.chars().take(budget).collect::<String>();
                        let n = take.chars().count();
                        out.push(take);
                        remaining = &remaining[remaining
                            .char_indices()
                            .nth(n)
                            .map(|(i, _)| i)
                            .unwrap_or(remaining.len())..];
                    }
                }
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Feed a streaming text delta through the `<think>` tag router.
/// Outside of think tags, content goes to `text`; inside, content goes
/// to `reasoning`. Partial tags at the chunk boundary (e.g. ending in
/// `<th`) are buffered in `tag_partial` and resolved on the next
/// delta. Returns `true` if any non-think-block text content was
/// appended — callers use this as the signal to mark `text_started_at`.
///
/// Why streaming-aware: many open-weights thinking-mode models inline
/// reasoning as `<think>...</think>` blocks in the regular content
/// stream rather than using the OpenAI-compat `reasoning_content`
/// field. Post-finalize stripping would work but flashes the
/// reasoning live before hiding it, which is what the user reported
/// as "thinking block is always displayed."
pub fn route_text_delta(
    chunk: &str,
    text: &mut String,
    reasoning: &mut String,
    inside_think: &mut bool,
    tag_partial: &mut String,
) -> bool {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";

    let mut buf = std::mem::take(tag_partial);
    buf.push_str(chunk);
    let mut wrote_text = false;
    let mut remaining = buf.as_str();
    while !remaining.is_empty() {
        if *inside_think {
            if let Some(idx) = remaining.find(CLOSE) {
                reasoning.push_str(&remaining[..idx]);
                remaining = &remaining[idx + CLOSE.len()..];
                *inside_think = false;
                // Drop a single `\n` directly after `</think>` so the
                // answer doesn't render with a leading blank line.
                if let Some(rest) = remaining.strip_prefix('\n') {
                    remaining = rest;
                }
            } else if let Some(idx) = trailing_partial_match(remaining, CLOSE) {
                reasoning.push_str(&remaining[..idx]);
                *tag_partial = remaining[idx..].to_string();
                return wrote_text;
            } else {
                reasoning.push_str(remaining);
                return wrote_text;
            }
        } else {
            if let Some(idx) = remaining.find(OPEN) {
                if idx > 0 {
                    text.push_str(&remaining[..idx]);
                    wrote_text = true;
                }
                remaining = &remaining[idx + OPEN.len()..];
                *inside_think = true;
                // Drop a single `\n` directly after `<think>` so the
                // reasoning block doesn't start with a blank line.
                if let Some(rest) = remaining.strip_prefix('\n') {
                    remaining = rest;
                }
            } else if let Some(idx) = trailing_partial_match(remaining, OPEN) {
                if idx > 0 {
                    text.push_str(&remaining[..idx]);
                    wrote_text = true;
                }
                *tag_partial = remaining[idx..].to_string();
                return wrote_text;
            } else {
                text.push_str(remaining);
                wrote_text = true;
                return wrote_text;
            }
        }
    }
    wrote_text
}

/// Return `Some(idx)` if the *tail* of `s` is a strict prefix of
/// `tag` — meaning we should buffer everything from `idx` onward
/// because it might be the start of `tag`. Length-1 matches like a
/// trailing `<` count; we want to buffer them so the next delta can
/// finish the tag.
fn trailing_partial_match(s: &str, tag: &str) -> Option<usize> {
    let max = tag.len().min(s.len());
    for n in (1..max).rev() {
        if s.ends_with(&tag[..n]) {
            return Some(s.len() - n);
        }
    }
    None
}

/// Advance the thinking dots through `"" → "." → ".." → "..."` on a
/// 333 ms phase cycle. The empty phase is intentional — the visible
/// "Thinking" word stays put while the dots vanish and re-appear,
/// giving a clearer "still working" pulse than a fixed-width
/// animation.
pub fn thinking_dots(elapsed_ms: u128) -> &'static str {
    match (elapsed_ms / 333) % 4 {
        0 => "",
        1 => ".",
        2 => "..",
        _ => "...",
    }
}

/// [`thinking_dots`] space-padded to a fixed width of 3 (`"" → "   "`,
/// `"..." → "..."`). Used by the status indicator so the trailing
/// timer stays horizontally fixed instead of jiggling as the dots
/// cycle.
pub fn thinking_dots_padded(elapsed_ms: u128) -> String {
    format!("{:<3}", thinking_dots(elapsed_ms))
}

/// Format an elapsed span for the working / thinking status indicator:
/// `(Xs)` under a minute, `(Xm Ys)` at or beyond. Whole seconds only —
/// the indicator advances once a second; sub-second precision is noise.
pub fn format_status_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("({secs}s)")
    } else {
        format!("({}m {}s)", secs / 60, secs % 60)
    }
}

/// Format a thinking duration. Examples: `0.4 seconds`, `7 seconds`,
/// `2m 14s` for longer pauses. Single-precision feels right for the
/// in-chat chip — exact milliseconds are noise.
pub fn format_think_duration(d: Duration) -> String {
    let total_ms = d.as_millis();
    if total_ms < 1000 {
        return "<1 second".to_string();
    }
    let total_secs = d.as_secs();
    if total_secs < 60 {
        if total_secs < 10 {
            let secs = total_ms as f64 / 1000.0;
            return format!("{secs:.1} seconds");
        }
        return format!("{total_secs} seconds");
    }
    let m = total_secs / 60;
    let s = total_secs % 60;
    format!("{m}m {s}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dots_cycle_four_phases() {
        assert_eq!(thinking_dots(0), "");
        assert_eq!(thinking_dots(333), ".");
        assert_eq!(thinking_dots(700), "..");
        assert_eq!(thinking_dots(1000), "...");
        // phase 4 wraps to ""
        assert_eq!(thinking_dots(333 * 4), "");
    }

    #[test]
    fn format_duration_human_readable() {
        assert_eq!(
            format_think_duration(Duration::from_millis(500)),
            "<1 second"
        );
        assert_eq!(
            format_think_duration(Duration::from_millis(1500)),
            "1.5 seconds"
        );
        assert_eq!(format_think_duration(Duration::from_secs(7)), "7.0 seconds");
        assert_eq!(format_think_duration(Duration::from_secs(45)), "45 seconds");
        assert_eq!(format_think_duration(Duration::from_secs(134)), "2m 14s");
    }

    #[test]
    fn padded_dots_are_always_width_three() {
        for ms in [0u128, 333, 700, 1000] {
            assert_eq!(thinking_dots_padded(ms).chars().count(), 3);
        }
        assert_eq!(thinking_dots_padded(0), "   ");
        assert_eq!(thinking_dots_padded(1000), "...");
    }

    #[test]
    fn status_elapsed_switches_to_minutes_at_sixty_seconds() {
        assert_eq!(format_status_elapsed(Duration::from_secs(0)), "(0s)");
        assert_eq!(format_status_elapsed(Duration::from_secs(5)), "(5s)");
        assert_eq!(format_status_elapsed(Duration::from_secs(59)), "(59s)");
        assert_eq!(format_status_elapsed(Duration::from_secs(60)), "(1m 0s)");
        assert_eq!(format_status_elapsed(Duration::from_secs(134)), "(2m 14s)");
        // Sub-second is floored, not rounded up.
        assert_eq!(format_status_elapsed(Duration::from_millis(1900)), "(1s)");
    }

    #[test]
    fn wrap_handles_short_lines() {
        let chunks = wrap_with_reserved_first_line("hi there", 40, 6);
        assert_eq!(chunks, vec!["hi there".to_string()]);
    }

    #[test]
    fn wrap_breaks_when_first_line_would_overlap_timestamp() {
        // area=20, reserve=6 → first line gets 14 chars
        let chunks = wrap_with_reserved_first_line("hello world how are you today", 20, 6);
        // First chunk fits in 14, rest wraps to 20-wide.
        assert!(chunks[0].chars().count() <= 14);
    }

    fn line_width(line: &Line<'static>) -> usize {
        line.spans.iter().map(|s| s.content.chars().count()).sum()
    }

    fn fixed_ts() -> DateTime<Local> {
        // Any concrete instant works — only the formatted "HH:MM"
        // width matters for these tests.
        Local::now()
    }

    #[test]
    fn agent_timestamp_stays_anchored_when_text_would_overlap() {
        // A long single-paragraph reply with no reasoning + no markdown.
        // Width 60 → text budget for first line is 60 - 2 (indent) - 5
        // (timestamp) - 1 (gap) = 52. The renderer must wrap before
        // that so the first row never exceeds the area width.
        let text = "x".repeat(200);
        let width: u16 = 60;
        let rendered = render_agent("coder", &text, "", fixed_ts(), false, None, width, false);
        assert!(!rendered.lines.is_empty());
        // The first line carries the timestamp and must fit in `width`
        // so ratatui's auto-wrap can't push the timestamp to row 2.
        assert!(
            line_width(&rendered.lines[0]) <= width as usize,
            "row 1 width = {}, area = {}",
            line_width(&rendered.lines[0]),
            width
        );
    }

    #[test]
    fn collapsed_chip_does_not_push_timestamp_off_row_one() {
        // Reasoning present + collapsed → chip label + " " + first
        // chunk + " " + timestamp must all fit in `width`.
        let width: u16 = 80;
        let rendered = render_agent(
            "coder",
            &"a ".repeat(200),
            "some hidden reasoning",
            fixed_ts(),
            /* expanded */ false,
            Some(Duration::from_secs(3)),
            width,
            /* markdown */ false,
        );
        assert!(line_width(&rendered.lines[0]) <= width as usize);
    }

    #[test]
    fn slice_spans_breaks_on_whitespace_when_possible() {
        let spans = vec![Span::raw("hello world how are you today".to_string())];
        let (head, tail) = slice_spans_at_width(spans, 14);
        let head_text: String = head.iter().map(|s| s.content.to_string()).collect();
        assert!(head_text.chars().count() <= 14);
        // "hello world " is 12 chars and breaks on a whitespace ≤ 14.
        assert!(head_text.ends_with(' '));
        assert!(tail.is_some());
    }

    #[test]
    fn slice_spans_preserves_styles_across_split() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        // No whitespace inside the bold span and the split lands in
        // the middle of it, so the bold style must appear on both
        // halves after grouping.
        let spans = vec![
            Span::raw("ab".to_string()),
            Span::styled("BOLDEDTOKEN".to_string(), bold),
            Span::raw("cd".to_string()),
        ];
        let (head, tail) = slice_spans_at_width(spans, 6);
        let tail = tail.expect("has tail");
        assert!(head.iter().any(|s| s.style == bold));
        assert!(tail.iter().any(|s| s.style == bold));
    }

    // ── tool box ──────────────────────────────────────────────────────

    fn mk_call(tool: &str, summary: &str, state: ToolCallState) -> ToolCall {
        ToolCall {
            call_id: "id".into(),
            tool: tool.into(),
            summary: summary.into(),
            full_input: summary.into(),
            output: String::new(),
            state,
        }
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn glyph_label_collapses_lock_variants_only_with_emoji() {
        // Emoji on: the lock/unlock emoji carries the lock state, so the
        // label collapses to the base verb.
        assert_eq!(tool_glyph_label("readlock", true).1, "read");
        assert_eq!(tool_glyph_label("writeunlock", true).1, "write");
        // Emoji off: keep the full tool name so the lock state is legible.
        assert_eq!(tool_glyph_label("readlock", false).1, "readlock");
        assert_eq!(tool_glyph_label("writeunlock", false).1, "writeunlock");
        // A glyph only appears when emojis are enabled.
        assert!(tool_glyph_label("bash", false).0.is_empty());
        assert!(!tool_glyph_label("bash", true).0.is_empty());
    }

    #[test]
    fn toolbox_top_follows_and_clamps() {
        // <= visible: always pinned to the start.
        assert_eq!(toolbox_top(3, 0, true), 0);
        assert_eq!(toolbox_top(3, 5, false), 0);
        // Following pins to the last window.
        assert_eq!(toolbox_top(10, 0, true), 10 - TOOLBOX_VISIBLE);
        // Not following: the stored offset wins, clamped to the max.
        assert_eq!(toolbox_top(10, 2, false), 2);
        assert_eq!(toolbox_top(10, 99, false), 10 - TOOLBOX_VISIBLE);
    }

    #[test]
    fn toolbox_collapsed_caps_at_visible_with_rounded_caps() {
        let calls: Vec<ToolCall> = (0..9)
            .map(|i| mk_call("bash", &format!("cmd{i}"), ToolCallState::Success))
            .collect();
        let r = render_toolbox(&calls, 0, true, false, 80, false);
        assert_eq!(r.lines.len(), TOOLBOX_VISIBLE);
        // Rounded caps top and bottom; in between the newest calls show.
        assert!(line_text(&r.lines[0]).starts_with('╭'));
        assert!(line_text(&r.lines[TOOLBOX_VISIBLE - 1]).starts_with('╰'));
        assert!(line_text(&r.lines[0]).contains("cmd3")); // 9 - 6
        assert!(line_text(&r.lines[TOOLBOX_VISIBLE - 1]).contains("cmd8"));
    }

    #[test]
    fn toolbox_processing_call_is_yellow() {
        let calls = vec![mk_call("bash", "build", ToolCallState::Processing)];
        let r = render_toolbox(&calls, 0, true, false, 80, false);
        assert!(
            r.lines[0]
                .spans
                .iter()
                .any(|s| s.style.fg == Some(Color::Yellow))
        );
    }

    #[test]
    fn toolbox_expanded_shows_output_only_for_output_bearing_tools() {
        let mut bash = mk_call("bash", "ls", ToolCallState::Success);
        bash.output = "file_a\nfile_b".into();
        let mut read = mk_call("read", "f.txt", ToolCallState::Success);
        read.output = "SHOULD_NOT_SHOW".into(); // input-only — never displayed
        let r = render_toolbox(&[bash, read], 0, true, true, 80, false);
        let joined = r.lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("file_a") && joined.contains("file_b"));
        assert!(!joined.contains("SHOULD_NOT_SHOW"));
    }

    #[test]
    fn toolbox_honors_emoji_setting() {
        let calls = vec![mk_call("read", "f.txt", ToolCallState::Success)];
        assert!(
            !line_text(&render_toolbox(&calls, 0, true, false, 80, false).lines[0]).contains('📖')
        );
        assert!(
            line_text(&render_toolbox(&calls, 0, true, false, 80, true).lines[0]).contains('📖')
        );
    }
}
