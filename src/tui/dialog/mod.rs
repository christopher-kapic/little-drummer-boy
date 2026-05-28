#![allow(dead_code)]
//! Reusable answering dialog (GOALS §3b).
//!
//! A modal that **replaces the composer** and walks the user through a
//! sequence of selectable pages ending in a confirm/submit page. The
//! `question` tool wires it today; a later tool-approval prompt reuses
//! the same core without touching it.
//!
//! ## What is generic vs. question-specific
//!
//! The state machine here ([`DialogState`]) knows nothing about
//! questions, proto types, or the daemon. It owns:
//!   - a `Vec<Page>` of [`Select`](PageKind::Select) /
//!     [`Multiselect`](PageKind::Multiselect) / [`Text`](PageKind::Text)
//!     pages, plus an implicit final confirm/submit page,
//!   - the cursor + selection + custom-text-typing state per page,
//!   - page-to-page navigation, validation, the anti-misfire lockout,
//!     and dismissal.
//!
//! On submit it yields a `Vec<`[`Answer`]`>` — one per page, in order —
//! which the *caller* maps to whatever resolution its use-case needs
//! (the `question` tool maps them to `ResolveResponse`s; a tool-approval
//! prompt would map them to an approve/deny decision). That `Answer →
//! resolution` mapping is the only question-specific code, and it lives
//! outside this module (`super::dialog::question`). That is the seam
//! that keeps the core reusable.
//!
//! The render + App-overlay glue is intentionally separate too
//! ([`super::dialog::question::QuestionDialog`]), so a second use-case
//! gets its own thin wrapper over this same state machine.

pub mod approval;
pub mod question;

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};

use crate::tui::textfield::TextField;

/// One proposed option on a select / multiselect page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogOption {
    pub id: String,
    pub label: String,
}

/// What a page asks for. The variants mirror the three answer modes; a
/// future use-case could add more without the navigation core caring.
#[derive(Debug, Clone)]
pub enum PageKind {
    /// Choose exactly one option (radio). Toggling a new option clears
    /// the previous selection.
    Select,
    /// Choose any number of options (checkboxes), independently.
    Multiselect,
    /// Free-text only; no option list.
    Text,
}

/// One page of the dialog: a prompt plus its answer mode and options.
#[derive(Debug, Clone)]
pub struct Page {
    pub prompt: String,
    pub kind: PageKind,
    pub options: Vec<DialogOption>,
}

impl Page {
    pub fn select(prompt: impl Into<String>, options: Vec<DialogOption>) -> Self {
        Self {
            prompt: prompt.into(),
            kind: PageKind::Select,
            options,
        }
    }

    pub fn multiselect(prompt: impl Into<String>, options: Vec<DialogOption>) -> Self {
        Self {
            prompt: prompt.into(),
            kind: PageKind::Multiselect,
            options,
        }
    }

    pub fn text(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            kind: PageKind::Text,
            options: Vec::new(),
        }
    }

    fn is_text(&self) -> bool {
        matches!(self.kind, PageKind::Text)
    }

    fn is_select(&self) -> bool {
        matches!(self.kind, PageKind::Select)
    }

    /// True for a radio (`select`) page. Public so the renderer can pick
    /// the radio vs. checkbox glyph.
    pub fn kind_is_select(&self) -> bool {
        self.is_select()
    }

    /// Cursor positions on this page: one per option, then one for the
    /// always-last "Type your own answer" affordance. A `text` page has
    /// a single position (its input).
    fn cursor_count(&self) -> usize {
        if self.is_text() {
            1
        } else {
            self.options.len() + 1
        }
    }

    /// Index of the always-last "Type your own answer" affordance on a
    /// select/multiselect page.
    fn custom_index(&self) -> usize {
        self.options.len()
    }
}

/// Per-page answer state the user has built so far.
#[derive(Debug, Clone, Default)]
struct PageState {
    /// Selected option ids (radio keeps ≤1; multiselect any number).
    selected: Vec<String>,
    /// The custom / free-text the user typed. For a `text` page this is
    /// the whole answer; for select/multiselect it's the additive
    /// "Type your own answer" value.
    custom: TextField,
}

/// The resolved answer for one page, handed back to the caller on
/// submit. Caller-agnostic — the question wiring maps these to proto
/// `ResolveResponse`s; a different use-case maps them differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A single chosen option id (select fast-path / radio).
    Single { id: String },
    /// Any number of chosen option ids plus an optional additive
    /// free-text answer (multiselect).
    Multi {
        ids: Vec<String>,
        custom: Option<String>,
    },
    /// A free-text answer (text page, or a select whose only answer was
    /// the custom field).
    Text { text: String },
}

/// Outcome of [`DialogState::handle_key`] — what the overlay host (the
/// TUI `App`) should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogOutcome {
    /// Stay open; redraw.
    Continue,
    /// User submitted from the confirm page. One [`Answer`] per page, in
    /// order.
    Submit(Vec<Answer>),
    /// User dismissed (Esc). Caller resolves as a cancel.
    Cancel,
}

/// The reusable dialog state machine. Terminal-free and fully testable.
pub struct DialogState {
    pages: Vec<Page>,
    page_states: Vec<PageState>,
    /// Current page index. Equals `pages.len()` on the confirm page.
    page: usize,
    /// Cursor within the current page (option index, or the custom
    /// affordance index). Unused on the confirm page.
    cursor: usize,
    /// True while the user is editing the custom / free-text field of
    /// the current page (keystrokes go to the field, not to navigation).
    typing: bool,
    /// When the dialog was created. The anti-misfire lockout runs from
    /// here.
    created_at: Instant,
    lockout: Duration,
}

impl DialogState {
    /// Build the state machine for `pages` with an anti-misfire
    /// `lockout`. `pages` must be non-empty (a dialog with no questions
    /// is a programming error at the call site).
    pub fn new(pages: Vec<Page>, lockout: Duration) -> Self {
        let page_states = pages.iter().map(|_| PageState::default()).collect();
        Self {
            pages,
            page_states,
            page: 0,
            cursor: 0,
            typing: false,
            created_at: Instant::now(),
            lockout,
        }
    }

    /// Test seam: build with an explicit creation instant so the lockout
    /// can be exercised deterministically.
    #[cfg(test)]
    fn new_at(pages: Vec<Page>, lockout: Duration, created_at: Instant) -> Self {
        let mut s = Self::new(pages, lockout);
        s.created_at = created_at;
        s
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn current_page(&self) -> usize {
        self.page
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_typing(&self) -> bool {
        self.typing
    }

    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// True while the dialog is in its non-interactive lockout window.
    /// The host renders a grey border and ignores input until this
    /// returns false (then: white border, interactive).
    pub fn locked(&self) -> bool {
        self.created_at.elapsed() < self.lockout
    }

    /// True when the confirm page is showing.
    pub fn on_confirm_page(&self) -> bool {
        self.page == self.pages.len()
    }

    /// Whether each page has a usable answer. Drives the confirm page's
    /// "unanswered" flags and gates submit.
    pub fn answered_flags(&self) -> Vec<bool> {
        (0..self.pages.len()).map(|i| self.is_answered(i)).collect()
    }

    fn is_answered(&self, page: usize) -> bool {
        let st = &self.page_states[page];
        match self.pages[page].kind {
            PageKind::Text => !st.custom.text().trim().is_empty(),
            PageKind::Select | PageKind::Multiselect => {
                !st.selected.is_empty() || !st.custom.text().trim().is_empty()
            }
        }
    }

    fn all_answered(&self) -> bool {
        (0..self.pages.len()).all(|i| self.is_answered(i))
    }

    /// Read the selected ids on `page` (for rendering check marks).
    pub fn selected_ids(&self, page: usize) -> &[String] {
        &self.page_states[page].selected
    }

    /// Read the custom-text buffer on `page` (for rendering + cursor).
    pub fn custom_text(&self, page: usize) -> &str {
        self.page_states[page].custom.text()
    }

    pub fn custom_cursor_col(&self, page: usize) -> usize {
        self.page_states[page].custom.cursor_col()
    }

    /// Apply a key. Returns the outcome the host acts on. Input is
    /// ignored (returns `Continue`) while [`locked`](Self::locked).
    pub fn handle_key(&mut self, key: KeyEvent) -> DialogOutcome {
        if self.locked() {
            return DialogOutcome::Continue;
        }
        // Esc always dismisses the whole dialog (even mid-typing — the
        // user wants out).
        if matches!(key.code, KeyCode::Esc) {
            return DialogOutcome::Cancel;
        }
        if self.typing {
            return self.handle_typing_key(key);
        }
        if self.on_confirm_page() {
            return self.handle_confirm_key(key);
        }
        self.handle_page_key(key)
    }

    /// Keys while editing a custom / free-text field.
    fn handle_typing_key(&mut self, key: KeyEvent) -> DialogOutcome {
        match key.code {
            // Enter exits typing mode back to cursor navigation. On a
            // `text` page there's nowhere to navigate, but leaving typing
            // mode is still the right "I'm done with this field" gesture.
            KeyCode::Enter => {
                self.typing = false;
                DialogOutcome::Continue
            }
            _ => {
                self.page_states[self.page].custom.handle_key(key);
                DialogOutcome::Continue
            }
        }
    }

    /// Keys on a select / multiselect / text page (not typing).
    fn handle_page_key(&mut self, key: KeyEvent) -> DialogOutcome {
        let page = &self.pages[self.page];
        // `text` pages: a single field. Entering it switches to typing.
        if page.is_text() {
            return match key.code {
                KeyCode::Char(' ') | KeyCode::Enter => {
                    self.typing = true;
                    DialogOutcome::Continue
                }
                KeyCode::Left | KeyCode::Char('h') => self.prev_page(),
                KeyCode::Right | KeyCode::Char('l') => self.next_page(),
                _ => DialogOutcome::Continue,
            };
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor(-1);
                DialogOutcome::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor(1);
                DialogOutcome::Continue
            }
            KeyCode::Left | KeyCode::Char('h') => self.prev_page(),
            KeyCode::Right | KeyCode::Char('l') => self.next_page(),
            KeyCode::Char(' ') => {
                self.toggle_or_type();
                DialogOutcome::Continue
            }
            KeyCode::Enter => self.enter_on_page(),
            _ => DialogOutcome::Continue,
        }
    }

    /// Space on a page: toggle the hovered option, or enter typing mode
    /// on the custom affordance.
    fn toggle_or_type(&mut self) {
        let page = &self.pages[self.page];
        if self.cursor == page.custom_index() {
            // Hovering "Type your own answer": space begins typing.
            self.typing = true;
            return;
        }
        let id = page.options[self.cursor].id.clone();
        let is_select = page.is_select();
        let st = &mut self.page_states[self.page];
        if is_select {
            // Radio: toggling a new option replaces the prior selection;
            // toggling the already-selected one clears it.
            if st.selected == [id.clone()] {
                st.selected.clear();
            } else {
                st.selected = vec![id];
            }
        } else if let Some(pos) = st.selected.iter().position(|s| *s == id) {
            st.selected.remove(pos);
        } else {
            st.selected.push(id);
        }
    }

    /// Enter on a select/multiselect page (cursor mode).
    fn enter_on_page(&mut self) -> DialogOutcome {
        let page = &self.pages[self.page];
        if self.cursor == page.custom_index() {
            // On the custom affordance: with text already typed, enter =
            // choose+submit that custom answer; with nothing typed, enter
            // = begin typing.
            if self.page_states[self.page].custom.text().trim().is_empty() {
                self.typing = true;
                return DialogOutcome::Continue;
            }
            return self.fast_path_submit_or_advance();
        }
        // Hovering a proposed option: enter chooses it (radio for
        // select; additive set for multiselect — but the single-question
        // fast path means we then try to submit).
        let id = page.options[self.cursor].id.clone();
        let is_select = page.is_select();
        let st = &mut self.page_states[self.page];
        if is_select {
            st.selected = vec![id];
        } else if !st.selected.contains(&id) {
            st.selected.push(id);
        }
        self.fast_path_submit_or_advance()
    }

    /// Single-question fast path: if this is the only page and it's now
    /// answered, submit immediately; otherwise advance toward the
    /// confirm page.
    fn fast_path_submit_or_advance(&mut self) -> DialogOutcome {
        if self.pages.len() == 1 && self.all_answered() {
            return DialogOutcome::Submit(self.collect_answers());
        }
        self.next_page()
    }

    /// Keys on the confirm/submit page.
    fn handle_confirm_key(&mut self, key: KeyEvent) -> DialogOutcome {
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => self.prev_page(),
            KeyCode::Enter => {
                if self.all_answered() {
                    DialogOutcome::Submit(self.collect_answers())
                } else {
                    // Jump the cursor to the first unanswered page so the
                    // user can fix it; refuse to submit.
                    if let Some(first) = (0..self.pages.len()).find(|&i| !self.is_answered(i)) {
                        self.page = first;
                        self.cursor = 0;
                    }
                    DialogOutcome::Continue
                }
            }
            _ => DialogOutcome::Continue,
        }
    }

    /// Move the cursor within the current page, wrapping. Down from the
    /// last position (the custom affordance) wraps to the top.
    fn move_cursor(&mut self, delta: i32) {
        let n = self.pages[self.page].cursor_count() as i32;
        if n == 0 {
            return;
        }
        self.cursor = (((self.cursor as i32 + delta) % n + n) % n) as usize;
    }

    /// Advance to the next page (or the confirm page). Resets the cursor.
    fn next_page(&mut self) -> DialogOutcome {
        if self.page < self.pages.len() {
            self.page += 1;
            self.cursor = 0;
            self.typing = false;
        }
        DialogOutcome::Continue
    }

    /// Step back one page. Resets the cursor.
    fn prev_page(&mut self) -> DialogOutcome {
        if self.page > 0 {
            self.page -= 1;
            self.cursor = 0;
            self.typing = false;
        }
        DialogOutcome::Continue
    }

    /// Build the final answer list — one [`Answer`] per page.
    pub fn collect_answers(&self) -> Vec<Answer> {
        self.pages
            .iter()
            .zip(self.page_states.iter())
            .map(|(page, st)| Self::answer_for(page, st))
            .collect()
    }

    fn answer_for(page: &Page, st: &PageState) -> Answer {
        let custom = st.custom.text().trim();
        match page.kind {
            PageKind::Text => Answer::Text {
                text: custom.to_string(),
            },
            PageKind::Select => {
                // A select with a checked option answers Single; a select
                // whose only answer is the custom field answers Text.
                if let Some(id) = st.selected.first() {
                    Answer::Single { id: id.clone() }
                } else {
                    Answer::Text {
                        text: custom.to_string(),
                    }
                }
            }
            PageKind::Multiselect => Answer::Multi {
                ids: st.selected.clone(),
                custom: if custom.is_empty() {
                    None
                } else {
                    Some(custom.to_string())
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn opt(id: &str) -> DialogOption {
        DialogOption {
            id: id.into(),
            label: id.to_uppercase(),
        }
    }

    /// Build an already-unlocked single-select dialog for behavior tests.
    fn unlocked(pages: Vec<Page>) -> DialogState {
        DialogState::new_at(
            pages,
            Duration::from_millis(1500),
            Instant::now() - Duration::from_secs(10),
        )
    }

    #[test]
    fn locked_then_unlocked_transition() {
        // Just-created: locked, ignores input.
        let mut d = DialogState::new(
            vec![Page::select("?", vec![opt("a"), opt("b")])],
            Duration::from_millis(50),
        );
        assert!(d.locked(), "fresh dialog must be locked (grey border)");
        assert_eq!(
            d.handle_key(press(KeyCode::Char('j'))),
            DialogOutcome::Continue
        );
        assert_eq!(d.cursor(), 0, "input ignored during lockout");

        // After the lockout window: interactive (white border).
        std::thread::sleep(Duration::from_millis(60));
        assert!(!d.locked(), "lockout must elapse to interactive");
        d.handle_key(press(KeyCode::Char('j')));
        assert_eq!(d.cursor(), 1, "input accepted after lockout");
    }

    #[test]
    fn jk_navigates_and_wraps_through_custom() {
        let mut d = unlocked(vec![Page::select("?", vec![opt("a"), opt("b")])]);
        // 2 options + custom affordance => 3 cursor slots.
        assert_eq!(d.cursor(), 0);
        d.handle_key(press(KeyCode::Char('j')));
        assert_eq!(d.cursor(), 1);
        d.handle_key(press(KeyCode::Char('j')));
        assert_eq!(d.cursor(), 2, "lands on the custom affordance");
        // Down from custom wraps to the top.
        d.handle_key(press(KeyCode::Down));
        assert_eq!(d.cursor(), 0);
        // Up from the top wraps to custom.
        d.handle_key(press(KeyCode::Up));
        assert_eq!(d.cursor(), 2);
    }

    #[test]
    fn select_space_is_radio() {
        let mut d = unlocked(vec![Page::select("?", vec![opt("a"), opt("b")])]);
        d.handle_key(press(KeyCode::Char(' '))); // select a
        assert_eq!(d.selected_ids(0), &["a".to_string()]);
        d.handle_key(press(KeyCode::Char('j'))); // hover b
        d.handle_key(press(KeyCode::Char(' '))); // select b -> a cleared
        assert_eq!(d.selected_ids(0), &["b".to_string()]);
        // Toggling the selected one clears it.
        d.handle_key(press(KeyCode::Char(' ')));
        assert!(d.selected_ids(0).is_empty());
    }

    #[test]
    fn multiselect_space_is_independent() {
        let mut d = unlocked(vec![Page::multiselect("?", vec![opt("a"), opt("b")])]);
        d.handle_key(press(KeyCode::Char(' '))); // a
        d.handle_key(press(KeyCode::Char('j')));
        d.handle_key(press(KeyCode::Char(' '))); // b
        assert_eq!(d.selected_ids(0), &["a".to_string(), "b".to_string()]);
        d.handle_key(press(KeyCode::Char('k')));
        d.handle_key(press(KeyCode::Char(' '))); // toggle a off
        assert_eq!(d.selected_ids(0), &["b".to_string()]);
    }

    #[test]
    fn single_question_enter_fast_path_submits() {
        let mut d = unlocked(vec![Page::select("?", vec![opt("a"), opt("b")])]);
        // Hover the first option, enter => choose + submit immediately.
        let out = d.handle_key(press(KeyCode::Enter));
        assert_eq!(
            out,
            DialogOutcome::Submit(vec![Answer::Single { id: "a".into() }])
        );
    }

    #[test]
    fn custom_text_typing_mode_flow() {
        let mut d = unlocked(vec![Page::select("?", vec![opt("a")])]);
        // Move to the custom affordance (index 1).
        d.handle_key(press(KeyCode::Char('j')));
        assert_eq!(d.cursor(), 1);
        // Nothing typed yet: enter begins typing mode.
        d.handle_key(press(KeyCode::Enter));
        assert!(d.is_typing());
        // Type a couple chars.
        d.handle_key(press(KeyCode::Char('h')));
        d.handle_key(press(KeyCode::Char('i')));
        assert_eq!(d.custom_text(0), "hi");
        // Enter exits typing mode.
        d.handle_key(press(KeyCode::Enter));
        assert!(!d.is_typing());
        // Now enter on the custom affordance (text present) submits.
        let out = d.handle_key(press(KeyCode::Enter));
        assert_eq!(
            out,
            DialogOutcome::Submit(vec![Answer::Text { text: "hi".into() }])
        );
    }

    #[test]
    fn multiselect_custom_answer_is_additive() {
        let mut d = unlocked(vec![Page::multiselect("?", vec![opt("a"), opt("b")])]);
        d.handle_key(press(KeyCode::Char(' '))); // check a
        // Go to custom (index 2), type.
        d.handle_key(press(KeyCode::Char('j')));
        d.handle_key(press(KeyCode::Char('j')));
        d.handle_key(press(KeyCode::Char(' '))); // begin typing
        d.handle_key(press(KeyCode::Char('x')));
        d.handle_key(press(KeyCode::Enter)); // exit typing
        let answers = d.collect_answers();
        assert_eq!(
            answers,
            vec![Answer::Multi {
                ids: vec!["a".into()],
                custom: Some("x".into())
            }]
        );
    }

    #[test]
    fn multi_question_nav_and_confirm_validation() {
        let mut d = unlocked(vec![Page::select("q1", vec![opt("a")]), Page::text("q2")]);
        // Page 0: answer it (space selects, no fast-path because 2 pages).
        d.handle_key(press(KeyCode::Char(' ')));
        assert_eq!(d.selected_ids(0), &["a".to_string()]);
        // Right to page 1.
        d.handle_key(press(KeyCode::Char('l')));
        assert_eq!(d.current_page(), 1);
        // Right to the confirm page WITHOUT answering q2.
        d.handle_key(press(KeyCode::Char('l')));
        assert!(d.on_confirm_page());
        // Enter on confirm with an unanswered q2: refuses, jumps to q2.
        let out = d.handle_key(press(KeyCode::Enter));
        assert_eq!(out, DialogOutcome::Continue);
        assert_eq!(d.current_page(), 1, "jumped to the unanswered page");
        // Answer q2.
        d.handle_key(press(KeyCode::Char(' '))); // begin typing
        d.handle_key(press(KeyCode::Char('z')));
        d.handle_key(press(KeyCode::Enter)); // exit typing
        // Right to confirm, enter submits now.
        d.handle_key(press(KeyCode::Char('l')));
        let out = d.handle_key(press(KeyCode::Enter));
        assert_eq!(
            out,
            DialogOutcome::Submit(vec![
                Answer::Single { id: "a".into() },
                Answer::Text { text: "z".into() },
            ])
        );
    }

    #[test]
    fn esc_cancels_even_while_typing() {
        let mut d = unlocked(vec![Page::text("q")]);
        d.handle_key(press(KeyCode::Char(' '))); // enter typing
        assert!(d.is_typing());
        let out = d.handle_key(press(KeyCode::Esc));
        assert_eq!(out, DialogOutcome::Cancel);
    }

    #[test]
    fn answered_flags_track_each_page() {
        let mut d = unlocked(vec![Page::select("q1", vec![opt("a")]), Page::text("q2")]);
        assert_eq!(d.answered_flags(), vec![false, false]);
        d.handle_key(press(KeyCode::Char(' '))); // answer q1
        assert_eq!(d.answered_flags(), vec![true, false]);
    }
}
