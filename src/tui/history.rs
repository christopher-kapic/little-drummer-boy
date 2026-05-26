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
}

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
            let h: u32 = name.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
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
const AGENT_INDENT: usize = 2;

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
}

/// Render one history entry. The renderer receives the area's `width`
/// so it can right-align timestamps and pad the user-message
/// background to the full width.
///
/// `thinking` controls how reasoning is surfaced:
/// - [`ThinkingDisplay::Condensed`] (default) — clickable chip, expands on click
/// - [`ThinkingDisplay::Hidden`] — drop the chip and reasoning entirely
/// - [`ThinkingDisplay::Verbose`] — force expanded regardless of the stored flag
pub fn render_entry(
    entry: &HistoryEntry,
    width: u16,
    thinking: ThinkingDisplay,
    md: MarkdownOpts,
) -> Rendered {
    match entry {
        HistoryEntry::User { text, timestamp } => Rendered {
            lines: render_user(text, *timestamp, width, md.user),
            chip_row: None,
        },
        HistoryEntry::Plain { line } => Rendered {
            lines: vec![Line::from(line.clone())],
            chip_row: None,
        },
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
        let mut spans = vec![
            gutter.clone(),
            Span::styled("│", border_style),
            inner_pad(),
        ];
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
        let pad = area.saturating_sub(2 + TIMESTAMP_WIDTH + 1).saturating_add(1);
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
    let mut chip_row = None;

    // When the agent produced reasoning, the *first* row of this entry
    // is the bullet + chip line — replacing the "Thinking…" placeholder
    // that lived there during streaming.  The timestamp lands on the
    // first actual text line (render_first_line_with_timestamp handles
    // that naturally for the first wrapped text chunk).
    if has_reasoning {
        let arrow = if expanded { "▼" } else { "▶" };
        let action_hint = if expanded {
            "click to collapse"
        } else {
            "click to expand"
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
        let text_width = (width as usize).saturating_sub(bullet_width).max(1);
        let wrapped: Vec<String> = text
            .split('\n')
            .flat_map(|raw_line| wrap_with_reserved_first_line(raw_line, text_width, 0))
            .collect();

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

        let body_lines: Vec<Line<'static>> = if markdown {
            indent_lines(markdown::render(text), AGENT_INDENT)
        } else {
            wrapped
                .iter()
                .map(|chunk| Line::from(vec![Span::raw(format!("{indent}{chunk}"))]))
                .collect()
        };

        if expanded {
            // Chip alone on row 1; reasoning lines under it, nested
            // under the chip's text (column ≈ AGENT_INDENT + 2 to land
            // right after "▼ "); then the agent's text. The user reads
            // the reasoning *before* the conclusion. Long reasoning
            // lines wrap explicitly so the continuation keeps the same
            // left indent — otherwise ratatui's auto-wrap drops them
            // to column 0 and the block looks ragged.
            out.push(render_first_line_timestamped(chip_spans, timestamp, width, false));
            let reasoning_indent = AGENT_INDENT + 2;
            let reasoning_w = (width as usize)
                .saturating_sub(reasoning_indent)
                .max(1);
            for raw_line in reasoning.lines() {
                let chunks = if raw_line.is_empty() {
                    vec![String::new()]
                } else {
                    wrap_with_reserved_first_line_and_prefix(raw_line, reasoning_w, 0, 0)
                };
                for chunk in chunks {
                    out.push(Line::from(vec![
                        Span::raw(" ".repeat(reasoning_indent)),
                        Span::styled(chunk, Style::default().fg(REASONING_FG)),
                    ]));
                }
            }
            out.extend(body_lines);
        } else if markdown {
            // Collapsed + markdown: chip on its own row (folding
            // markdown spans onto the chip line is more visual jank than
            // it's worth), body markdown lines follow.
            out.push(render_first_line_timestamped(chip_spans, timestamp, width, false));
            out.extend(body_lines);
        } else {
            // Collapsed: chip + first text chunk on the same line so
            // there's no visual blank between the chip and the answer.
            let mut first_line_spans = chip_spans;
            if !wrapped.is_empty() {
                first_line_spans.push(Span::raw(" "));
                first_line_spans.push(Span::raw(wrapped[0].clone()));
            }
            out.push(render_first_line_timestamped(first_line_spans, timestamp, width, false));
            for chunk in wrapped.iter().skip(1) {
                out.push(Line::from(vec![Span::raw(format!("{indent}{chunk}"))]));
            }
        }
    } else if markdown {
        // No reasoning + markdown: emit markdown lines, attaching the
        // timestamp to the first line via right-edge padding. Every
        // line carries AGENT_INDENT on the left.
        let body = indent_lines(markdown::render(text), AGENT_INDENT);
        if body.is_empty() {
            out.extend(render_with_timestamp(vec![], timestamp, width));
        } else {
            for (i, line) in body.into_iter().enumerate() {
                if i == 0 {
                    out.push(render_first_line_with_timestamp(line.spans, timestamp, width));
                } else {
                    out.push(line);
                }
            }
        }
    } else {
        // No reasoning, no markdown — text gets the standard left
        // indent; the timestamp is right-aligned on the first wrapped
        // line. The wrap is sized so `indent + chunk` fits in `width`.
        let chunks = wrap_with_reserved_first_line_and_prefix(
            text,
            (width as usize).saturating_sub(bullet_width).max(1),
            reserve_first,
            0,
        );
        if chunks.is_empty() {
            out.extend(render_with_timestamp(vec![], timestamp, width));
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
                } else {
                    let indent = " ".repeat(bullet_width);
                    out.push(Line::from(vec![Span::raw(format!("{indent}{chunk}"))]));
                }
            }
        }
    }

    Rendered {
        lines: out,
        chip_row,
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
        assert_eq!(format_think_duration(Duration::from_millis(500)), "<1 second");
        assert_eq!(format_think_duration(Duration::from_millis(1500)), "1.5 seconds");
        assert_eq!(format_think_duration(Duration::from_secs(7)), "7.0 seconds");
        assert_eq!(format_think_duration(Duration::from_secs(45)), "45 seconds");
        assert_eq!(format_think_duration(Duration::from_secs(134)), "2m 14s");
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
}
