//! Composer key handling: vim mode state machine, history navigation,
//! submit, and the small Ctrl+Shift+{Y,C} helpers shared with the
//! mouse module.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::tui::composer::{Operator, VimMode};
use crate::tui::history::HistoryEntry;

use super::{App, slash_matches};
use crate::tui::settings::Dialog;

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Ctrl+C / Ctrl+D quit. Explicitly exclude Shift so that
        // Ctrl+Shift+C (copy-selection, plan.md T8.f) doesn't trigger
        // an exit on terminals that report the shift state in
        // `modifiers` even when the key code is lowercase.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            return true;
        }

        // Any meaningful keystroke dismisses the toast — the user has
        // moved on. Pure-modifier presses (Shift, Ctrl, etc. alone)
        // don't count.
        if self.toast.is_some() && !is_modifier_only(&key) {
            self.toast = None;
        }

        // Context menu intercepts keys while open. Arrows / j-k move
        // the focus, Enter executes, Esc dismisses, any other
        // printable key dismisses without executing (so the user can
        // resume typing into the composer without a stray menu).
        if let Some(menu) = self.context_menu.clone() {
            match key.code {
                KeyCode::Esc => {
                    self.context_menu = None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(m) = self.context_menu.as_mut() {
                        m.move_cursor(-1);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(m) = self.context_menu.as_mut() {
                        m.move_cursor(1);
                    }
                }
                KeyCode::Enter => {
                    self.context_menu = None;
                    if let Some(action) = menu.focused_action() {
                        self.execute_context_menu_action(action, menu.clicked_chat_row);
                    }
                }
                _ if !is_modifier_only(&key) => {
                    // Any other typed key dismisses without action.
                    self.context_menu = None;
                }
                _ => {}
            }
            return false;
        }

        // Escape with an active selection: clear the selection and
        // swallow the key. Ordering: ahead of dialog routing because
        // the selection lives on App-state and isn't visible to the
        // dialog handlers; behind the Ctrl+C quit because the user
        // expects Ctrl+C to always exit regardless of selection.
        if matches!(key.code, KeyCode::Esc) && self.selection.is_some() {
            self.selection = None;
            return false;
        }

        // Modal dialog rule: whenever a modal is open we must
        // `return false` (consume the key) before any other handler
        // sees it. Otherwise navigation chars (`j`/`k`/etc.) that the
        // modal interpreted as up/down also fall through to the
        // composer's char-insert arm and leak into the textbox.
        //
        // The shape below is the same for every modal:
        //   1. let inner handle the key
        //   2. if it requested close: drain its result, close it
        //   3. unconditionally `return false`
        if let Some(prompt) = self.daemon_prompt.as_mut() {
            let should_close = prompt.handle_key(key);
            if !should_close {
                return false;
            }
            let choice = prompt.take_choice();
            match choice {
                Some(crate::tui::daemon_prompt::DaemonChoice::StartAndConnect) => {
                    match crate::daemon::DaemonPaths::resolve()
                        .and_then(|_| crate::daemon::spawn_detached())
                    {
                        Ok(pid) => {
                            self.history.push(HistoryEntry::Plain {
                                line: format!(
                                    "daemon: spawned (pid {pid}); stop later with `cockpit daemon stop`"
                                ),
                            });
                            self.daemon_connected = true;
                            self.daemon_prompt = None;
                            self.maybe_open_add_provider_wizard();
                        }
                        Err(e) => {
                            if let Some(p) = self.daemon_prompt.as_mut() {
                                p.set_error(format!("failed to spawn daemon: {e}"));
                            }
                        }
                    }
                }
                Some(crate::tui::daemon_prompt::DaemonChoice::ContinueWithout) => {
                    self.history.push(HistoryEntry::Plain {
                        line:
                            "daemon: continuing without — features that need the daemon will be limited"
                                .to_string(),
                    });
                    self.daemon_prompt = None;
                    self.maybe_open_add_provider_wizard();
                }
                Some(crate::tui::daemon_prompt::DaemonChoice::Exit) | None => {
                    return true;
                }
            }
            return false;
        }

        if self.dialog.is_active() {
            if self.dialog.handle_key(key) {
                // Closing the settings dialog can change the active
                // provider/model — reload launch info so the status
                // line and header refresh. TUI-side settings (vim
                // mode, thinking display, markdown) are also reloaded
                // so they apply without a restart.
                self.dialog = Dialog::None;
                self.reload_launch_info();
                self.reload_tui_config();
            }
            return false;
        }

        if let Some(picker) = self.model_picker.as_mut() {
            let should_close = picker.handle_key(key);
            if should_close {
                self.model_picker = None;
                self.reload_launch_info();
                let line = self.model_summary_history_line();
                self.history.push(HistoryEntry::Plain { line });
            }
            // See the "modal dialog rule" comment above — always
            // consume the key while the picker is open.
            return false;
        }

        // Ctrl+J toggles every agent reasoning block's expand/collapse
        // state. (See the doc comment on `toggle_recent_reasoning` for
        // why this is a keybind rather than a click handler.) Only
        // intercepted when at least one entry actually has a reasoning
        // block — otherwise Ctrl+J falls through to its newline-insert
        // role in the composer.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('j'))
            && self.history.iter().any(|e| {
                matches!(e,
                HistoryEntry::Agent { reasoning, .. } if !reasoning.trim().is_empty())
            })
        {
            self.toggle_recent_reasoning();
            return false;
        }

        // Ctrl+Shift+Y — copy the most-recent agent message to the
        // system clipboard as rich text (HTML + plain alt). Falls back
        // to plain text over SSH. Gated by tui.rich_text_copy.
        // (plan.md T8.g)
        if self.is_ctrl_shift_y(&key) {
            self.copy_last_agent_message_as_rich_text();
            return false;
        }

        // Ctrl+Shift+C — copy the active drag-selection's plaintext
        // through OSC52 (SSH-safe) + local clipboard. No-op when
        // nothing is selected. (plan.md T8.f copy path)
        if self.is_ctrl_shift_c(&key) {
            self.copy_selection_plaintext();
            return false;
        }

        // Ctrl+G — pop the composer text out into `$EDITOR`. We can't
        // suspend ratatui from inside the key handler (the terminal
        // handle lives in `event_loop`), so just request the action;
        // the loop services it before the next draw.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g')) {
            if std::env::var_os("EDITOR").is_none() {
                self.history.push(HistoryEntry::Plain {
                    line: "No $EDITOR environment variable".to_string(),
                });
            } else {
                self.pending_external_edit = true;
            }
            return false;
        }

        // Anything that gets this far is a composer-facing key — char
        // input, arrow nav, vim-mode keys, etc. By design the user is
        // engaging with the composer, so any active chat selection
        // becomes stale and gets in the way. Drop it before the
        // composer mutates. Modifier-only keys (Shift alone, etc.) are
        // skipped so just *holding* Shift doesn't clear the selection
        // mid-drag-extend-by-keyboard.
        if self.selection.is_some() && !is_modifier_only(&key) {
            self.selection = None;
        }

        // Vim-aware dispatch. Normal / Operator-pending intercept
        // char keys; Insert mode falls through to the standard editor
        // path (also used when vim is disabled).
        if self.composer.vim_enabled() {
            match self.composer.vim_mode() {
                VimMode::Normal => return self.handle_key_normal(key),
                VimMode::Operator(op) => return self.handle_key_operator(key, op),
                VimMode::Insert => {}
            }
        }
        self.handle_key_insert(key)
    }

    pub(super) fn handle_key_insert(&mut self, key: KeyEvent) -> bool {
        // `@`-popup intercepts navigation + accept keys when active.
        if self.at_popup_active() {
            match key.code {
                KeyCode::Esc => {
                    self.at_dismissed = true;
                    self.at_selected = 0;
                    return false;
                }
                KeyCode::Up => {
                    let n = self.at_suggestions().len();
                    if n > 0 {
                        // Hard stop at the top (no wrap) + scrolloff so
                        // the previous item stays visible until index 0.
                        self.at_selected = self.at_selected.saturating_sub(1);
                        self.at_scroll = super::windowed_scroll(
                            self.at_selected,
                            self.at_scroll,
                            n,
                            super::AUTOCOMPLETE_ROWS as usize,
                        );
                    }
                    return false;
                }
                KeyCode::Down => {
                    let n = self.at_suggestions().len();
                    if n > 0 {
                        self.at_selected = (self.at_selected + 1).min(n - 1);
                        self.at_scroll = super::windowed_scroll(
                            self.at_selected,
                            self.at_scroll,
                            n,
                            super::AUTOCOMPLETE_ROWS as usize,
                        );
                    }
                    return false;
                }
                KeyCode::Tab => {
                    // Tab finalizes a file (space + close) but *descends*
                    // into a directory (no space, popup stays open).
                    if self.accept_at_suggestion(false) {
                        return false;
                    }
                    // No suggestion to take — Tab is otherwise inert.
                    return false;
                }
                KeyCode::Enter => {
                    // Enter finalizes whatever is highlighted, file or
                    // dir: append a space and close the @ session.
                    if self.accept_at_suggestion(true) {
                        return false;
                    }
                    // Fall through to default Enter handling if accept
                    // failed (e.g. no suggestions to take) — submits.
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => {
                // Esc cancels an in-progress slash command. Otherwise:
                // when vim is enabled, it drops the composer into
                // Normal mode. When vim is disabled it's a no-op
                // (deliberate — too easy to hit accidentally for an
                // exit path; `/exit`, Ctrl+C, Ctrl+D cover that).
                if self.slash_query().is_some() {
                    self.composer.clear();
                } else if self.composer.vim_enabled() {
                    self.composer.set_vim_mode(VimMode::Normal);
                    self.composer.set_pending_g(false);
                }
                false
            }
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    self.composer.insert_char('\n');
                    self.refresh_at_dismiss();
                    false
                } else {
                    self.complete_or_submit()
                }
            }
            // Newline fallback for terminals that can't disambiguate
            // Shift+Enter (most legacy terminfo entries, every plain
            // xterm-256color, and the common path through tmux+ssh
            // without the kitty keyboard protocol). Ctrl+J is the
            // canonical LF on every Unix terminal and survives every
            // multiplexer hop.
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.insert_char('\n');
                false
            }
            KeyCode::Backspace => {
                // Whole-tag delete: when not actively composing a tag
                // (popup closed) and the cursor sits at a completed
                // tag's right edge, one Backspace removes the whole tag.
                if !self.at_popup_active()
                    && let Some((s, e)) = self.completed_tag_left()
                {
                    self.composer.delete_range(s, e);
                    self.refresh_at_dismiss();
                    self.reset_at_window();
                    return false;
                }
                self.composer.delete_left();
                // Two-keystroke trailing space: if we just removed a
                // space that sat right after a completed tag, keep the
                // popup suppressed so the *next* Backspace deletes the
                // whole tag rather than re-opening the popup on it.
                if self.completed_tag_left().is_some() {
                    self.at_dismissed = true;
                } else {
                    self.refresh_at_dismiss();
                }
                self.reset_at_window();
                false
            }
            KeyCode::Delete => {
                if !self.at_popup_active()
                    && let Some((s, e)) = self.completed_tag_right()
                {
                    self.composer.delete_range(s, e);
                    self.refresh_at_dismiss();
                    self.reset_at_window();
                    return false;
                }
                self.composer.delete_right();
                self.refresh_at_dismiss();
                self.reset_at_window();
                false
            }
            KeyCode::Left => {
                self.composer.move_left();
                false
            }
            KeyCode::Right => {
                self.composer.move_right();
                false
            }
            KeyCode::Up => {
                self.history_up();
                false
            }
            KeyCode::Down => {
                self.history_down();
                false
            }
            KeyCode::Home => {
                self.composer.move_line_start();
                false
            }
            KeyCode::End => {
                self.composer.move_line_end();
                false
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.insert_char(ch);
                // Note: we deliberately do NOT reset
                // `prompt_history_cursor` here. Edits made while in
                // recall mode stay in the buffer, but pressing Down
                // back to cursor 0 still restores the original
                // staged draft — matching the user-visible spec for
                // history navigation.
                self.refresh_at_dismiss();
                self.reset_at_window();
                false
            }
            _ => false,
        }
    }

    /// Shell-style "go back through prompt history" — the Up key.
    ///
    /// Rule (matches user spec): history only advances when the
    /// composer cursor is on the *top* line of the current text.
    /// Otherwise Up just moves the cursor up one line within the
    /// buffer. The first transition into history mode snapshots the
    /// live buffer into `staged_draft` so a later Down can restore
    /// it.
    pub(super) fn history_up(&mut self) {
        if !cursor_on_first_line(self.composer.text(), self.composer.cursor()) {
            self.composer.move_up();
            return;
        }
        // Buffer empty + queue non-empty → unqueue first (keeps the
        // existing pop-from-queue affordance the user already had).
        if self.prompt_history_cursor == 0 && self.composer.is_empty() && !self.queue.is_empty() {
            self.composer.set(self.queue.pop().unwrap());
            return;
        }
        if self.prompt_history.is_empty() {
            return;
        }
        if self.prompt_history_cursor == 0 {
            // Entering history mode — save the live draft so we can
            // restore it on the way back. `None` if the buffer was
            // empty (nothing meaningful to restore).
            let draft = self.composer.text().to_string();
            self.staged_draft = if draft.is_empty() { None } else { Some(draft) };
            self.prompt_history_cursor = 1;
            let idx = self.prompt_history.len() - 1;
            self.composer.set(self.prompt_history[idx].clone());
        } else if self.prompt_history_cursor < self.prompt_history.len() {
            self.prompt_history_cursor += 1;
            let idx = self.prompt_history.len() - self.prompt_history_cursor;
            self.composer.set(self.prompt_history[idx].clone());
        }
    }

    /// Counterpart to [`Self::history_up`]. Down only steps history
    /// when the composer cursor is on the *bottom* line of the
    /// current text. Otherwise it just moves the cursor down a line.
    /// Stepping past the newest entry (`cursor 1 → 0`) restores the
    /// `staged_draft` if there was one, else clears the composer.
    pub(super) fn history_down(&mut self) {
        if !cursor_on_last_line(self.composer.text(), self.composer.cursor()) {
            self.composer.move_down();
            return;
        }
        if self.prompt_history_cursor == 0 {
            // Not in history mode and already on the bottom line —
            // nothing to do (don't move_down because there's no row
            // below to move to).
            return;
        }
        self.prompt_history_cursor -= 1;
        if self.prompt_history_cursor == 0 {
            // Out of history — restore the saved draft, if any.
            match self.staged_draft.take() {
                Some(draft) => self.composer.set(draft),
                None => self.composer.clear(),
            }
        } else {
            let idx = self.prompt_history.len() - self.prompt_history_cursor;
            self.composer.set(self.prompt_history[idx].clone());
        }
    }

    /// If the composer no longer has an active `@partial` token, clear
    /// the dismissal latch so the next `@` reopens the popup. Otherwise
    /// (token still present) keep the existing state untouched.
    pub(super) fn refresh_at_dismiss(&mut self) {
        if self.composer.at_query().is_none() {
            self.at_dismissed = false;
            self.at_selected = 0;
            self.at_scroll = 0;
        }
    }

    /// Span of a completed `@`-tag whose right edge is at the cursor (for
    /// Backspace whole-tag delete), or `None`.
    pub(super) fn completed_tag_left(&self) -> Option<(usize, usize)> {
        completed_tag_span(
            self.composer.text(),
            self.composer.cursor(),
            &self.accepted_tags,
        )
    }

    /// Span of a completed `@`-tag whose left edge is at the cursor (for
    /// forward-`Delete` whole-tag delete), or `None`.
    pub(super) fn completed_tag_right(&self) -> Option<(usize, usize)> {
        completed_tag_span_forward(
            self.composer.text(),
            self.composer.cursor(),
            &self.accepted_tags,
        )
    }

    /// Reset the `@`-popup highlight + scroll window to the top. Called
    /// after any composer edit that changes the active `@`-query (typing
    /// narrows the list, so the selection should jump back to the first
    /// match). Harmless when no popup is active.
    pub(super) fn reset_at_window(&mut self) {
        self.at_selected = 0;
        self.at_scroll = 0;
    }

    /// Accept the currently-highlighted `@`-suggestion: replace the
    /// active `@partial` with the chosen path (trailing `/` for dirs).
    /// Returns true if a replacement was applied.
    ///
    /// `enter` distinguishes the two accept keys:
    /// - `Enter` (`enter = true`) **finalizes** any selection — file or
    ///   directory: appends a trailing space and closes the popup.
    /// - `Tab` (`enter = false`) finalizes a **file** the same way, but
    ///   on a **directory** *descends* — no trailing space, popup stays
    ///   open, and `at_query` now returns `<dir>/` so suggestions
    ///   re-query inside it.
    pub(super) fn accept_at_suggestion(&mut self, enter: bool) -> bool {
        let suggestions = self.at_suggestions();
        if suggestions.is_empty() {
            return false;
        }
        let idx = self.at_selected.min(suggestions.len() - 1);
        let sug = suggestions[idx].clone();
        self.composer.replace_at_token(&sug.replacement);
        self.at_selected = 0;
        self.at_scroll = 0;

        let finalize = enter || !sug.is_dir;
        if finalize {
            // Record spaced/special paths so the submit-time quoting
            // pass (file_tag) can wrap them — keeps the display clean
            // while the wire payload stays unambiguous.
            self.note_accepted_tag(&sug.replacement);
            // Trailing space terminates the tag and closes the popup.
            self.composer.insert_char(' ');
            self.at_dismissed = true;
        }
        // Dir-descend (Tab on a directory): `replacement` ends with `/`,
        // so the active `@`-query is now `<dir>/` and the popup re-walks
        // inside it. Nothing else to do.
        true
    }

    /// Render each `@`-tag expansion as a harness-automatic tool-call
    /// line in the chat (GOALS §1e). One line per tag, in the same
    /// `→ tool(path)` idiom the agent's own tools use, with a ✓/✗ + the
    /// detail (lines read / entries listed / why it was skipped).
    pub(super) fn push_tag_call_entries(
        &mut self,
        expansions: &[crate::tui::file_tag::TagExpansion],
    ) {
        for e in expansions {
            let mark = if e.ok { '✓' } else { '✗' };
            self.history.push(HistoryEntry::Plain {
                line: format!("  → {}({}) {mark} {}", e.tool, e.path, e.detail),
            });
        }
    }

    /// Remember an accepted tag path that contains a space or other
    /// shell-special character, so the submit-time pass can quote it.
    /// Plain paths need no tracking and are skipped.
    pub(super) fn note_accepted_tag(&mut self, path: &str) {
        if crate::tui::file_tag::needs_quoting(path)
            && !self.accepted_tags.contains(&path.to_string())
        {
            self.accepted_tags.push(path.to_string());
        }
    }

    pub(super) fn handle_key_normal(&mut self, key: KeyEvent) -> bool {
        // Arrow keys + Backspace/Delete still work in Normal mode —
        // they're convenient even for vim users. Char keys go through
        // the vim dispatcher below.
        match key.code {
            KeyCode::Esc => {
                // Already in Normal; clear any pending `g`.
                self.composer.set_pending_g(false);
                false
            }
            KeyCode::Enter => {
                self.composer.set_pending_g(false);
                // Shift+Enter / Alt+Enter inserts a newline regardless
                // of mode — composer is a chat input, not a vim
                // editor, and users expect newline-on-shift to work
                // even if they forgot to switch modes. Plain Enter
                // still submits (matches most TUIs).
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    self.composer.insert_char('\n');
                    return false;
                }
                self.complete_or_submit()
            }
            KeyCode::Left => {
                self.composer.move_left();
                self.composer.set_pending_g(false);
                false
            }
            KeyCode::Right => {
                self.composer.move_right();
                self.composer.set_pending_g(false);
                false
            }
            KeyCode::Up => {
                self.history_up();
                self.composer.set_pending_g(false);
                false
            }
            KeyCode::Down => {
                self.history_down();
                self.composer.set_pending_g(false);
                false
            }
            KeyCode::Char(ch) => {
                let was_pending_g = self.composer.pending_g();
                let pending_find = self.composer.pending_find();
                // Default: any char key clears the pending `g`/`f`/`F`;
                // the `g`/`f`/`F` arms below re-arm them if applicable.
                self.composer.set_pending_g(false);
                self.composer.set_pending_find(None);
                if let Some(forward) = pending_find {
                    if forward {
                        self.composer.find_char_forward(ch);
                    } else {
                        self.composer.find_char_backward(ch);
                    }
                    return false;
                }
                match ch {
                    'h' => self.composer.move_left(),
                    'l' => self.composer.move_right(),
                    'k' => self.history_up(),
                    'j' => self.history_down(),
                    'w' => self.composer.move_word_forward(false),
                    'W' => self.composer.move_word_forward(true),
                    'b' => self.composer.move_word_backward(false),
                    'B' => self.composer.move_word_backward(true),
                    '0' => self.composer.move_line_start(),
                    '$' => self.composer.move_line_end(),
                    'G' => self.composer.move_buffer_end(),
                    'g' => {
                        if was_pending_g {
                            self.composer.move_buffer_start();
                        } else {
                            self.composer.set_pending_g(true);
                        }
                    }
                    'f' => self.composer.set_pending_find(Some(true)),
                    'F' => self.composer.set_pending_find(Some(false)),
                    'i' => self.composer.set_vim_mode(VimMode::Insert),
                    'I' => {
                        self.composer.move_line_start();
                        self.composer.set_vim_mode(VimMode::Insert);
                    }
                    'a' => {
                        self.composer.move_right();
                        self.composer.set_vim_mode(VimMode::Insert);
                    }
                    'A' => {
                        self.composer.move_line_end();
                        self.composer.set_vim_mode(VimMode::Insert);
                    }
                    'x' => self.composer.delete_right(),
                    'D' => self.composer.delete_to_line_end(),
                    'C' => {
                        self.composer.delete_to_line_end();
                        self.composer.set_vim_mode(VimMode::Insert);
                    }
                    'o' => {
                        self.composer.open_below();
                        self.composer.set_vim_mode(VimMode::Insert);
                    }
                    'O' => {
                        self.composer.open_above();
                        self.composer.set_vim_mode(VimMode::Insert);
                    }
                    'd' => self
                        .composer
                        .set_vim_mode(VimMode::Operator(Operator::Delete)),
                    'c' => self
                        .composer
                        .set_vim_mode(VimMode::Operator(Operator::Change)),
                    _ => {}
                }
                false
            }
            _ => false,
        }
    }

    /// Operator-pending: we just saw `d` or `c`; the next key is the
    /// motion. `dd`/`cc` (doubled operator) deletes/changes the
    /// current line; `dw`/`cw` etc. apply the operator to the range
    /// covered by the motion. Any unrecognized key cancels back to
    /// Normal.
    pub(super) fn handle_key_operator(&mut self, key: KeyEvent, op: Operator) -> bool {
        let to_insert_on_change = matches!(op, Operator::Change);
        // Esc always cancels operator-pending.
        if matches!(key.code, KeyCode::Esc) {
            self.composer.set_vim_mode(VimMode::Normal);
            self.composer.set_pending_g(false);
            return false;
        }
        // Pending `g` for `dgg` / `cgg` chord.
        if let KeyCode::Char('g') = key.code {
            if self.composer.pending_g() {
                self.composer.delete_to_buffer_start();
                self.composer.set_pending_g(false);
                self.composer.set_vim_mode(if to_insert_on_change {
                    VimMode::Insert
                } else {
                    VimMode::Normal
                });
                return false;
            }
            self.composer.set_pending_g(true);
            return false;
        }
        self.composer.set_pending_g(false);
        let applied = match key.code {
            KeyCode::Char('w') => {
                self.composer.delete_word_forward(false);
                true
            }
            KeyCode::Char('W') => {
                self.composer.delete_word_forward(true);
                true
            }
            KeyCode::Char('b') => {
                self.composer.delete_word_backward(false);
                true
            }
            KeyCode::Char('B') => {
                self.composer.delete_word_backward(true);
                true
            }
            KeyCode::Char('$') => {
                self.composer.delete_to_line_end();
                true
            }
            KeyCode::Char('0') => {
                self.composer.delete_to_line_start();
                true
            }
            KeyCode::Char('G') => {
                self.composer.delete_to_buffer_end();
                true
            }
            KeyCode::Char('d') if matches!(op, Operator::Delete) => {
                self.composer.delete_current_line();
                true
            }
            KeyCode::Char('c') if matches!(op, Operator::Change) => {
                // `cc` changes the current line — semantically: clear
                // the line's content, leave the line itself, and enter
                // Insert. vim does the same.
                self.composer.move_line_start();
                self.composer.delete_to_line_end();
                true
            }
            _ => false,
        };
        if applied {
            self.composer.set_vim_mode(if to_insert_on_change {
                VimMode::Insert
            } else {
                VimMode::Normal
            });
        } else {
            // Unrecognized motion — cancel.
            self.composer.set_vim_mode(VimMode::Normal);
        }
        false
    }

    pub(super) fn complete_or_submit(&mut self) -> bool {
        if let Some(query) = self.slash_query() {
            if let Some(cmd) = slash_matches(query).first() {
                return self.execute_slash(**cmd);
            }
            return false;
        }
        self.submit_input()
    }

    pub(super) fn submit_input(&mut self) -> bool {
        let submitted = self.composer.text().trim().to_string();
        if submitted.is_empty() {
            return false;
        }

        // Submitting a new turn implies the user has finished reading
        // history — jump back to the live tail so they see the reply.
        self.chat_scroll_offset = 0;

        // Expand any `@path[:range]` tags into fenced file/dir blocks
        // before dispatch (GOALS §1e). The displayed user message keeps
        // the original `@`-form; only the wire payload gets inlined.
        // Autocompleted spaced paths are quoted on this submit copy so
        // the scanner reads them as one token (the composer stays clean).
        let quoted = crate::tui::file_tag::quote_tracked_tags(&submitted, &self.accepted_tags);
        let expanded = crate::tui::file_tag::expand_tags(&quoted, &self.launch.cwd);
        let wire = expanded.wire;
        // Per-tag entries are surfaced as harness-automatic tool calls in
        // the chat (GOALS §1e); the agent didn't invoke them, the
        // composer did. Cleared the accepted-tags tracker now that the
        // submit copy has consumed it.
        self.accepted_tags.clear();

        // If a turn is in flight, the daemon will queue this message
        // and fold it into the next inference call (GOALS §1c). Track
        // it locally so the user sees what's pending; cleared when the
        // daemon emits `ThinkingStarted` (its drain signal).
        let agent_busy = self.pending.is_some();
        if agent_busy {
            self.queue.push(submitted.clone());
            // Defer the tool-call entries so they render right after the
            // folded user message (on the next `ThinkingStarted`).
            self.queued_tag_calls.extend(expanded.expansions);
        } else {
            // No queueing — render as the user's turn immediately.
            self.history.push(HistoryEntry::User {
                text: submitted.clone(),
                timestamp: chrono::Local::now(),
            });
            self.push_tag_call_entries(&expanded.expansions);

            // Track for Up/Down history navigation.
            self.prompt_history.push(submitted.clone());
            self.prompt_history_cursor = 0;
            self.staged_draft = None;
        }

        self.ensure_agent_runner();
        match self.agent_runner.as_ref() {
            Some(Ok(runner)) => match runner.input_tx.try_send(wire) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.history.push(HistoryEntry::Plain {
                        line: "engine: input queue full — wait for the current turn to finish"
                            .to_string(),
                    });
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    self.history.push(HistoryEntry::Plain {
                        line: "engine: driver task has exited".to_string(),
                    });
                }
            },
            Some(Err(e)) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("engine: {e}"),
                });
            }
            None => {}
        }
        self.composer.clear();
        self.at_dismissed = false;
        self.at_selected = 0;
        self.at_scroll = 0;
        // Re-enter Normal mode on submit when vim is enabled, so the
        // composer is ready to be navigated without typing into it.
        // Mirror Insert otherwise.
        if self.composer.vim_enabled() {
            self.composer.set_vim_mode(VimMode::Insert);
        }
        false
    }
}

impl App {
    pub(super) fn is_ctrl_shift_y(&self, key: &KeyEvent) -> bool {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match key.code {
            KeyCode::Char('Y') => true,
            KeyCode::Char('y') => key.modifiers.contains(KeyModifiers::SHIFT),
            _ => false,
        }
    }

    /// True when the key event represents `Ctrl+Shift+C`. Same shape
    /// dance as `is_ctrl_shift_y` (kitty protocol vs legacy).
    pub(super) fn is_ctrl_shift_c(&self, key: &KeyEvent) -> bool {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match key.code {
            KeyCode::Char('C') => true,
            KeyCode::Char('c') => key.modifiers.contains(KeyModifiers::SHIFT),
            _ => false,
        }
    }
}

fn is_modifier_only(key: &KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Modifier(_) | KeyCode::CapsLock | KeyCode::NumLock | KeyCode::ScrollLock
    )
}

fn is_ws_byte(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Detect a *completed* `@`-tag whose right edge is exactly at `cursor`,
/// returning its `[start, cursor)` byte span. "Completed" means the tag
/// is terminated on the right (whitespace or end-of-buffer) and matches
/// one of: a quoted span `@"…"`, a tracked spaced path `@<accepted>`, or
/// a bare whitespace-free `@token`. Returns `None` when the cursor is
/// mid-tag or no completed tag ends here. This is what makes Backspace
/// at a tag's edge delete the whole tag atomically (GOALS §1e).
fn completed_tag_span(buffer: &str, cursor: usize, accepted: &[String]) -> Option<(usize, usize)> {
    if cursor == 0 {
        return None;
    }
    let bytes = buffer.as_bytes();
    let terminated = cursor >= buffer.len() || is_ws_byte(bytes[cursor]);
    if !terminated {
        return None;
    }

    // A — quoted: ends with a closing quote whose opener is `@"` at a
    // word boundary.
    if bytes[cursor - 1] == b'"'
        && let Some(qpos) = buffer[..cursor - 1].rfind('"')
        && qpos >= 1
        && bytes[qpos - 1] == b'@'
        && (qpos - 1 == 0 || is_ws_byte(bytes[qpos - 2]))
    {
        return Some((qpos - 1, cursor));
    }

    // B — tracked spaced path stored unquoted in the buffer: `@<accepted>`.
    // Longest first so a longer accepted path wins over a prefix.
    let mut tracked: Vec<&String> = accepted.iter().collect();
    tracked.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for p in tracked {
        let need = p.len() + 1;
        if cursor >= need {
            let at = cursor - need;
            if bytes[at] == b'@'
                && &buffer[at + 1..cursor] == p.as_str()
                && (at == 0 || is_ws_byte(bytes[at - 1]))
            {
                return Some((at, cursor));
            }
        }
    }

    // C — bare whitespace-free `@token`.
    if let Some(at) = buffer[..cursor].rfind('@') {
        let seg = &buffer[at + 1..cursor];
        if at + 1 < cursor
            && !seg.chars().any(char::is_whitespace)
            && (at == 0 || is_ws_byte(bytes[at - 1]))
        {
            return Some((at, cursor));
        }
    }
    None
}

/// Mirror of [`completed_tag_span`] for forward-`Delete`: detect a tag
/// whose left edge (`@`) is exactly at `cursor`, returning `[cursor, end)`.
fn completed_tag_span_forward(
    buffer: &str,
    cursor: usize,
    accepted: &[String],
) -> Option<(usize, usize)> {
    let bytes = buffer.as_bytes();
    if cursor >= buffer.len() || bytes[cursor] != b'@' {
        return None;
    }
    if !(cursor == 0 || is_ws_byte(bytes[cursor - 1])) {
        return None;
    }
    let rest = &buffer[cursor + 1..];

    // A — quoted.
    if rest.starts_with('"') {
        if let Some(close_rel) = buffer[cursor + 2..].find('"') {
            let mut end = cursor + 2 + close_rel + 1;
            if buffer[end..].starts_with(':') {
                let rs = end + 1;
                let re = buffer[rs..]
                    .find(char::is_whitespace)
                    .map(|o| rs + o)
                    .unwrap_or(buffer.len());
                if re > rs
                    && buffer[rs..re]
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '-')
                {
                    end = re;
                }
            }
            return Some((cursor, end));
        }
        return None;
    }

    // B — tracked spaced path.
    let mut tracked: Vec<&String> = accepted.iter().collect();
    tracked.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for p in tracked {
        if rest.starts_with(p.as_str()) {
            let end = cursor + 1 + p.len();
            if end >= buffer.len() || is_ws_byte(bytes[end]) {
                return Some((cursor, end));
            }
        }
    }

    // C — bare.
    let end = rest
        .find(char::is_whitespace)
        .map(|o| cursor + 1 + o)
        .unwrap_or(buffer.len());
    if end > cursor + 1 {
        Some((cursor, end))
    } else {
        None
    }
}

/// Render a toast over the status-line rect. Single line; left-padded
/// one cell; foreground color encodes intent (green/red/grey).
pub(super) fn accepts_key(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}
/// True when `cursor` falls on the first line of `text` (i.e. there's
/// no `\n` in `text[..cursor]`). Used by history navigation to decide
/// "is the user at the top of the buffer?" — only then does Up step
/// into prompt history, otherwise it moves the cursor up one line.
fn cursor_on_first_line(text: &str, cursor: usize) -> bool {
    !text[..cursor.min(text.len())].contains('\n')
}

/// True when `cursor` falls on the last line of `text` (no `\n` after
/// it). Used by history navigation: Down only steps history when the
/// cursor is at the bottom of the buffer; otherwise it moves the
/// composer cursor down a line.
fn cursor_on_last_line(text: &str, cursor: usize) -> bool {
    let after = &text[cursor.min(text.len())..];
    !after.contains('\n')
}

#[cfg(test)]
mod tag_delete_tests {
    use super::{completed_tag_span, completed_tag_span_forward};

    fn none() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn bare_tag_at_eof_is_deletable() {
        // `@foo` with cursor at end (terminated by EOF).
        let b = "@foo";
        assert_eq!(completed_tag_span(b, b.len(), &none()), Some((0, 4)));
    }

    #[test]
    fn bare_tag_before_space_is_deletable() {
        // `@foo bar`, cursor right after `@foo` (index 4, space follows).
        assert_eq!(completed_tag_span("@foo bar", 4, &none()), Some((0, 4)));
    }

    #[test]
    fn cursor_mid_tag_is_not_deletable() {
        // cursor inside `@foo` (index 2) → normal char delete.
        assert_eq!(completed_tag_span("@foo", 2, &none()), None);
    }

    #[test]
    fn trailing_space_is_not_a_tag_edge() {
        // `@foo ` cursor after the space → first backspace removes space.
        assert_eq!(completed_tag_span("@foo ", 5, &none()), None);
    }

    #[test]
    fn quoted_tag_is_deletable_as_a_whole() {
        let b = "@\"my file.rs\"";
        assert_eq!(completed_tag_span(b, b.len(), &none()), Some((0, b.len())));
    }

    #[test]
    fn tracked_spaced_path_is_deletable() {
        let accepted = vec!["src/my file.rs".to_string()];
        let b = "@src/my file.rs";
        assert_eq!(
            completed_tag_span(b, b.len(), &accepted),
            Some((0, b.len()))
        );
    }

    #[test]
    fn email_at_is_not_a_tag() {
        // `user@host` — `@` not at a word boundary.
        assert_eq!(completed_tag_span("user@host", 9, &none()), None);
    }

    #[test]
    fn forward_delete_bare_tag() {
        // cursor at the `@` of `@foo bar`.
        assert_eq!(
            completed_tag_span_forward("@foo bar", 0, &none()),
            Some((0, 4))
        );
    }

    #[test]
    fn forward_delete_quoted_tag() {
        let b = "@\"my file.rs\" rest";
        assert_eq!(completed_tag_span_forward(b, 0, &none()), Some((0, 13)));
    }
}
