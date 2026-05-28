//! Rendering: every `render_*` method on `App` plus the small free
//! helpers they call (token formatting, wrap math, row-estimate, the
//! toast overlay). Cluster moved here so `mod.rs` reads as event-loop
//! plumbing instead of paragraph wrangling.

use std::time::Duration;

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use crate::tui::chrome;
use crate::tui::composer::{INPUT_PREFIX, VimMode, input_prefix_width};
use crate::tui::history::{
    HistoryEntry, Rendered, format_status_elapsed, render_entry, render_pending, thinking_dots,
    thinking_dots_padded,
};
use crate::tui::theme::MUTED_COLOR_INDEX;

use super::{
    AUTOCOMPLETE_ROWS, App, INPUT_BORDER, MAX_INPUT_CONTENT, MIN_INPUT_CONTENT, Selection, Toast,
    ToastKind, WORKING_MESSAGES, slash_matches,
};

/// Startup grace before the working indicator first appears — prevents
/// quick turns from flashing it on and off.
const STATUS_GRACE: Duration = Duration::from_secs(2);
/// A reasoning block must last at least this long before the indicator
/// flips from the working line to the yellow `Thinking` override.
const THINKING_FLIP_AFTER: Duration = Duration::from_secs(2);

impl App {
    pub(super) fn model_summary_history_line(&self) -> String {
        match &self.launch.active_model {
            Some((p, m)) => format!(
                "/model: active model is now {p}/{m}{}",
                if self.launch.active_model_is_favorite {
                    " (★)"
                } else {
                    ""
                }
            ),
            None => "/model: no active model".to_string(),
        }
    }

    pub(super) fn slash_query(&self) -> Option<&str> {
        let rest = self.composer.text().strip_prefix('/')?;
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        Some(&rest[..end])
    }

    /// True when the `@`-popup should be drawn: the composer reports an
    /// active `@partial` token and the user hasn't dismissed it via Esc.
    pub(super) fn at_popup_active(&self) -> bool {
        !self.at_dismissed && self.composer.at_query().is_some()
    }

    pub(super) fn at_suggestions(&self) -> Vec<crate::tui::file_tag::Suggestion> {
        let Some(q) = self.composer.at_query() else {
            self.at_cache.borrow_mut().take();
            return Vec::new();
        };
        // Memo hit: same query as last walk → reuse (cheap clone of a
        // bounded list; far cheaper than re-walking the tree).
        if let Some((cached_q, cached)) = self.at_cache.borrow().as_ref() {
            if cached_q == q {
                return cached.clone();
            }
        }
        let walked = crate::tui::file_tag::suggestions(&self.launch.cwd, q, &self.usage_tags);
        *self.at_cache.borrow_mut() = Some((q.to_string(), walked.clone()));
        walked
    }

    pub(super) fn popup_lines(&self) -> u16 {
        if self.slash_query().is_some() || self.at_popup_active() {
            // Always reserve `AUTOCOMPLETE_ROWS` while either popup is
            // active; the renderer pads with blanks so the composer
            // doesn't shift as the candidate set narrows.
            return AUTOCOMPLETE_ROWS;
        }
        if self.show_vim_hint() { 1 } else { 0 }
    }

    /// True when the Normal-mode hint chip should occupy the popup
    /// strip. Hidden when the user has set `vim_mode` to `enabled`
    /// (advanced user; doesn't need the prompt) or `disabled` (vim
    /// off), and when the composer is in Insert mode.
    pub(super) fn show_vim_hint(&self) -> bool {
        self.vim_setting.show_hint()
            && self.composer.vim_enabled()
            && self.composer.vim_mode() == VimMode::Normal
    }

    /// Height of the queued-messages strip above the input box. Zero
    /// when nothing's queued; otherwise top border (1) + N messages +
    /// shared bottom (1). The shared bottom is the queue's bottom AND
    /// the input's top, with T-joins where the inset side rails meet
    /// the input's wider top edge.
    pub(super) fn queue_lines(&self) -> u16 {
        if self.queue.is_empty() {
            0
        } else {
            2 + self.queue.len() as u16
        }
    }

    pub(super) fn input_height(&self) -> u16 {
        let (term_w, _) = crossterm::terminal::size().unwrap_or((80, 24));
        // Inner content width = terminal width - 2 side rails.
        let wrap_width = (term_w as usize).saturating_sub(2).max(1);
        let prefix = input_prefix_width();
        let text = self.composer.text();
        let lines: Vec<&str> = if text.is_empty() {
            vec![""]
        } else {
            text.split('\n').collect()
        };
        let mut visual: usize = 0;
        for line in &lines {
            let visual_chars = prefix + line.chars().count();
            let n = if visual_chars == 0 {
                1
            } else {
                visual_chars.div_ceil(wrap_width)
            };
            visual = visual.saturating_add(n.max(1));
        }
        (visual as u16).clamp(MIN_INPUT_CONTENT, MAX_INPUT_CONTENT) + INPUT_BORDER
    }

    /// Elapsed time on the cumulative span clock, but only once the
    /// agent has been busy past the startup grace. `None` (→ indicator
    /// hidden) when idle or still inside the grace window.
    pub(super) fn status_span_elapsed(&self) -> Option<Duration> {
        if !self.busy {
            return None;
        }
        let elapsed = self.span_started_at?.elapsed();
        (elapsed >= STATUS_GRACE).then_some(elapsed)
    }

    /// 1 when the working indicator should occupy a row above the queue
    /// strip, else 0.
    pub(super) fn indicator_lines(&self) -> u16 {
        u16::from(self.status_span_elapsed().is_some())
    }

    /// Render the "agent is working" status indicator. Ground state is
    /// the playful working line (muted, span clock); it flips to a
    /// yellow `Thinking` override only while the current reasoning block
    /// has itself lasted past [`THINKING_FLIP_AFTER`], reading as
    /// "working" otherwise so there are no blank gaps after the grace
    /// period. No-op when the indicator shouldn't show.
    pub(super) fn render_status_indicator(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(span_elapsed) = self.status_span_elapsed() else {
            return;
        };
        let dots = thinking_dots_padded(self.started_at.elapsed().as_millis());
        let block_elapsed = self.pending.as_ref().map(|p| p.started_at.elapsed());
        let thinking =
            self.in_thinking_block() && block_elapsed.is_some_and(|e| e >= THINKING_FLIP_AFTER);

        let (label, elapsed, color) = if thinking {
            (
                "Thinking",
                block_elapsed.unwrap_or(span_elapsed),
                Color::Yellow,
            )
        } else {
            let msg = WORKING_MESSAGES
                .get(self.working_msg_idx)
                .copied()
                .unwrap_or("Working");
            (msg, span_elapsed, Color::Indexed(MUTED_COLOR_INDEX))
        };
        let text = format!("{label}{dots} {}", format_status_elapsed(elapsed));
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                text,
                Style::default().fg(color).add_modifier(Modifier::ITALIC),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    pub(super) fn total_history_lines(&self) -> u16 {
        // We can't perfectly compute the rendered line count without
        // the area width, but the history geometry caller doesn't have
        // that yet either. Approximate: 1 row per Plain, 3 rows per
        // User (padding + body + padding; multi-line bodies cost more
        // but for sizing this is fine), 2 rows per Agent, plus pending.
        let mut total: u16 = 0;
        let mut prev_agent = false;
        for entry in &self.history {
            total = total.saturating_add(match entry {
                HistoryEntry::Plain { .. } => 1,
                HistoryEntry::ToolLine { .. } => 2, // line + trailing gap
                HistoryEntry::ToolBox {
                    calls, expanded, ..
                } => toolbox_row_estimate(calls, *expanded).saturating_add(1),
                HistoryEntry::Diff { old, new, .. } => diff_row_estimate(old, new),
                HistoryEntry::User { text, .. } => {
                    let body = text.matches('\n').count() as u16 + 1;
                    // Bubble = top border + body + bottom border (+2);
                    // plus the trailing gap row inserted in render_history
                    // (+1) so the chat area gets sized to fit the box.
                    body.saturating_add(3)
                }
                HistoryEntry::Agent {
                    text,
                    reasoning,
                    expanded,
                    ..
                } => {
                    let body = text.matches('\n').count() as u16 + 1;
                    // When reasoning is collapsed, the chip shares the
                    // first text row (see render_agent), so no extra
                    // chip row to count. When expanded, +1 for chip
                    // plus all the reasoning lines.
                    let mut rows = body;
                    if !reasoning.trim().is_empty() && *expanded {
                        rows = rows.saturating_add(1);
                        rows = rows.saturating_add(reasoning.lines().count() as u16);
                    }
                    // Trailing gap row after agent — skipped when the
                    // previous entry was also an agent.
                    if !prev_agent {
                        rows = rows.saturating_add(1);
                    }
                    rows
                }
            });
            prev_agent = matches!(entry, HistoryEntry::Agent { .. });
        }
        if self.pending.is_some() {
            total = total.saturating_add(1);
        }
        total
    }

    pub(super) fn render(&mut self, frame: &mut ratatui::Frame) {
        let geom = self.geometry();
        let rects = geom.layout(frame.area());

        if let Some(prompt) = self.daemon_prompt.as_ref() {
            prompt.render(frame, rects.body);
        } else if self.dialog.is_active() {
            self.dialog.render(frame, rects.body);
        } else if let Some(picker) = self.model_picker.as_ref() {
            picker.render(frame, rects.body);
        } else {
            self.render_history(frame, rects.body);
            if geom.indicator > 0 {
                self.render_status_indicator(frame, rects.indicator);
            }
            if geom.queue > 0 {
                self.render_queue(frame, rects.queue);
            }
            let cursor_pos = self.render_input(frame, rects.input, geom.queue > 0);
            if geom.popup > 0 {
                self.render_popup(frame, rects.popup);
            }
            frame.set_cursor_position(cursor_pos);
        }
        self.render_status(frame, rects.status);

        // Toast sits on top of the status line. Rendered before the
        // context menu / text popup so those still cover it if both
        // happen to be active at the same time.
        if let Some(toast) = self.toast.clone() {
            render_toast(frame, rects.status, &toast);
        }

        // Context menu overlay renders LAST so it sits on top of
        // every other pane. The Clear widget inside the renderer
        // wipes the cells under the overlay so the chat / status
        // line don't bleed through.
        if let Some(menu) = self.context_menu.as_ref() {
            crate::tui::context_menu::render_context_menu(frame, frame.area(), menu);
        }
    }

    /// Queued-messages box. Inset one column from each side of the
    /// input box; rounded top corners (`╭ ╮`); white border throughout;
    /// shared bottom row with the input box rendered as `╭┴────┴╮`
    /// (input's rounded top corners with `┴` T-joins where the queue's
    /// inset side rails terminate). The shared row counts as the
    /// queue's bottom border AND the input's top border.
    pub(super) fn render_queue(&self, frame: &mut ratatui::Frame, area: Rect) {
        if area.height < 2 || area.width < 5 || self.queue.is_empty() {
            return;
        }
        // Border tracks the input box: dark grey for the whole span the
        // agent is busy (matches the "agent is working, hold off" cue on
        // the input border), white when idle. Indexed(238) — same shade
        // the input uses.
        let border_color = if self.busy {
            Color::Indexed(238)
        } else {
            Color::White
        };
        let dim_white = Color::Indexed(250);
        let outer_w = area.width as usize;
        // Queue is inset 1 col on each side; inside the inset, 1 col
        // is the rail and 1 col is padding before/after the text.
        let inset = 1usize;
        let queue_w = outer_w.saturating_sub(inset * 2);
        let inner_w = queue_w.saturating_sub(4); // 1 rail + 1 pad on each side
        let inner_w = inner_w.max(1);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);

        // Top row: `  ╭─────────╮  ` — rounded corners, inset.
        let top_bar = "─".repeat(queue_w.saturating_sub(2));
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(inset)),
            Span::styled(format!("╭{top_bar}╮"), Style::default().fg(border_color)),
            Span::raw(" ".repeat(inset)),
        ]));

        // Content rows: `  │ message │  `.
        for msg in &self.queue {
            let body = first_line_truncated(msg, inner_w);
            let body_w = body.chars().count();
            let trailing = inner_w.saturating_sub(body_w);
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(inset)),
                Span::styled("│", Style::default().fg(border_color)),
                Span::raw(" "),
                Span::styled(body, Style::default().fg(dim_white)),
                Span::raw(" ".repeat(trailing)),
                Span::raw(" "),
                Span::styled("│", Style::default().fg(border_color)),
                Span::raw(" ".repeat(inset)),
            ]));
        }

        // Shared bottom row: `╭┴────────┴╮`. Spans the full input
        // width — `╭` and `╮` at the corners (these are the input's
        // rounded top), and `┴` where the queue's inset side rails
        // terminate. The horizontal fills between use `─`.
        let mut shared: String = String::with_capacity(outer_w * 3);
        for col in 0..outer_w {
            let ch = if col == 0 {
                '╭'
            } else if col == outer_w - 1 {
                '╮'
            } else if col == inset {
                '┴'
            } else if col == outer_w - 1 - inset {
                '┴'
            } else {
                '─'
            };
            shared.push(ch);
        }
        lines.push(Line::from(vec![Span::styled(
            shared,
            Style::default().fg(border_color),
        )]));

        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Build the launch-banner box lines for the current pane, or an
    /// empty `Vec` when the banner is suppressed or doesn't fit. See
    /// [`crate::tui::banner_box`].
    fn banner_box_lines(&self, pane_w: u16, pane_h: u16) -> Vec<Line<'static>> {
        crate::tui::banner_box::build(&self.launch, pane_w, pane_h).unwrap_or_default()
    }

    pub(super) fn render_history(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.chat_area = Some(area);
        let area_h = area.height as usize;

        let mut all: Vec<Line<'static>> = Vec::new();
        // `targets[i]` carries the history-entry index whose thinking
        // chip occupies row `i` of `all`, or `None` otherwise. Only
        // the chip row toggles on click — body rows stay open for
        // drag-select.
        let mut targets: Vec<Option<usize>> = Vec::new();
        // `box_targets[i]` carries the history index of the `ToolBox`
        // that owns row `i` of `all`, else `None`. Drives wheel capture
        // (scroll the box, not the transcript) and click-to-expand.
        let mut box_targets: Vec<Option<usize>> = Vec::new();
        // `conts[i]` is `true` when row `i` of `all` is a soft-wrap
        // continuation of the prior logical line.
        let mut conts: Vec<bool> = Vec::new();
        for (idx, entry) in self.history.iter().enumerate() {
            let Rendered {
                lines,
                chip_row,
                continuations,
            } = render_entry(
                entry,
                area.width,
                self.thinking_setting,
                self.markdown_opts,
                self.diff_style,
                self.use_emojis,
            );
            let chip_abs = chip_row.map(|cr| all.len() + cr);
            let is_box = matches!(entry, HistoryEntry::ToolBox { .. });
            for i in 0..lines.len() {
                targets.push(if Some(all.len() + i) == chip_abs {
                    Some(idx)
                } else {
                    None
                });
                box_targets.push(if is_box { Some(idx) } else { None });
            }
            // Each entry's renderer returns one bool per emitted line;
            // pad if there's any mismatch (defensive — shouldn't
            // happen but keeps the parallel arrays in lockstep).
            let mut entry_conts = continuations;
            entry_conts.resize(lines.len(), false);
            conts.extend(entry_conts);
            all.extend(lines);
            // One-line gap after a block so it separates from what
            // follows. Consecutive agents share a block (no gap between).
            let gap = match entry {
                HistoryEntry::User { .. }
                | HistoryEntry::ToolBox { .. }
                | HistoryEntry::ToolLine { .. } => true,
                HistoryEntry::Agent { .. } => !idx
                    .checked_sub(1)
                    .map(|i| matches!(self.history[i], HistoryEntry::Agent { .. }))
                    .unwrap_or(false),
                _ => false,
            };
            if gap {
                all.push(Line::default());
                targets.push(None);
                box_targets.push(None);
                conts.push(false);
            }
        }
        if let Some(pending) = &self.pending {
            let dots = thinking_dots(self.started_at.elapsed().as_millis());
            let pending_lines = render_pending(pending, dots, area.width);
            for _ in 0..pending_lines.len() {
                targets.push(None);
                box_targets.push(None);
                conts.push(false);
            }
            all.extend(pending_lines);
        }

        // The launch-banner box is the topmost scroll entry (GOALS
        // §1g): it floats centered in an under-full pane and scrolls
        // off the top with the oldest messages once the transcript
        // grows tall enough to reach it.
        let box_lines = self.banner_box_lines(area.width, area.height);
        let b = box_lines.len();
        let m = all.len();

        // Total scrollable content height, box included — drives the
        // mouse-wheel scrollback clamp.
        self.chat_total_lines = b + m;
        self.chat_visible_lines = area_h;

        let (visible, visible_targets, visible_box, visible_conts): (
            Vec<Line<'static>>,
            Vec<Option<usize>>,
            Vec<Option<usize>>,
            Vec<bool>,
        ) = if b > 0 && b + m <= area_h {
            // Fits with room to spare: messages stay bottom-aligned and
            // the box floats at the vertical center, sliding up to sit
            // directly above the messages once they'd reach it. Content
            // fits, so there's no scrollback.
            self.chat_scroll_offset = 0;
            let centered_top = (area_h - b) / 2;
            let box_top = centered_top.min(area_h - m - b);
            let msg_top = area_h - m;
            let mut v: Vec<Line<'static>> = (0..area_h).map(|_| Line::default()).collect();
            let mut t: Vec<Option<usize>> = vec![None; area_h];
            let mut bx: Vec<Option<usize>> = vec![None; area_h];
            let mut c: Vec<bool> = vec![false; area_h];
            for (i, line) in box_lines.into_iter().enumerate() {
                v[box_top + i] = line;
            }
            for (i, line) in all.into_iter().enumerate() {
                v[msg_top + i] = line;
            }
            for (i, val) in targets.into_iter().enumerate() {
                t[msg_top + i] = val;
            }
            for (i, val) in box_targets.into_iter().enumerate() {
                bx[msg_top + i] = val;
            }
            for (i, val) in conts.into_iter().enumerate() {
                c[msg_top + i] = val;
            }
            (v, t, bx, c)
        } else {
            // No box, or box + messages overflow the pane: the box is
            // the top of one contiguous, bottom-aligned scroll buffer
            // and scrolls off the top with the oldest messages. Box rows
            // are non-interactive (None / false). With no box this is
            // exactly the previous behavior over `all`.
            let mut combined = box_lines;
            let prefix = combined.len();
            combined.extend(all);
            let mut ctargets = vec![None; prefix];
            ctargets.extend(targets);
            let mut cbox = vec![None; prefix];
            cbox.extend(box_targets);
            let mut cconts = vec![false; prefix];
            cconts.extend(conts);

            let total = combined.len();
            let max_offset = total.saturating_sub(area_h);
            if self.chat_scroll_offset > max_offset {
                self.chat_scroll_offset = max_offset;
            }

            if total < area_h {
                let pad = area_h - total;
                let mut v: Vec<Line<'static>> = (0..pad).map(|_| Line::default()).collect();
                let mut t: Vec<Option<usize>> = vec![None; pad];
                let mut bx: Vec<Option<usize>> = vec![None; pad];
                let mut c: Vec<bool> = vec![false; pad];
                v.extend(combined);
                t.extend(ctargets);
                bx.extend(cbox);
                c.extend(cconts);
                (v, t, bx, c)
            } else {
                let drop = total - area_h - self.chat_scroll_offset;
                let v: Vec<Line<'static>> = combined.into_iter().skip(drop).take(area_h).collect();
                let t: Vec<Option<usize>> = ctargets.into_iter().skip(drop).take(area_h).collect();
                let bx: Vec<Option<usize>> = cbox.into_iter().skip(drop).take(area_h).collect();
                let c: Vec<bool> = cconts.into_iter().skip(drop).take(area_h).collect();
                (v, t, bx, c)
            }
        };
        self.clickable_rows = visible_targets;
        self.box_rows = visible_box;
        self.chat_cont_rows = visible_conts;

        frame.render_widget(Paragraph::new(visible).wrap(Wrap { trim: false }), area);

        // Snapshot the rendered chat cells. We do this after the
        // paragraph widget has written into the frame buffer; the
        // grid we capture here is the source-of-truth for the copy
        // path (plan.md T8.f) — it survives wrap, multi-cell glyphs,
        // and the bottom-align padding because it reflects what the
        // user actually sees on screen.
        self.chat_text_grid = capture_grid(frame.buffer_mut(), area);

        // Apply the in-app selection highlight, if any. We mutate
        // cell styles on the same buffer the paragraph just wrote
        // to — the inverted bg lands underneath the next frame's
        // diff, exactly like a "real" selection.
        if let Some(sel) = self.selection {
            // Skip chip rows from highlight: visually, the
            // "▶ thought for Xs (ctrl+j to expand)" line is a
            // control affordance, not message content. Building
            // the bool mask here so apply_selection_highlight stays
            // a free function.
            let chip_row_mask: Vec<bool> =
                self.clickable_rows.iter().map(|t| t.is_some()).collect();
            apply_selection_highlight(
                frame.buffer_mut(),
                area,
                sel,
                &chip_row_mask,
                &self.chat_text_grid,
            );
        }
    }

    pub(super) fn render_input(
        &mut self,
        frame: &mut ratatui::Frame,
        area: Rect,
        queue_above: bool,
    ) -> Position {
        // Stash for the mouse handler so a click can route to
        // click-to-position-cursor (plan.md T8.d).
        self.input_area = Some(area);
        // When the queue strip is above, its shared bottom row IS our
        // top border — render only sides + bottom here.
        let borders = if queue_above {
            Borders::LEFT | Borders::RIGHT | Borders::BOTTOM
        } else {
            Borders::ALL
        };
        // Dark grey border for the whole span the agent is busy; white
        // when idle. Gated on `busy` (not `pending.is_some()`) so it
        // stays dim across reasoning, streaming, AND tool execution —
        // `pending` drops to `None` between tool rounds, which used to
        // flicker the border white mid-turn. We use a darker grey than
        // MUTED_COLOR_INDEX so the "agent is working, hold off typing"
        // signal reads at a glance against the surrounding chrome.
        let border_color = if self.busy {
            Color::Indexed(238)
        } else {
            Color::White
        };
        let input_block = Block::default()
            .borders(borders)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color));
        let input_inner = input_block.inner(area);

        let prefix_width = input_prefix_width();
        let indent: String = " ".repeat(prefix_width);
        let text = self.composer.text();
        let buf_lines: Vec<&str> = if text.is_empty() {
            vec![""]
        } else {
            text.split('\n').collect()
        };
        // Pre-wrap the composer text ourselves so the rendered visual
        // rows match what `cursor_visual_pos` assumes — `Paragraph::
        // wrap`'s word-wrap algorithm doesn't have a clean way to
        // report the cursor's position back to us, so the two sides
        // would otherwise drift apart on wrapped input.
        let inner_w = input_inner.width as usize;
        let budget = inner_w.saturating_sub(prefix_width).max(1);
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (li, line) in buf_lines.iter().enumerate() {
            let line_chars: Vec<char> = line.chars().collect();
            let chunks = wrap_logical_line_chunks(line, budget);
            for (ci, (start, end)) in chunks.iter().enumerate() {
                let chunk_text: String = line_chars[*start..*end].iter().collect();
                let pre = if li == 0 && ci == 0 {
                    INPUT_PREFIX
                } else {
                    indent.as_str()
                };
                lines.push(Line::from(vec![
                    Span::styled(pre.to_string(), Style::default().fg(Color::White)),
                    Span::styled(chunk_text, Style::default().fg(Color::White)),
                ]));
            }
        }

        let (cursor_line, cursor_col) = self.composer.cursor_line_col();
        let (vis_row, vis_col) = cursor_visual_pos(
            self.composer.text(),
            cursor_line,
            cursor_col,
            prefix_width,
            inner_w.max(1),
        );
        let cursor_row = vis_row as u16;
        let cursor_col = vis_col as u16;

        let visible_rows = input_inner.height;
        let scroll_y = cursor_row.saturating_sub(visible_rows.saturating_sub(1));
        // No `Wrap` modifier — the lines we just emitted are already
        // visual rows. Letting Paragraph::wrap re-wrap them would
        // desync the cursor again.
        let para = Paragraph::new(lines)
            .block(input_block)
            .scroll((scroll_y, 0));
        frame.render_widget(para, area);

        // Context indicator on the top-right of the input box. Only
        // shown when the composer is empty so it doesn't fight with
        // the text the user is typing. Light grey, right-aligned to
        // the inner edge.
        if self.composer.text().is_empty() {
            let label = self.context_indicator_text();
            let label_w = label.chars().count() as u16;
            if label_w + 1 < input_inner.width {
                let x = input_inner.x + input_inner.width.saturating_sub(label_w);
                let chip_area = Rect::new(x, input_inner.y, label_w, 1);
                let chip = Paragraph::new(Line::from(vec![Span::styled(
                    label,
                    Style::default().fg(Color::Indexed(250)),
                )]));
                frame.render_widget(chip, chip_area);
            }
        }

        Position::new(
            input_inner.x + cursor_col,
            input_inner.y + cursor_row.saturating_sub(scroll_y),
        )
    }

    /// Build the chrome's context indicator. Format:
    /// - With known max:   `12% context (max 192k), 0% prunable`
    /// - Without:          `1.2k tokens, 0% prunable`
    /// `prunable` is a placeholder zero until the pruning estimator
    /// (plan §10) lands.
    pub(super) fn context_indicator_text(&self) -> String {
        // Fresh chat (nothing sent, no provider usage yet): replace the
        // useless `0% prunable` placeholder with the instruction-file
        // size the daemon estimated. Reverts to the usual form once the
        // first round-trip returns usage or any history exists. No
        // guidance file → fall through to the usual form entirely.
        if let Some(label) = fresh_chat_guidance_label(
            self.history.is_empty(),
            self.last_usage.is_some(),
            self.guidance_estimate.as_ref(),
        ) {
            return label;
        }
        let tokens = self.context_tokens();
        let prunable = 0u32; // placeholder
        match self.launch.active_model_max_context {
            Some(max) if max > 0 => {
                let pct = ((tokens as u64 * 100) / max as u64).min(999) as u32;
                let k = max / 1000;
                format!("{pct}% context (max {k}k), {prunable}% prunable")
            }
            _ => format!(
                "{} tokens, {prunable}% prunable",
                format_token_count(tokens)
            ),
        }
    }

    /// Best available token count for the current context. Prefers the
    /// provider's `input + output` from the most recent round-trip
    /// (authoritative for what the model actually saw + produced) and
    /// falls back to the local tiktoken estimate over visible history
    /// when no provider count is available yet.
    pub(super) fn context_tokens(&self) -> u32 {
        if let Some(u) = self.last_usage {
            return u.total().min(u32::MAX as u64) as u32;
        }
        self.estimate_context_tokens()
    }

    /// cl100k_base token count over visible chat content. Tools and
    /// system prompts aren't included — they live on the engine side.
    /// Provider-native counts will replace this where available
    /// (GOALS §10 / plan §3h); cl100k_base is the documented fallback.
    pub(super) fn estimate_context_tokens(&self) -> u32 {
        let mut tokens: usize = 0;
        for entry in &self.history {
            tokens += match entry {
                HistoryEntry::User { text, .. } => crate::tokens::count(text),
                HistoryEntry::Plain { line } => crate::tokens::count(line),
                HistoryEntry::ToolLine { summary, .. } => crate::tokens::count(summary),
                HistoryEntry::ToolBox { calls, .. } => calls
                    .iter()
                    .map(|c| crate::tokens::count(&c.summary) + crate::tokens::count(&c.output))
                    .sum(),
                HistoryEntry::Diff { old, new, .. } => {
                    crate::tokens::count(old) + crate::tokens::count(new)
                }
                HistoryEntry::Agent {
                    text, reasoning, ..
                } => crate::tokens::count(text) + crate::tokens::count(reasoning),
            };
        }
        if let Some(p) = &self.pending {
            tokens += crate::tokens::count(&p.text) + crate::tokens::count(&p.reasoning);
        }
        tokens.min(u32::MAX as usize) as u32
    }

    pub(super) fn render_popup(&self, frame: &mut ratatui::Frame, area: Rect) {
        // `@`-popup takes precedence over the vim hint when active.
        if self.at_popup_active() {
            self.render_at_popup(frame, area);
            return;
        }
        // Vim hint preempts the popup when the composer is in Normal
        // mode and the user hasn't opted out via the vim_mode setting.
        if self.slash_query().is_none() {
            if self.show_vim_hint() {
                let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
                let line = Line::from(vec![
                    Span::raw("  "),
                    Span::styled("Press ", muted),
                    Span::styled("`i`", Style::default().fg(Color::Yellow)),
                    Span::styled(" to resume typing. Disable vim mode in ", muted),
                    Span::styled("/settings", muted),
                ]);
                frame.render_widget(Paragraph::new(line), area);
            }
            return;
        }
        let query = self.slash_query().unwrap_or("");
        let mut matches = slash_matches(query, &self.usage_slash);
        // Cap to the autocomplete-rows budget; pad blanks below so the
        // popup keeps a stable 6-row footprint regardless of match count.
        matches.truncate(AUTOCOMPLETE_ROWS as usize);
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));

        let mut lines: Vec<Line<'static>> = if matches.is_empty() {
            vec![Line::from(vec![
                Span::raw("  "),
                Span::styled("no matching command", Style::default().fg(Color::Red)),
            ])]
        } else {
            let name_w = matches.iter().map(|c| c.name.len()).max().unwrap_or(0);
            matches
                .iter()
                .enumerate()
                .map(|(i, cmd)| {
                    let is_best = i == 0;
                    let marker = if is_best { "▸ " } else { "  " };
                    let name_padded = format!("/{:<width$}", cmd.name, width = name_w);
                    let name_style = if is_best {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    Line::from(vec![
                        Span::raw(marker),
                        Span::styled(name_padded, name_style),
                        Span::raw("  "),
                        Span::styled(cmd.description.to_string(), muted),
                    ])
                })
                .collect()
        };
        while (lines.len() as u16) < AUTOCOMPLETE_ROWS {
            lines.push(Line::default());
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    pub(super) fn render_at_popup(&self, frame: &mut ratatui::Frame, area: Rect) {
        let suggestions = self.at_suggestions();
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let mut lines: Vec<Line<'static>> = if suggestions.is_empty() {
            vec![Line::from(vec![
                Span::raw("  "),
                Span::styled("no matching file", Style::default().fg(Color::Red)),
            ])]
        } else {
            let window = AUTOCOMPLETE_ROWS as usize;
            let selected = self.at_selected.min(suggestions.len().saturating_sub(1));
            // Clamp the stored scroll offset defensively (the list can
            // shrink between a keypress and this render).
            let offset = crate::tui::app::windowed_scroll(
                selected,
                self.at_scroll,
                suggestions.len(),
                window,
            );
            suggestions
                .iter()
                .enumerate()
                .skip(offset)
                .take(window)
                .map(|(i, sug)| {
                    let is_sel = i == selected;
                    let marker = if is_sel { "▸ " } else { "  " };
                    let name_style = if is_sel {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    let kind = if sug.is_dir { "dir" } else { "file" };
                    Line::from(vec![
                        Span::raw(marker),
                        Span::styled(format!("@{}", sug.display), name_style),
                        Span::raw("  "),
                        Span::styled(kind.to_string(), muted),
                    ])
                })
                .collect()
        };
        while (lines.len() as u16) < AUTOCOMPLETE_ROWS {
            lines.push(Line::default());
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    pub(super) fn render_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        let right = chrome::status_line_spans(&self.launch);
        let left = chrome::left_status_spans(&self.launch);
        let right_width: u16 = right
            .iter()
            .map(|s| s.width() as u16)
            .sum::<u16>()
            .min(area.width);
        let bottom =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).split(area);
        frame.render_widget(Paragraph::new(Line::from(left)), bottom[0]);
        frame.render_widget(Paragraph::new(Line::from(right)), bottom[1]);
    }
}

/// Render a toast over the status-line rect. Single line; left-padded
/// one cell; foreground color encodes intent (green/red/grey).
/// Uses `Clear` so the status text underneath doesn't bleed through.
fn render_toast(frame: &mut ratatui::Frame, status_rect: Rect, toast: &Toast) {
    use ratatui::widgets::Clear;
    if status_rect.height == 0 || status_rect.width == 0 {
        return;
    }
    let fg = match toast.kind {
        ToastKind::Success => Color::Green,
        ToastKind::Error => Color::Red,
        ToastKind::Info => Color::Indexed(250),
    };
    let text = format!(" {} ", toast.text);
    // Truncate to fit if the message is longer than the status row.
    let max = status_rect.width as usize;
    let display: String = if text.chars().count() > max {
        let cap = max.saturating_sub(1);
        let truncated: String = text.chars().take(cap).collect();
        format!("{truncated}…")
    } else {
        text
    };
    frame.render_widget(Clear, status_rect);
    let para = Paragraph::new(Line::from(Span::styled(
        display,
        Style::default().fg(fg).add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(para, status_rect);
}

/// Snapshot the chat-area cells into a `(row, col) → symbol` grid so
/// the copy path can reconstruct selected plaintext without redoing
/// ratatui's wrap. Run after `frame.render_widget(...)` so the cells
/// reflect what the user actually sees.
fn capture_grid(buf: &ratatui::buffer::Buffer, area: Rect) -> Vec<Vec<String>> {
    let mut grid = Vec::with_capacity(area.height as usize);
    for y in 0..area.height {
        let mut row = Vec::with_capacity(area.width as usize);
        for x in 0..area.width {
            let abs_x = area.x + x;
            let abs_y = area.y + y;
            if let Some(cell) = buf.cell((abs_x, abs_y)) {
                row.push(cell.symbol().to_string());
            } else {
                row.push(String::new());
            }
        }
        grid.push(row);
    }
    grid
}

/// Apply the drag-select highlight to the chat area. Invert each
/// selected cell's fg/bg via the `REVERSED` modifier — same visual
/// affordance terminal selection uses, and it survives any underlying
/// color theme.
///
/// Highlights only the *content range* of each row: from the first
/// non-whitespace cell to the last non-whitespace cell. Cells outside
/// that range (left/right padding, end-of-line gap) stay un-inverted.
/// In-content spaces (between words) are highlighted so the selection
/// reads as a continuous bar rather than a gappy one. Chip rows
/// (`chip_row_mask`) are skipped entirely.
fn apply_selection_highlight(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    sel: Selection,
    chip_row_mask: &[bool],
    chat_text_grid: &[Vec<String>],
) {
    let (start, end) = sel.ordered();
    let left = area.x;
    let right = area.x + area.width.saturating_sub(1);
    let top = area.y;
    let bottom = area.y + area.height.saturating_sub(1);
    for row in start.1..=end.1 {
        if row < top || row > bottom {
            continue;
        }
        let chat_rel = row.saturating_sub(area.y) as usize;
        if chip_row_mask.get(chat_rel).copied().unwrap_or(false) {
            continue;
        }
        let Some(grid_row) = chat_text_grid.get(chat_rel) else {
            continue;
        };
        let Some((content_first, content_last)) = content_bounds(grid_row) else {
            // Row is entirely whitespace (bottom-align padding,
            // blank gap) — nothing to highlight.
            continue;
        };
        let sel_first = if row == start.1 { start.0 } else { left };
        let sel_last = if row == end.1 { end.0 } else { right };
        let content_first_abs = (area.x as usize + content_first) as u16;
        let content_last_abs = (area.x as usize + content_last) as u16;
        let highlight_first = sel_first.max(content_first_abs);
        let highlight_last = sel_last.min(content_last_abs);
        if highlight_first > highlight_last {
            continue;
        }
        for col in highlight_first..=highlight_last {
            if let Some(cell) = buf.cell_mut((col, row)) {
                let mut style = cell.style();
                style = style.add_modifier(ratatui::style::Modifier::REVERSED);
                cell.set_style(style);
            }
        }
    }
}

/// `(first_content_col, last_content_col)` for a row of the chat
/// grid, or `None` if the row is entirely whitespace. Used by the
/// highlight pass to draw the inversion only across content cells.
fn content_bounds(row: &[String]) -> Option<(usize, usize)> {
    let first = row
        .iter()
        .position(|c| !c.chars().all(|ch| ch.is_whitespace()))?;
    let last = row
        .iter()
        .rposition(|c| !c.chars().all(|ch| ch.is_whitespace()))?;
    Some((first, last))
}

/// Extract the plaintext under the active drag-selection from the
/// cached chat grid. Walks the selection in reading order: first row
/// from start.col to row-end, full intermediate rows, last row from
/// row-start to end.col.
///
/// Two refinements on top of the cell-by-cell extraction:
///
/// 1. **Strip the agent-message left padding.** Each row gets at most
///    `AGENT_INDENT` leading spaces removed, preserving any *extra*
///    indent (code-block indentation, list nesting) above that base.
///    `\u{a0}` (NBSP) is intentionally preserved because that's a
///    user-meaningful character.
/// 2. **Soft-wrap rejoin.** When a row is a continuation of the
///    previous logical line (per `cont_rows`), join it with a space
///    instead of a newline so a wrapped paragraph pastes as one
///    paragraph, not a stack of short visual lines. Hard line breaks
///    (paragraph boundaries) still produce newlines.
pub(super) fn extract_selection_plaintext(
    grid: &[Vec<String>],
    cont_rows: &[bool],
    area: Rect,
    sel: Selection,
) -> String {
    use crate::tui::history::AGENT_INDENT;
    let (start, end) = sel.ordered();
    let mut out = String::new();
    let mut first_emitted = true;
    for abs_row in start.1..=end.1 {
        let grid_row = abs_row.saturating_sub(area.y) as usize;
        let Some(row) = grid.get(grid_row) else {
            continue;
        };
        let first_col = if abs_row == start.1 {
            start.0.saturating_sub(area.x) as usize
        } else {
            0
        };
        let last_col = if abs_row == end.1 {
            end.0.saturating_sub(area.x) as usize
        } else {
            row.len().saturating_sub(1)
        };
        let mut line = String::new();
        for col in first_col..=last_col {
            if let Some(symbol) = row.get(col) {
                line.push_str(symbol);
            }
        }
        // Drop trailing spaces — bottom-align padding and end-of-line
        // gaps would otherwise turn into ugly trailing whitespace.
        let trimmed = line.trim_end_matches(' ').to_string();
        // Strip up to AGENT_INDENT leading spaces. Extra indent
        // (code blocks, nested lists) survives.
        let leading_spaces = trimmed.chars().take_while(|c| *c == ' ').count();
        let strip = leading_spaces.min(AGENT_INDENT);
        let stripped: String = trimmed.chars().skip(strip).collect();
        // Join: space for soft-wrap continuations, newline for hard
        // line boundaries. First emitted row never gets a leading
        // separator.
        if first_emitted {
            first_emitted = false;
        } else {
            let is_continuation = cont_rows.get(grid_row).copied().unwrap_or(false);
            out.push(if is_continuation { ' ' } else { '\n' });
        }
        out.push_str(&stripped);
    }
    out
}

/// Rough row count for a history entry. Mirrors the breakdown in
/// `total_history_lines` so the spill math is consistent.
fn entry_rendered_rows(entry: &HistoryEntry) -> u16 {
    match entry {
        HistoryEntry::Plain { .. } => 1,
        HistoryEntry::ToolLine { .. } => 1,
        HistoryEntry::ToolBox {
            calls, expanded, ..
        } => toolbox_row_estimate(calls, *expanded),
        HistoryEntry::Diff { old, new, .. } => diff_row_estimate(old, new),
        HistoryEntry::User { text, .. } => (text.matches('\n').count() as u16 + 1) + 2,
        HistoryEntry::Agent {
            text,
            reasoning,
            expanded,
            ..
        } => {
            let mut rows = text.matches('\n').count() as u16 + 1;
            if !reasoning.trim().is_empty() {
                rows = rows.saturating_add(1);
                if *expanded {
                    rows = rows.saturating_add(reasoning.lines().count() as u16);
                }
            }
            rows
        }
    }
}

/// Plain-text projection of an entry, one string per logical row.
/// Used when spilling into terminal scrollback.
/// `1234 → "1.2k"`, `820 → "820"`. For the context indicator when no
/// max-context is known.
fn format_token_count(n: u32) -> String {
    if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// The fresh-chat context-indicator label (`X tokens in <file>`), or
/// `None` to fall back to the normal context display. Shown only on a
/// truly fresh chat — no history and no provider usage yet — and only
/// when the daemon found a guidance file. Pure so the trigger/revert
/// logic is unit-testable without standing up an `App`.
fn fresh_chat_guidance_label(
    history_empty: bool,
    has_usage: bool,
    estimate: Option<&(String, u64)>,
) -> Option<String> {
    if !history_empty || has_usage {
        return None;
    }
    let (file, tokens) = estimate?;
    let n = (*tokens).min(u32::MAX as u64) as u32;
    Some(format!("{} tokens in {file}", format_token_count(n)))
}

/// First line of `s`, hard-clipped to `width` columns with a trailing
/// `…` when truncated. Used by the queue strip; only previews the first
/// line of multi-line queued messages to keep the box compact.
fn first_line_truncated(s: &str, width: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    if width == 0 {
        return String::new();
    }
    if first.chars().count() <= width {
        return first.to_string();
    }
    let mut out: String = first.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Visual (row, col) of the cursor inside the input box's inner area,
/// accounting for soft-wrap. `wrap_width` is the inner width; `prefix`
/// is the width of the leading `❯ ` on the first logical line (a
/// matching indent is used on subsequent logical lines, so the wrap
/// math is symmetric).
/// Compute the cursor's visual `(row, col)` inside the composer's
/// inner rect. Both the renderer and this function rely on
/// [`wrap_logical_line_chunks`] for the wrap algorithm, so they're
/// guaranteed to agree on where each character lands.
fn cursor_visual_pos(
    text: &str,
    cursor_line: usize,
    cursor_col: usize,
    prefix: usize,
    wrap_width: usize,
) -> (usize, usize) {
    if wrap_width == 0 {
        return (0, 0);
    }
    let budget = wrap_width.saturating_sub(prefix).max(1);
    let lines: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.split('\n').collect()
    };
    let mut visual_row: usize = 0;
    for (li, line) in lines.iter().enumerate() {
        let chunks = wrap_logical_line_chunks(line, budget);
        if li < cursor_line {
            visual_row += chunks.len();
            continue;
        }
        // On the cursor's logical line — locate the chunk containing
        // the cursor and return its visual position.
        for (ci, (start, end)) in chunks.iter().enumerate() {
            let is_last = ci + 1 == chunks.len();
            let contains = if is_last {
                cursor_col >= *start && cursor_col <= *end
            } else {
                cursor_col >= *start && cursor_col < *end
            };
            if contains {
                let col_within = cursor_col - start;
                return (visual_row + ci, prefix + col_within);
            }
        }
        // Defensive fallback — cursor past the last chunk.
        let (last_start, last_end) = *chunks.last().expect("chunks non-empty");
        return (
            visual_row + chunks.len().saturating_sub(1),
            prefix + (last_end - last_start),
        );
    }
    (visual_row, prefix)
}

/// Greedy word-aware wrap of a single logical line into char-range
/// chunks. Each `(start, end)` is a half-open range into the line's
/// chars; the chunks tile the entire line (`end[i] == start[i+1]`)
/// so cursor positions map back deterministically. Breaks at the
/// last space inside `budget` if there is one; falls back to a
/// hard cut at `budget` for unbreakable tokens.
fn wrap_logical_line_chunks(line: &str, budget: usize) -> Vec<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![(0, 0)];
    }
    if budget == 0 {
        return vec![(0, chars.len())];
    }
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < chars.len() {
        let remaining = chars.len() - idx;
        if remaining <= budget {
            out.push((idx, chars.len()));
            break;
        }
        let max_end = idx + budget;
        let break_at = (idx + 1..=max_end)
            .rev()
            .find(|&i| i > 0 && chars[i - 1] == ' ')
            .unwrap_or(max_end);
        out.push((idx, break_at));
        idx = break_at;
    }
    if out.is_empty() {
        out.push((0, 0));
    }
    out
}

/// True for tools that take an `old_string` / `new_string` pair we
/// can render as a diff. `write` / `writeunlock` aren't in here yet
/// because the engine doesn't surface the pre-write file content (see
/// `flagged-for-christopher.md`).
pub(super) fn is_edit_tool(tool: &str) -> bool {
    matches!(tool, "edit" | "editunlock")
}

/// Approximate row count for a `Diff` entry, used by the chat-pane
/// sizing math. SideBySide ≈ max(old, new); Inline ≈ old + new. The
/// chat sizer doesn't know which mode is active at this point, so
/// we use the inline (upper-bound) estimate to avoid undersized
/// panes — slight over-allocation is cheaper than clipping.
pub(super) fn diff_row_estimate(old: &str, new: &str) -> u16 {
    let old_lines = old.matches('\n').count() as u16 + 1;
    let new_lines = new.matches('\n').count() as u16 + 1;
    old_lines.saturating_add(new_lines).saturating_add(1) // +1 for header
}

/// Approximate rendered row count for a `ToolBox`. Collapsed caps at
/// [`crate::tui::history::TOOLBOX_VISIBLE`]; expanded sums each call's
/// input + (non-empty) output lines. Mirrors `render_toolbox`.
pub(super) fn toolbox_row_estimate(calls: &[crate::tui::history::ToolCall], expanded: bool) -> u16 {
    use crate::tui::history::TOOLBOX_VISIBLE;
    if !expanded {
        return (calls.len().min(TOOLBOX_VISIBLE).max(1)) as u16;
    }
    let mut rows: u16 = 0;
    for c in calls {
        rows = rows.saturating_add(c.full_input.matches('\n').count() as u16 + 1);
        if !c.output.is_empty() {
            rows = rows.saturating_add(c.output.matches('\n').count() as u16 + 1);
        }
    }
    rows.max(1)
}

#[cfg(test)]
mod guidance_label_tests {
    use super::fresh_chat_guidance_label;

    #[test]
    fn shows_on_fresh_chat_with_estimate() {
        let est = ("AGENTS.md".to_string(), 1234u64);
        let label = fresh_chat_guidance_label(true, false, Some(&est));
        assert_eq!(label.as_deref(), Some("1.2k tokens in AGENTS.md"));
    }

    #[test]
    fn reverts_once_history_or_usage_exists() {
        let est = ("AGENTS.md".to_string(), 1234u64);
        // History present → revert.
        assert!(fresh_chat_guidance_label(false, false, Some(&est)).is_none());
        // Usage reported → revert.
        assert!(fresh_chat_guidance_label(true, true, Some(&est)).is_none());
    }

    #[test]
    fn no_guidance_file_falls_back() {
        assert!(fresh_chat_guidance_label(true, false, None).is_none());
    }
}
