//! Question-tool wiring over the reusable [`DialogState`] (GOALS §3b).
//!
//! This is the thin, use-case-specific layer the spec calls for: it
//! translates the daemon's [`InterruptQuestionSet`] into dialog
//! [`Page`]s, drives the shared state machine for input, renders the
//! dialog over the composer, and maps the resulting [`Answer`]s back to
//! the proto [`ResolveResponse`]s the `question` tool expects. A future
//! tool-approval prompt gets its own equivalent of this file and reuses
//! [`DialogState`] unchanged.

use std::time::Duration;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use uuid::Uuid;

use crate::daemon::proto::{
    InterruptOption, InterruptQuestion, InterruptQuestionSet, ResolveResponse,
};
use crate::tui::dialog::{Answer, DialogOption, DialogOutcome, DialogState, Page, PageKind};
use crate::tui::theme::{ACCENT_BLUE_INDEX, MUTED_COLOR_INDEX};

/// Reserved body height for the dialog overlay (mirrors
/// `daemon_prompt::DIALOG_HEIGHT`'s role for geometry).
pub const DIALOG_HEIGHT: u16 = 18;

const CUSTOM_LABEL: &str = "Type your own answer";

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
/// questions, so option ids map correctly even for select free-text).
pub struct QuestionDialog {
    interrupt_id: Uuid,
    questions: Vec<InterruptQuestion>,
    state: DialogState,
    result: Option<QuestionResult>,
}

impl QuestionDialog {
    /// Build the dialog for a raised interrupt. `lockout` is the
    /// configured anti-misfire delay (default 1.5s).
    pub fn new(interrupt_id: Uuid, set: InterruptQuestionSet, lockout: Duration) -> Self {
        let pages = set.questions.iter().map(page_for).collect();
        let state = DialogState::new(pages, lockout);
        Self {
            interrupt_id,
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

    pub fn render(&self, frame: &mut Frame, area: Rect) {
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

        let lines = if self.state.on_confirm_page() {
            self.render_confirm()
        } else {
            self.render_page()
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
        format!("↑/↓ j/k: move  ·  space: select  ·  enter: choose{nav}  ·  esc: cancel")
    }

    fn render_page(&self) -> Vec<Line<'static>> {
        let page_idx = self.state.current_page();
        let page = &self.state.pages()[page_idx];
        let accent = Style::default().fg(Color::Indexed(ACCENT_BLUE_INDEX));
        let mut lines: Vec<Line<'static>> = Vec::new();
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
                let shown = if typed.is_empty() && !self.state.is_typing() {
                    "(press space/enter to type)".to_string()
                } else {
                    typed.to_string()
                };
                lines.push(Line::from(vec![
                    Span::raw("▌ "),
                    Span::styled(shown, style),
                ]));
            }
            PageKind::Select | PageKind::Multiselect => {
                let selected = self.state.selected_ids(page_idx);
                let radio = page.kind_is_select();
                for (i, opt) in page.options.iter().enumerate() {
                    let hovered = self.state.cursor() == i;
                    let checked = selected.contains(&opt.id);
                    let marker = match (radio, checked) {
                        (true, true) => "(•) ",
                        (true, false) => "( ) ",
                        (false, true) => "[x] ",
                        (false, false) => "[ ] ",
                    };
                    lines.push(self.option_line(marker, &opt.label, hovered));
                }
                // Always-last "Type your own answer".
                let custom_idx = page.options.len();
                let hovered = self.state.cursor() == custom_idx;
                let typed = self.state.custom_text(page_idx);
                let label = if typed.is_empty() {
                    CUSTOM_LABEL.to_string()
                } else {
                    format!("{CUSTOM_LABEL}: {typed}")
                };
                let marker = if self.state.is_typing() && hovered {
                    "✎ "
                } else {
                    "+ "
                };
                lines.push(self.option_line(marker, &label, hovered));
            }
        }
        lines
    }

    fn option_line(&self, marker: &str, label: &str, hovered: bool) -> Line<'static> {
        let cursor = if hovered { "▸ " } else { "  " };
        let style = if hovered {
            Style::default()
                .fg(Color::Indexed(ACCENT_BLUE_INDEX))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        Line::from(vec![
            Span::raw(cursor.to_string()),
            Span::styled(format!("{marker}{label}"), style),
        ])
    }

    fn render_confirm(&self) -> Vec<Line<'static>> {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let red = Style::default().fg(Color::Red);
        let flags = self.state.answered_flags();
        let answers = self.state.collect_answers();
        let mut lines: Vec<Line<'static>> = Vec::new();
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

    fn single_q() -> InterruptQuestionSet {
        InterruptQuestionSet {
            questions: vec![InterruptQuestion::Single {
                prompt: "DB?".into(),
                options: vec![
                    InterruptOption {
                        id: "pg".into(),
                        label: "Postgres".into(),
                    },
                    InterruptOption {
                        id: "sqlite".into(),
                        label: "SQLite".into(),
                    },
                ],
                allow_freetext: true,
            }],
        }
    }

    #[test]
    fn submit_maps_to_single_resolve_response() {
        let iid = Uuid::new_v4();
        // Zero lockout so the dialog is immediately interactive.
        let mut d = QuestionDialog::new(iid, single_q(), Duration::ZERO);
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
    fn esc_maps_to_cancel() {
        let iid = Uuid::new_v4();
        let mut d = QuestionDialog::new(iid, single_q(), Duration::ZERO);
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
            options: vec![InterruptOption {
                id: "a".into(),
                label: "A".into(),
            }],
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
}
