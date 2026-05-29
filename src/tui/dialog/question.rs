//! Question-tool wiring over the reusable [`DialogState`] (GOALS §3b).
//!
//! This is the thin, use-case-specific layer the spec calls for: it
//! translates the daemon's [`InterruptQuestionSet`] into dialog
//! [`Page`]s, drives the shared state machine for input, renders the
//! dialog as a compact bottom-anchored overlay above the status row
//! (codex bottom-pane style), and maps the resulting [`Answer`]s back to
//! the proto [`ResolveResponse`]s the `question` tool expects. The
//! approval prompt reuses [`DialogState`] unchanged via its own thin
//! wrapper.

use std::time::Duration;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use crate::daemon::proto::{
    InterruptOption, InterruptQuestion, InterruptQuestionSet, ResolveResponse,
};
use crate::tui::dialog::{Answer, DialogOption, DialogOutcome, DialogState, Page, PageKind};
use crate::tui::theme::{ACCENT_BLUE_INDEX, MUTED_COLOR_INDEX};

/// Codex-style cap on visible option rows. Longer lists scroll, keeping
/// the focused row in view, instead of clipping.
const MAX_VISIBLE_OPTION_ROWS: usize = 8;

/// Hard ceiling on the compact overlay's height (rows, incl. border +
/// footer) so a giant question can't eat the whole screen. The dialog
/// sizes to content up to this; beyond it the option list scrolls.
const MAX_DIALOG_HEIGHT: u16 = 16;

const CUSTOM_LABEL: &str = "Type your own answer";
const NEXT_LABEL: &str = "Next";

/// Leading hover/cursor glyph on every option row: "▸ " when focused,
/// two spaces otherwise. Both render two cells wide, so the column a row's
/// content starts at is fixed regardless of focus.
const OPTION_CURSOR_HOVERED: &str = "▸ ";
const OPTION_CURSOR_PLAIN: &str = "  ";
/// Rendered width (terminal cells) of the leading cursor glyph. Used to
/// park the real terminal cursor by display column rather than byte length
/// (the hover glyph is multi-byte UTF-8).
const OPTION_CURSOR_WIDTH: usize = 2;

/// What the host should do once the dialog closes.
#[derive(Debug, Clone)]
pub enum QuestionResult {
    /// Send these resolutions back to the daemon for `interrupt_id`.
    Submit {
        interrupt_id: Uuid,
        responses: Vec<ResolveResponse>,
    },
    /// User dismissed: resolve as a cancel.
    Cancel { interrupt_id: Uuid },
}

/// The App-facing question dialog overlay. Owns a [`DialogState`] plus
/// the bits the resolution needs (the interrupt id and the original
/// questions, so option ids map correctly even for select free-text) and
/// the interrupt-level context header.
pub struct QuestionDialog {
    interrupt_id: Uuid,
    /// Interrupt-level context (from `raise_interrupt(description, …)`),
    /// rendered as a muted/italic context header. Empty = omit.
    description: String,
    questions: Vec<InterruptQuestion>,
    state: DialogState,
    result: Option<QuestionResult>,
}

impl QuestionDialog {
    /// Build the dialog for a raised interrupt. `description` is the
    /// interrupt-level context header (empty to omit). `lockout` is the
    /// configured anti-misfire delay (default 1.5s).
    pub fn new(
        interrupt_id: Uuid,
        description: String,
        set: InterruptQuestionSet,
        lockout: Duration,
    ) -> Self {
        let pages = set.questions.iter().map(page_for).collect();
        let state = DialogState::new(pages, lockout);
        Self {
            interrupt_id,
            description,
            questions: set.questions,
            state,
            result: None,
        }
    }

    /// Drain the close result once `handle_key` returned `true`.
    pub fn take_result(&mut self) -> Option<QuestionResult> {
        self.result.take()
    }

    /// Route a key. Returns `true` when the dialog wants to close (the
    /// host then drains [`take_result`](Self::take_result)).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        match self.state.handle_key(key) {
            DialogOutcome::Continue => false,
            DialogOutcome::Cancel => {
                self.result = Some(QuestionResult::Cancel {
                    interrupt_id: self.interrupt_id,
                });
                true
            }
            DialogOutcome::Submit(answers) => {
                let responses = answers
                    .iter()
                    .zip(self.questions.iter())
                    .map(|(a, q)| answer_to_response(a, q))
                    .collect();
                self.result = Some(QuestionResult::Submit {
                    interrupt_id: self.interrupt_id,
                    responses,
                });
                true
            }
        }
    }

    /// Content-sized height (rows) the bottom-anchored overlay wants,
    /// capped at [`MAX_DIALOG_HEIGHT`]. Drives the geometry: history fills
    /// the space above. Includes the top+bottom border and the footer row.
    pub fn desired_height(&self) -> u16 {
        // 1 row each: top border, bottom border, footer hint.
        let chrome: u16 = 3;
        let body = self.body_line_count();
        // Floor of 4 (border x2 + 1 prompt + 1 footer) ≤ MAX_DIALOG_HEIGHT,
        // so the clamp bounds are well-ordered.
        chrome.saturating_add(body).clamp(4, MAX_DIALOG_HEIGHT)
    }

    /// Number of body lines the current view wants (before capping). The
    /// option list is capped at [`MAX_VISIBLE_OPTION_ROWS`] rows worth of
    /// lines so a long list scrolls rather than inflating the overlay.
    fn body_line_count(&self) -> u16 {
        let mut lines = 0usize;
        // Context header (+ blank separator) when present.
        if !self.description.trim().is_empty() {
            lines += 1 + 1;
        }
        if self.state.on_confirm_page() {
            // Title + blank + one row per question + blank + status row.
            lines += 1 + 1 + self.questions.len() + 1 + 1;
            return (lines as u16).max(1);
        }
        // Prompt + blank separator.
        lines += 1 + 1;
        let page_idx = self.state.current_page();
        let page = &self.state.pages()[page_idx];
        match page.kind {
            PageKind::Text => {
                // The single input row.
                lines += 1;
            }
            PageKind::Select | PageKind::Multiselect => {
                // Visible option/custom/Next rows are line-accounted and
                // capped; descriptions add continuation lines.
                lines += self.visible_body_lines(page_idx, page);
            }
        }
        (lines as u16).max(1)
    }

    /// Lines the visible portion of a select/multiselect row list spans,
    /// capping the number of *rows* shown at [`MAX_VISIBLE_OPTION_ROWS`]
    /// and counting each row's continuation lines (descriptions).
    fn visible_body_lines(&self, page_idx: usize, page: &Page) -> usize {
        let rows = self.row_line_counts(page_idx, page);
        let total_rows = rows.len();
        let scroll = self.state.scroll().min(total_rows);
        let shown = MAX_VISIBLE_OPTION_ROWS.min(total_rows.saturating_sub(scroll));
        rows[scroll..scroll + shown].iter().copied().sum()
    }

    /// Per-row line count for the current page's row list (options, then
    /// the custom affordance, then the multiselect "Next" entry). A row
    /// is one line plus one per description line.
    fn row_line_counts(&self, _page_idx: usize, page: &Page) -> Vec<usize> {
        let mut counts: Vec<usize> = page
            .options
            .iter()
            .map(|o| 1 + o.description.as_deref().map(|_| 1).unwrap_or(0))
            .collect();
        // Custom affordance: one row (its typed text shares the row).
        counts.push(1);
        // Multiselect "Next" entry.
        if page.next_index().is_some() {
            counts.push(1);
        }
        counts
    }

    /// Sync the shared scroll state with the real available height before
    /// a render. Computes how many option rows fit in the body's line
    /// budget and feeds it to the core so the focused row stays in view.
    /// No-op on text / confirm pages (no scrollable option list).
    pub fn sync_viewport(&mut self, area_height: u16) {
        if self.state.on_confirm_page() {
            self.state.set_viewport(0);
            return;
        }
        let page_idx = self.state.current_page();
        let page = self.state.pages()[page_idx].clone();
        if matches!(page.kind, PageKind::Text) {
            self.state.set_viewport(0);
            return;
        }
        // Lines available for the option list = body minus chrome (border
        // x2, footer) minus the header/prompt lines.
        let mut overhead: u16 = 3; // borders + footer
        if !self.description.trim().is_empty() {
            overhead = overhead.saturating_add(2);
        }
        overhead = overhead.saturating_add(2); // prompt + blank
        let budget = area_height.saturating_sub(overhead) as usize;
        let rows = self.row_line_counts(page_idx, &page);
        // How many leading rows (from the focused window) fit in `budget`
        // lines, capped at the codex row cap.
        let mut fit = 0usize;
        let mut used = 0usize;
        for &c in rows.iter().take(MAX_VISIBLE_OPTION_ROWS) {
            if used + c > budget && fit > 0 {
                break;
            }
            used += c;
            fit += 1;
        }
        self.state.set_viewport(fit.max(1));
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        // Anti-misfire lockout: grey border while locked, white once
        // interactive.
        let locked = self.state.locked();
        let border_color = if locked {
            Color::Indexed(MUTED_COLOR_INDEX)
        } else {
            Color::White
        };
        let title = if self.state.page_count() > 1 {
            let n = self.state.page_count();
            let cur = (self.state.current_page() + 1).min(n + 1);
            if self.state.on_confirm_page() {
                " question · review ".to_string()
            } else {
                format!(" question · {cur}/{n} ")
            }
        } else {
            " question ".to_string()
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

        let (lines, cursor) = if self.state.on_confirm_page() {
            (self.render_confirm(), None)
        } else {
            self.render_page(layout[0])
        };
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), layout[0]);

        let hint = if locked {
            "waiting…".to_string()
        } else {
            self.footer_hint()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX)),
            ))),
            layout[1],
        );

        // Park the real terminal cursor at the active input position
        // (freetext / custom field), once the lockout has cleared.
        if let Some((x, y)) = cursor
            && !locked
        {
            frame.set_cursor_position(Position::new(x, y));
        }
    }

    fn footer_hint(&self) -> String {
        if self.state.is_typing() {
            return "type your answer  ·  enter: done  ·  esc: cancel".to_string();
        }
        if self.state.on_confirm_page() {
            return "enter: submit  ·  ←/h: back  ·  esc: cancel".to_string();
        }
        let multi = self.state.page_count() > 1;
        let nav = if multi {
            "  ·  ←/→: questions"
        } else {
            ""
        };
        let pick = if self.state.next_index().is_some() {
            "1-9/enter: toggle  ·  ↑/↓: move"
        } else {
            "1-9: pick  ·  ↑/↓: move  ·  enter: choose"
        };
        format!("{pick}{nav}  ·  esc: cancel")
    }

    /// Render the current question page. Returns the body lines and an
    /// optional (x, y) terminal-cursor position for the active text input.
    fn render_page(&self, area: Rect) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
        let page_idx = self.state.current_page();
        let page = &self.state.pages()[page_idx];
        let accent = Style::default().fg(Color::Indexed(ACCENT_BLUE_INDEX));
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut cursor: Option<(u16, u16)> = None;

        // Interrupt-level context header (codex `Reason:` style).
        if !self.description.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                self.description.clone(),
                muted.add_modifier(Modifier::ITALIC),
            )));
            lines.push(Line::default());
        }

        lines.push(Line::from(Span::styled(
            page.prompt.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        match page.kind {
            PageKind::Text => {
                let typed = self.state.custom_text(page_idx);
                let style = if self.state.is_typing() {
                    accent
                } else {
                    Style::default().fg(Color::White)
                };
                let row = lines.len() as u16;
                lines.push(Line::from(vec![
                    Span::raw("▌ "),
                    Span::styled(typed.to_string(), style),
                ]));
                // Cursor sits after the "▌ " prefix (2 cells) + the caret's
                // display column within the typed text (so multi-byte / wide
                // input stays aligned).
                let col = 2 + self.state.custom_cursor_display_col(page_idx) as u16;
                cursor = Some((area.x + col, area.y + row));
            }
            PageKind::Select | PageKind::Multiselect => {
                let radio = page.kind_is_select();
                let selected = self.state.selected_ids(page_idx);
                let rows = self.row_line_counts(page_idx, page);
                let total_rows = rows.len();
                let scroll = self.state.scroll().min(total_rows.saturating_sub(1));
                let shown = MAX_VISIBLE_OPTION_ROWS.min(total_rows.saturating_sub(scroll));
                let custom_idx = page.options.len();
                let next_idx = page.next_index();

                // A leading "▲ more" marker when scrolled down.
                if scroll > 0 {
                    lines.push(Line::from(Span::styled("  ▲ more".to_string(), muted)));
                }

                for row_idx in scroll..scroll + shown {
                    let hovered = self.state.cursor() == row_idx;
                    if row_idx < page.options.len() {
                        let opt = &page.options[row_idx];
                        let checked = selected.contains(&opt.id);
                        let marker = match (radio, checked) {
                            (true, true) => "(•) ",
                            (true, false) => "( ) ",
                            (false, true) => "[x] ",
                            (false, false) => "[ ] ",
                        };
                        let num = format!("{}. ", row_idx + 1);
                        lines.push(self.option_line(&num, marker, &opt.label, hovered));
                        if let Some(desc) = opt.description.as_deref() {
                            // Continuation line aligned under the label
                            // column (cursor + number + marker width).
                            let indent = 2 + num.len() + marker.len();
                            lines.push(Line::from(Span::styled(
                                format!("{}{desc}", " ".repeat(indent)),
                                muted,
                            )));
                        }
                    } else if row_idx == custom_idx {
                        let typed = self.state.custom_text(page_idx);
                        // Placeholder and typed text are mutually exclusive:
                        // an empty field shows the `Type your own answer`
                        // placeholder; once the user types, the row shows
                        // only what they typed (with the edit marker).
                        let label = if typed.is_empty() {
                            CUSTOM_LABEL.to_string()
                        } else {
                            typed.to_string()
                        };
                        let marker = if self.state.is_typing() && hovered {
                            "✎ "
                        } else {
                            "+ "
                        };
                        lines.push(self.option_line("", marker, &label, hovered));
                        if self.state.is_typing() && hovered {
                            // Park the cursor at the caret's display column.
                            // The rendered prefix on this row is the
                            // hover/cursor glyph ("▸ ") then the marker
                            // ("✎ ") — both multi-byte, so measure them by
                            // RENDERED WIDTH, not `.len()`. Since fix #1
                            // dropped the label prefix while typing, the
                            // only text before the caret is those two glyphs
                            // plus the typed string up to the caret.
                            let prefix = OPTION_CURSOR_WIDTH
                                + UnicodeWidthStr::width(marker)
                                + self.state.custom_cursor_display_col(page_idx);
                            let col = prefix as u16;
                            let row = (lines.len() - 1) as u16;
                            cursor = Some((area.x + col, area.y + row));
                        }
                    } else if Some(row_idx) == next_idx {
                        lines.push(self.option_line("", "→ ", NEXT_LABEL, hovered));
                    }
                }

                // A trailing "▼ more" marker when more rows lie below.
                if scroll + shown < total_rows {
                    lines.push(Line::from(Span::styled("  ▼ more".to_string(), muted)));
                }
            }
        }
        (lines, cursor)
    }

    fn option_line(&self, num: &str, marker: &str, label: &str, hovered: bool) -> Line<'static> {
        let cursor = if hovered {
            OPTION_CURSOR_HOVERED
        } else {
            OPTION_CURSOR_PLAIN
        };
        let style = if hovered {
            Style::default()
                .fg(Color::Indexed(ACCENT_BLUE_INDEX))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        Line::from(vec![
            Span::raw(cursor.to_string()),
            Span::styled(format!("{num}{marker}{label}"), style),
        ])
    }

    fn render_confirm(&self) -> Vec<Line<'static>> {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let red = Style::default().fg(Color::Red);
        let flags = self.state.answered_flags();
        let answers = self.state.collect_answers();
        let mut lines: Vec<Line<'static>> = Vec::new();
        if !self.description.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                self.description.clone(),
                muted.add_modifier(Modifier::ITALIC),
            )));
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            "Review your answers".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
        for (i, q) in self.questions.iter().enumerate() {
            let prompt = question_prompt(q).to_string();
            if flags.get(i).copied().unwrap_or(false) {
                let summary = summarize_answer(answers.get(i), q);
                lines.push(Line::from(vec![
                    Span::styled(format!("{prompt}: "), muted),
                    Span::styled(summary, Style::default().fg(Color::White)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(format!("{prompt}: "), muted),
                    Span::styled("⚠ unanswered".to_string(), red),
                ]));
            }
        }
        lines.push(Line::default());
        if flags.iter().all(|f| *f) {
            lines.push(Line::from(Span::styled(
                "Press enter to submit.".to_string(),
                Style::default().fg(Color::Green),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "Answer every question before submitting.".to_string(),
                red,
            )));
        }
        lines
    }
}

/// Map one proto question to a dialog page.
fn page_for(q: &InterruptQuestion) -> Page {
    match q {
        InterruptQuestion::Single {
            prompt, options, ..
        } => Page::select(prompt.clone(), opts(options)),
        InterruptQuestion::Multi {
            prompt, options, ..
        } => Page::multiselect(prompt.clone(), opts(options)),
        InterruptQuestion::Freetext { prompt } => Page::text(prompt.clone()),
    }
}

fn opts(options: &[InterruptOption]) -> Vec<DialogOption> {
    options
        .iter()
        .map(|o| DialogOption {
            id: o.id.clone(),
            label: o.label.clone(),
            description: o.description.clone(),
        })
        .collect()
}

/// Map a dialog [`Answer`] back to the proto [`ResolveResponse`] for its
/// question. The additive multiselect free-text rides as an extra
/// selected id (the option ids are stable; a typed value can't collide
/// with a proposed id, and the tool renders unknown ids verbatim).
fn answer_to_response(answer: &Answer, _q: &InterruptQuestion) -> ResolveResponse {
    match answer {
        Answer::Single { id } => ResolveResponse::Single {
            selected_id: id.clone(),
        },
        Answer::Multi { ids, custom } => {
            let mut selected_ids = ids.clone();
            if let Some(text) = custom {
                selected_ids.push(text.clone());
            }
            ResolveResponse::Multi { selected_ids }
        }
        Answer::Text { text } => ResolveResponse::Freetext { text: text.clone() },
    }
}

fn question_prompt(q: &InterruptQuestion) -> &str {
    match q {
        InterruptQuestion::Single { prompt, .. }
        | InterruptQuestion::Multi { prompt, .. }
        | InterruptQuestion::Freetext { prompt } => prompt,
    }
}

/// One-line confirm-page summary of a page's answer, resolving option
/// ids to labels where possible.
fn summarize_answer(answer: Option<&Answer>, q: &InterruptQuestion) -> String {
    match answer {
        Some(Answer::Single { id }) => label_for(q, id),
        Some(Answer::Multi { ids, custom }) => {
            let mut parts: Vec<String> = ids.iter().map(|id| label_for(q, id)).collect();
            if let Some(text) = custom {
                parts.push(format!("“{text}”"));
            }
            if parts.is_empty() {
                "[none]".to_string()
            } else {
                parts.join(", ")
            }
        }
        Some(Answer::Text { text }) => text.clone(),
        None => "[no answer]".to_string(),
    }
}

fn label_for(q: &InterruptQuestion, id: &str) -> String {
    let options: &[InterruptOption] = match q {
        InterruptQuestion::Single { options, .. } | InterruptQuestion::Multi { options, .. } => {
            options
        }
        InterruptQuestion::Freetext { .. } => &[],
    };
    options
        .iter()
        .find(|o| o.id == id)
        .map(|o| o.label.clone())
        .unwrap_or_else(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn opt(id: &str, label: &str) -> InterruptOption {
        InterruptOption {
            id: id.into(),
            label: label.into(),
            description: None,
        }
    }

    fn single_q() -> InterruptQuestionSet {
        InterruptQuestionSet {
            questions: vec![InterruptQuestion::Single {
                prompt: "DB?".into(),
                options: vec![opt("pg", "Postgres"), opt("sqlite", "SQLite")],
                allow_freetext: true,
            }],
        }
    }

    fn dialog(set: InterruptQuestionSet) -> QuestionDialog {
        QuestionDialog::new(Uuid::new_v4(), String::new(), set, Duration::ZERO)
    }

    #[test]
    fn submit_maps_to_single_resolve_response() {
        let iid = Uuid::new_v4();
        // Zero lockout so the dialog is immediately interactive.
        let mut d = QuestionDialog::new(iid, String::new(), single_q(), Duration::ZERO);
        // Hover first option, enter => fast-path submit.
        assert!(d.handle_key(press(KeyCode::Enter)));
        match d.take_result() {
            Some(QuestionResult::Submit {
                interrupt_id,
                responses,
            }) => {
                assert_eq!(interrupt_id, iid);
                assert!(matches!(
                    responses.as_slice(),
                    [ResolveResponse::Single { selected_id }] if selected_id == "pg"
                ));
            }
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn number_key_selects_and_submits_single() {
        let iid = Uuid::new_v4();
        let mut d = QuestionDialog::new(iid, String::new(), single_q(), Duration::ZERO);
        // Pressing `2` selects the second option AND advances => fast-path
        // submit (lone question).
        assert!(d.handle_key(press(KeyCode::Char('2'))));
        match d.take_result() {
            Some(QuestionResult::Submit { responses, .. }) => {
                assert!(matches!(
                    responses.as_slice(),
                    [ResolveResponse::Single { selected_id }] if selected_id == "sqlite"
                ));
            }
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn esc_maps_to_cancel() {
        let iid = Uuid::new_v4();
        let mut d = QuestionDialog::new(iid, String::new(), single_q(), Duration::ZERO);
        assert!(d.handle_key(press(KeyCode::Esc)));
        assert!(matches!(
            d.take_result(),
            Some(QuestionResult::Cancel { interrupt_id }) if interrupt_id == iid
        ));
    }

    #[test]
    fn multiselect_custom_rides_as_extra_id() {
        let q = InterruptQuestion::Multi {
            prompt: "tags?".into(),
            options: vec![opt("a", "A")],
            allow_freetext: true,
        };
        let answer = Answer::Multi {
            ids: vec!["a".into()],
            custom: Some("custom".into()),
        };
        let resp = answer_to_response(&answer, &q);
        match resp {
            ResolveResponse::Multi { selected_ids } => {
                assert_eq!(selected_ids, vec!["a".to_string(), "custom".to_string()]);
            }
            other => panic!("expected Multi, got {other:?}"),
        }
    }

    #[test]
    fn freetext_opens_in_typing_mode() {
        let set = InterruptQuestionSet {
            questions: vec![InterruptQuestion::Freetext {
                prompt: "Name?".into(),
            }],
        };
        let mut d = dialog(set);
        // No space/enter needed: typing is live immediately. A char lands
        // in the field.
        d.handle_key(press(KeyCode::Char('h')));
        d.handle_key(press(KeyCode::Char('i')));
        // Enter on a lone freetext question submits.
        assert!(d.handle_key(press(KeyCode::Enter)));
        match d.take_result() {
            Some(QuestionResult::Submit { responses, .. }) => {
                assert!(matches!(
                    responses.as_slice(),
                    [ResolveResponse::Freetext { text }] if text == "hi"
                ));
            }
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn desired_height_grows_with_descriptions() {
        let plain = dialog(single_q());
        let with_desc = dialog(InterruptQuestionSet {
            questions: vec![InterruptQuestion::Single {
                prompt: "DB?".into(),
                options: vec![
                    InterruptOption {
                        id: "pg".into(),
                        label: "Postgres".into(),
                        description: Some("Relational, ACID".into()),
                    },
                    InterruptOption {
                        id: "sqlite".into(),
                        label: "SQLite".into(),
                        description: Some("Embedded, single-file".into()),
                    },
                ],
                allow_freetext: true,
            }],
        });
        assert!(
            with_desc.desired_height() > plain.desired_height(),
            "per-option descriptions add body lines"
        );
        assert!(with_desc.desired_height() <= MAX_DIALOG_HEIGHT);
    }

    #[test]
    fn render_includes_description_and_context_header() {
        let set = InterruptQuestionSet {
            questions: vec![InterruptQuestion::Single {
                prompt: "DB?".into(),
                options: vec![InterruptOption {
                    id: "pg".into(),
                    label: "Postgres".into(),
                    description: Some("Relational engine".into()),
                }],
                allow_freetext: true,
            }],
        };
        let d = QuestionDialog::new(
            Uuid::new_v4(),
            "Choosing the storage backend".into(),
            set,
            Duration::ZERO,
        );
        let area = Rect::new(0, 0, 60, 12);
        let (lines, _) = d.render_page(area);
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Choosing the storage backend"),
            "context header"
        );
        assert!(text.contains("Relational engine"), "option description");
        assert!(text.contains("Postgres"), "option label");
    }

    /// Flatten a page's rendered body into one string per line.
    fn render_lines(d: &QuestionDialog, area: Rect) -> Vec<String> {
        let (lines, _) = d.render_page(area);
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn cursor_glyph_width_matches_constant() {
        // The parked-cursor math assumes both hover glyphs are
        // OPTION_CURSOR_WIDTH cells; assert that so it can't drift.
        assert_eq!(
            UnicodeWidthStr::width(OPTION_CURSOR_HOVERED),
            OPTION_CURSOR_WIDTH
        );
        assert_eq!(
            UnicodeWidthStr::width(OPTION_CURSOR_PLAIN),
            OPTION_CURSOR_WIDTH
        );
    }

    #[test]
    fn typed_custom_replaces_placeholder_label() {
        let mut d = dialog(single_q());
        let area = Rect::new(0, 0, 60, 12);
        // Empty field: the placeholder shows.
        let before = render_lines(&d, area).join("\n");
        assert!(
            before.contains(CUSTOM_LABEL),
            "empty field shows the placeholder"
        );
        // Move to the custom affordance, begin typing, type "hello".
        d.handle_key(press(KeyCode::Down)); // option 2
        d.handle_key(press(KeyCode::Down)); // custom affordance
        d.handle_key(press(KeyCode::Enter)); // begin typing (empty)
        assert!(d.state.is_typing());
        for c in "hello".chars() {
            d.handle_key(press(KeyCode::Char(c)));
        }
        let (lines, cursor) = d.render_page(area);
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let joined = text.join("\n");
        // The custom row reads only "hello" with the edit marker — never
        // the "Type your own answer:" prefix.
        assert!(
            text.iter().any(|l| l.contains("✎ hello")),
            "custom row shows the edit marker + typed text: {text:?}"
        );
        assert!(
            !joined.contains(&format!("{CUSTOM_LABEL}: ")),
            "placeholder prefix must not coexist with typed text"
        );
        assert!(
            !joined.contains(&format!("{CUSTOM_LABEL}\nhello"))
                && !text
                    .iter()
                    .any(|l| l.contains(CUSTOM_LABEL) && l.contains("hello")),
            "placeholder and typed text are mutually exclusive"
        );
        // The parked cursor sits immediately after "hello": hover glyph (2)
        // + marker "✎ " (2) + 5 chars = column 9.
        let (cx, _) = cursor.expect("typing parks a cursor");
        let expected = OPTION_CURSOR_WIDTH as u16
            + UnicodeWidthStr::width("✎ ") as u16
            + "hello".chars().count() as u16;
        assert_eq!(cx, area.x + expected, "cursor lands right after `hello`");
    }

    #[test]
    fn clearing_custom_reverts_to_placeholder() {
        let mut d = dialog(single_q());
        let area = Rect::new(0, 0, 60, 12);
        d.handle_key(press(KeyCode::Down));
        d.handle_key(press(KeyCode::Down)); // custom affordance
        d.handle_key(press(KeyCode::Enter)); // begin typing
        d.handle_key(press(KeyCode::Char('x')));
        assert!(render_lines(&d, area).iter().any(|l| l.contains("✎ x")));
        // Delete the only char: row reverts to the placeholder.
        d.handle_key(press(KeyCode::Backspace));
        let joined = render_lines(&d, area).join("\n");
        assert!(
            joined.contains(CUSTOM_LABEL),
            "empty field reverts to the placeholder: {joined}"
        );
    }

    #[test]
    fn cursor_display_col_tracks_multibyte_caret() {
        // A wide/multi-byte char before the caret must shift the parked
        // cursor by its DISPLAY width, not its byte length.
        let mut d = dialog(single_q());
        let area = Rect::new(0, 0, 60, 12);
        d.handle_key(press(KeyCode::Down));
        d.handle_key(press(KeyCode::Down)); // custom affordance
        d.handle_key(press(KeyCode::Enter)); // begin typing
        // "世" is a 3-byte, 2-cell-wide CJK glyph.
        d.handle_key(press(KeyCode::Char('世')));
        d.handle_key(press(KeyCode::Char('a')));
        let (_, cursor) = d.render_page(area);
        let (cx, _) = cursor.expect("typing parks a cursor");
        // hover(2) + marker(2) + width("世a") = 2 + 2 + (2 + 1) = 7.
        let expected = OPTION_CURSOR_WIDTH as u16 + 2 + 3;
        assert_eq!(cx, area.x + expected, "caret tracks display width");
    }

    #[test]
    fn esc_round_trip_preserves_typed_custom_text() {
        let mut d = dialog(single_q());
        let area = Rect::new(0, 0, 60, 12);
        d.handle_key(press(KeyCode::Down));
        d.handle_key(press(KeyCode::Down)); // custom affordance
        d.handle_key(press(KeyCode::Enter)); // begin typing
        for c in "abc".chars() {
            d.handle_key(press(KeyCode::Char(c)));
        }
        // First Esc defocuses; dialog stays open; text intact.
        assert!(!d.handle_key(press(KeyCode::Esc)), "Esc must not close");
        assert!(!d.state.is_typing());
        let joined = render_lines(&d, area).join("\n");
        assert!(joined.contains("abc"), "typed text survives Esc: {joined}");
        // Re-enter typing (Enter on the custom affordance with text present
        // commits on single-select; Space re-enters). Use Space to resume.
        d.handle_key(press(KeyCode::Char(' ')));
        assert!(d.state.is_typing(), "resumes typing");
        assert_eq!(d.state.custom_text(0), "abc", "resumes from same text");
    }

    #[test]
    fn long_list_scrolls_keeping_focus_visible() {
        let options: Vec<InterruptOption> = (0..20)
            .map(|i| opt(&format!("o{i}"), &format!("Option {i}")))
            .collect();
        let set = InterruptQuestionSet {
            questions: vec![InterruptQuestion::Single {
                prompt: "Pick".into(),
                options,
                allow_freetext: true,
            }],
        };
        let mut d = dialog(set);
        // Tight viewport: only a few rows fit.
        d.sync_viewport(8);
        // Move the cursor well past the initial window.
        for _ in 0..12 {
            d.handle_key(press(KeyCode::Down));
            d.sync_viewport(8);
        }
        // The focused cursor must lie within the rendered window.
        let scroll = d.state.scroll();
        let cursor = d.state.cursor();
        assert!(cursor >= scroll, "cursor not above the window");
        assert!(
            cursor < scroll + MAX_VISIBLE_OPTION_ROWS,
            "cursor not below the window"
        );
        assert!(scroll > 0, "list should have scrolled");
    }
}
