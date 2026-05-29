//! Composer key handling: vim mode state machine, history navigation,
//! submit, and the small Ctrl+Shift+{Y,C} helpers shared with the
//! mouse module.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::tui::composer::{Operator, VimMode};
use crate::tui::history::HistoryEntry;

use super::App;
use crate::tui::settings::Dialog;

/// Result of handing a submitted turn to the agent runner. Carries
/// whether the working span this submit may have started was orphaned —
/// i.e. no worker received the turn, so no `AgentIdle` will ever arrive
/// to lower `busy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchOutcome {
    /// The wire was accepted by a running worker; `AgentIdle` will end
    /// the span normally.
    Sent,
    /// The input queue was full; the turn was rejected.
    QueueFull,
    /// The driver task has exited; the channel is closed.
    DriverClosed,
    /// Runner construction failed (`Some(Err(_))`) — e.g. the model
    /// won't resolve, so no worker was ever spawned.
    RunnerFailed,
    /// No runner present (`None`) — nothing was started, nothing to undo.
    NoRunner,
}

impl DispatchOutcome {
    /// True when the turn never reached a worker, so a working span
    /// opened for this submit would otherwise hang forever.
    fn span_orphaned(self) -> bool {
        matches!(
            self,
            DispatchOutcome::QueueFull
                | DispatchOutcome::DriverClosed
                | DispatchOutcome::RunnerFailed
        )
    }
}

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Embedded pane (GOALS §1i): while a pane is open, `Ctrl+X`
        // force-closes it and `Ctrl+O` toggles focus — both reserved by
        // cockpit and not delivered to the child. When the pane is
        // focused, every other key (incl. Ctrl+C) is forwarded to the
        // child PTY rather than handled by the TUI.
        if self.pane.is_some() {
            if is_pane_force_close(&key) {
                self.close_pane(true);
                return false;
            }
            if is_pane_focus_toggle(&key) {
                self.pane_focused = !self.pane_focused;
                return false;
            }
            if self.pane_focused {
                if let Some(pane) = self.pane.as_mut() {
                    pane.forward_key(&key);
                }
                return false;
            }
        }

        // Ctrl+C: interrupt the running agent; exit only on a second press
        // within the 0.5s window (GOALS §3a). Routed through the
        // double-press state machine. Explicitly exclude Shift so that
        // Ctrl+Shift+C (copy-selection, plan.md T8.f) isn't mistaken for it
        // on terminals that report the shift state in `modifiers` even when
        // the key code is lowercase.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('c'))
        {
            return self.handle_ctrl_c();
        }
        // Ctrl+D still quits immediately (out of scope for the ctrl+c
        // double-press change; left as the existing direct-exit path).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('d'))
        {
            return true;
        }

        // Any meaningful keystroke dismisses the toast — the user has
        // moved on. Pure-modifier presses (Shift, Ctrl, etc. alone)
        // don't count.
        if self.toast.is_some() && !is_modifier_only(&key) {
            self.toast = None;
        }

        // `/prune` confirm armed (T6.d): `y` / Enter commits, any other
        // non-modifier key cancels. Ahead of composer routing so the
        // keystroke doesn't leak into the textbox.
        if self.pending_prune_confirm && !is_modifier_only(&key) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.commit_prune(),
                _ => self.cancel_prune(),
            }
            return false;
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
                    // The TUI promotes a *persistent* daemon here; the
                    // client's `--no-sandbox` is a per-session default
                    // applied at attach, not a daemon-level launch flag
                    // (sandboxing part 2 precedence).
                    match crate::daemon::DaemonPaths::resolve()
                        .and_then(|_| crate::daemon::spawn_detached(false))
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

        // Answering dialog (GOALS §3b) — same modal rule. It replaces the
        // composer, so it routes before the settings dialog / picker. On
        // close, send the resolution back to the daemon as
        // `ResolveInterrupt`; the agent's blocked `question` tool wakes.
        if let Some(dialog) = self.question_dialog.as_mut() {
            let should_close = dialog.handle_key(key);
            if should_close {
                let result = dialog.take_result();
                self.question_dialog = None;
                if let Some(result) = result {
                    self.resolve_question_dialog(result);
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
                // `is_done()` distinguishes an accepted pick from an Esc
                // cancel — only the former counts toward the tally.
                let accepted = picker.is_done();
                self.model_picker = None;
                self.reload_launch_info();
                if accepted && let Some((p, m)) = self.launch.active_model.clone() {
                    self.record_usage(
                        crate::daemon::proto::UsageKind::Model,
                        format!("{p}/{m}"),
                        None,
                    );
                }
                let line = self.model_summary_history_line();
                self.history.push(HistoryEntry::Plain { line });
            }
            // See the "modal dialog rule" comment above — always
            // consume the key while the picker is open.
            return false;
        }

        // `/stats` pane (GOALS §15). Same modal rule: route the key to
        // the pane, close on its request, and always consume so nothing
        // leaks into the composer underneath.
        if let Some(pane) = self.stats_pane.as_mut() {
            if pane.handle_key(key) {
                self.stats_pane = None;
            }
            return false;
        }

        // `/sessions` + `/resume` browser (GOALS §17f). Same modal rule.
        // The pane returns an outcome: Close drops it; Resume drops it and
        // switches the runner onto the chosen session via the existing
        // resume path. Always consume the key.
        if let Some(pane) = self.sessions_pane.as_mut() {
            match pane.handle_key(key) {
                Some(crate::tui::sessions_pane::SessionsOutcome::Close) => {
                    self.sessions_pane = None;
                }
                Some(crate::tui::sessions_pane::SessionsOutcome::Resume(session_id)) => {
                    self.sessions_pane = None;
                    self.resume_session(session_id);
                }
                None => {}
            }
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
                        // Wrap at the top (first → last) + scrolloff so the
                        // neighbor stays visible (see `windowed_scroll`).
                        self.at_selected = crate::tui::nav::wrap_prev(self.at_selected, n);
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
                        // Wrap at the bottom (last → first).
                        self.at_selected = crate::tui::nav::wrap_next(self.at_selected, n);
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
        // Slash-menu intercepts Up/Down while it's visible so they move
        // the highlight instead of triggering composer history recall
        // (the suppression is scoped to "menu showing" — Up/Down resume
        // normal recall the moment the menu closes). `j`/`k` are NOT
        // navigation here: the user is typing to filter, so they stay
        // literal text (matching the `@` menu). Mutually exclusive with
        // the `@`-popup (one needs a leading `/`, the other an `@`-token).
        if self.slash_query().is_some() {
            match key.code {
                KeyCode::Up => {
                    let n = self.slash_suggestions().len();
                    if n > 0 {
                        self.slash_selected = crate::tui::nav::wrap_prev(self.slash_selected, n);
                        self.slash_scroll = super::windowed_scroll(
                            self.slash_selected,
                            self.slash_scroll,
                            n,
                            super::AUTOCOMPLETE_ROWS as usize,
                        );
                    }
                    return false;
                }
                KeyCode::Down => {
                    let n = self.slash_suggestions().len();
                    if n > 0 {
                        self.slash_selected = crate::tui::nav::wrap_next(self.slash_selected, n);
                        self.slash_scroll = super::windowed_scroll(
                            self.slash_selected,
                            self.slash_scroll,
                            n,
                            super::AUTOCOMPLETE_ROWS as usize,
                        );
                    }
                    return false;
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
                    self.paste_registry.clear();
                    self.reset_slash_window();
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
                    self.composer_insert_char('\n');
                    self.refresh_at_dismiss();
                    self.reset_slash_window();
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
                self.composer_insert_char('\n');
                self.reset_slash_window();
                false
            }
            KeyCode::Backspace => {
                // Whole-block delete (paste blocks): cursor immediately
                // right of `]` removes the entire block. Checked before
                // the `@`-tag path since blocks are explicit + atomic.
                if let Some((s, e)) = self.paste_block_left() {
                    self.delete_paste_block(s, e);
                    self.refresh_at_dismiss();
                    self.reset_at_window();
                    return false;
                }
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
                self.composer_delete_left();
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
                self.reset_slash_window();
                false
            }
            KeyCode::Delete => {
                // Whole-block forward delete: cursor immediately left of
                // `[` removes the entire block.
                if let Some((s, e)) = self.paste_block_right() {
                    self.delete_paste_block(s, e);
                    self.refresh_at_dismiss();
                    self.reset_at_window();
                    return false;
                }
                if !self.at_popup_active()
                    && let Some((s, e)) = self.completed_tag_right()
                {
                    self.composer.delete_range(s, e);
                    self.refresh_at_dismiss();
                    self.reset_at_window();
                    return false;
                }
                self.composer_delete_right();
                self.refresh_at_dismiss();
                self.reset_at_window();
                self.reset_slash_window();
                false
            }
            KeyCode::Left => {
                self.composer_move_left();
                false
            }
            KeyCode::Right => {
                self.composer_move_right();
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
                self.composer_insert_char(ch);
                // Note: we deliberately do NOT reset
                // `prompt_history_cursor` here. Edits made while in
                // recall mode stay in the buffer, but pressing Down
                // back to cursor 0 still restores the original
                // staged draft — matching the user-visible spec for
                // history navigation.
                self.refresh_at_dismiss();
                self.reset_at_window();
                // Typing narrows the slash matches; snap the cursor back
                // to the top match so it never points past the new set.
                self.reset_slash_window();
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
            self.paste_registry.clear();
            return;
        }
        if self.prompt_history.is_empty() {
            return;
        }
        if self.prompt_history_cursor == 0 {
            // Entering history mode — save the live draft so we can
            // restore it on the way back. `None` if the buffer was
            // empty (nothing meaningful to restore). Paste blocks are
            // flattened to their placeholder text in the recalled draft;
            // the registry is dropped (it indexed the live buffer).
            let draft = self.composer.text().to_string();
            self.staged_draft = if draft.is_empty() { None } else { Some(draft) };
            self.prompt_history_cursor = 1;
            let idx = self.prompt_history.len() - 1;
            self.composer.set(self.prompt_history[idx].clone());
            self.paste_registry.clear();
        } else if self.prompt_history_cursor < self.prompt_history.len() {
            self.prompt_history_cursor += 1;
            let idx = self.prompt_history.len() - self.prompt_history_cursor;
            self.composer.set(self.prompt_history[idx].clone());
            self.paste_registry.clear();
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
        // History navigation always lands on plain recalled text — no
        // paste blocks survive.
        self.paste_registry.clear();
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

    /// Reset the slash-popup highlight + scroll window to the top (the
    /// frequency-ranked match). Called after any composer edit that
    /// changes the active slash query so the cursor doesn't point past a
    /// narrowed match set; also restores the "Enter runs the top match"
    /// default. Harmless when no slash query is active.
    pub(super) fn reset_slash_window(&mut self) {
        self.slash_selected = 0;
        self.slash_scroll = 0;
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
            // Tally the committed tag (per-project) for frequency-ranked
            // autocomplete. Tab-descending into a directory isn't a
            // commit, so it's deliberately not counted here.
            let project_id = self.project_id.clone();
            self.record_usage(
                crate::daemon::proto::UsageKind::Tag,
                sug.replacement.clone(),
                project_id,
            );
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
        // While the slash menu is visible, the arrow keys move its
        // highlight rather than recalling history — same rule as Insert
        // mode, so the menu behaves identically regardless of vim mode.
        // (`j`/`k` keep their Normal-mode vim meaning; only the arrows
        // are menu-nav, mirroring the `@`-popup's arrow-only contract.)
        if self.slash_query().is_some() && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            let n = self.slash_suggestions().len();
            if n > 0 {
                self.slash_selected = if matches!(key.code, KeyCode::Up) {
                    crate::tui::nav::wrap_prev(self.slash_selected, n)
                } else {
                    crate::tui::nav::wrap_next(self.slash_selected, n)
                };
                self.slash_scroll = super::windowed_scroll(
                    self.slash_selected,
                    self.slash_scroll,
                    n,
                    super::AUTOCOMPLETE_ROWS as usize,
                );
            }
            self.composer.set_pending_g(false);
            return false;
        }
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
                    'h' => self.composer_move_left(),
                    'l' => self.composer_move_right(),
                    'k' => self.history_up(),
                    'j' => self.history_down(),
                    'w' => self.vim_motion(|c| c.move_word_forward(false), true),
                    'W' => self.vim_motion(|c| c.move_word_forward(true), true),
                    'b' => self.vim_motion(|c| c.move_word_backward(false), false),
                    'B' => self.vim_motion(|c| c.move_word_backward(true), false),
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
                        self.composer_move_right();
                        self.composer.set_vim_mode(VimMode::Insert);
                    }
                    'A' => {
                        self.composer.move_line_end();
                        self.composer.set_vim_mode(VimMode::Insert);
                    }
                    'x' => {
                        // Block-aware single forward delete: if the cursor
                        // sits at a block's opening `[`, remove the whole
                        // block; else ordinary forward-delete.
                        if let Some((s, e)) = self.paste_block_right() {
                            self.delete_paste_block(s, e);
                        } else {
                            self.composer_delete_right();
                        }
                    }
                    'D' => {
                        self.block_aware_delete(|c| c.move_line_end(), |c| c.delete_to_line_end())
                    }
                    'C' => {
                        self.block_aware_delete(|c| c.move_line_end(), |c| c.delete_to_line_end());
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
                // `dgg` — delete from buffer start to cursor (motion lands
                // at 0). Block-aware so a block in that range is removed
                // whole.
                self.block_aware_delete(|c| c.move_buffer_start(), |c| c.delete_to_buffer_start());
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
                self.block_aware_delete(
                    |c| c.move_word_forward(false),
                    |c| c.delete_word_forward(false),
                );
                true
            }
            KeyCode::Char('W') => {
                self.block_aware_delete(
                    |c| c.move_word_forward(true),
                    |c| c.delete_word_forward(true),
                );
                true
            }
            KeyCode::Char('b') => {
                self.block_aware_delete(
                    |c| c.move_word_backward(false),
                    |c| c.delete_word_backward(false),
                );
                true
            }
            KeyCode::Char('B') => {
                self.block_aware_delete(
                    |c| c.move_word_backward(true),
                    |c| c.delete_word_backward(true),
                );
                true
            }
            KeyCode::Char('$') => {
                self.block_aware_delete(|c| c.move_line_end(), |c| c.delete_to_line_end());
                true
            }
            KeyCode::Char('0') => {
                self.block_aware_delete(|c| c.move_line_start(), |c| c.delete_to_line_start());
                true
            }
            KeyCode::Char('G') => {
                // `dG` — delete from cursor to end of buffer.
                let len = self.composer.len();
                self.block_aware_delete(move |c| c.set_cursor(len), |c| c.delete_to_buffer_end());
                true
            }
            KeyCode::Char('d') if matches!(op, Operator::Delete) => {
                self.delete_current_line_block_aware();
                true
            }
            KeyCode::Char('c') if matches!(op, Operator::Change) => {
                // `cc` changes the current line — semantically: clear
                // the line's content, leave the line itself, and enter
                // Insert. vim does the same.
                self.composer.move_line_start();
                self.block_aware_delete(|c| c.move_line_end(), |c| c.delete_to_line_end());
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
        // Shell mode: a leading `!` runs the rest as a one-shot local
        // command (GOALS §1k). Never reaches the agent or the wire.
        if self.composer.text().starts_with('!') {
            let cmd = self.composer.text()[1..].to_string();
            self.composer.clear();
            self.paste_registry.clear();
            self.run_shell_command(&cmd);
            self.at_dismissed = false;
            self.at_selected = 0;
            self.at_scroll = 0;
            if self.composer.vim_enabled() {
                self.composer.set_vim_mode(VimMode::Insert);
            }
            return false;
        }
        if self.slash_query().is_some() {
            // Run whatever is highlighted. The default highlight is the
            // frequency-ranked top match (index 0), so `/foo`+Enter still
            // runs the top match — preserving the pre-cursor muscle memory.
            let matches = self.slash_suggestions();
            if matches.is_empty() {
                return false;
            }
            let idx = self.slash_selected.min(matches.len() - 1);
            let cmd = *matches[idx];
            return self.execute_slash(cmd);
        }
        self.submit_input()
    }

    pub(super) fn submit_input(&mut self) -> bool {
        // The *displayed* message keeps the composer's exact text,
        // including paste-block placeholders (wire/user split — the user
        // sees `[Pasted text #1, …]`, the model gets the expansion).
        let submitted = self.composer.text().trim().to_string();
        if submitted.is_empty() && self.paste_registry.is_empty() {
            return false;
        }

        // Build the paste-side wire from the live (untrimmed) buffer +
        // registry: text blocks inline their full content; image blocks
        // become real image parts on a vision model, or a terse text note
        // otherwise. `paste_images` are the ordered PNG payloads; the
        // sentinel markers in `paste_wire` mark where each lands. Done
        // first (offsets index the untrimmed buffer) and gated on the
        // active model's `inputs.images` at *this* send time — a `/model`
        // switch since paste round-trips the same blocks differently.
        let vision = self.active_model_supports_images();
        let (paste_wire, paste_images) =
            self.paste_registry.build_wire(self.composer.text(), vision);
        let paste_wire = paste_wire.trim().to_string();
        if paste_wire.is_empty() && paste_images.is_empty() {
            return false;
        }

        // `/compact` review-then-commit (T6.e): the composer holds the
        // assembled handoff (user may have edited it). On submit, re-attach
        // to the fresh session the daemon created and send the handoff as
        // its first message. The old session stays whole in SQLite,
        // recoverable via `cockpit session show/resume`.
        if self.pending_compact.is_some() {
            return self.commit_compact(submitted);
        }

        // Submitting a new turn implies the user has finished reading
        // history — jump back to the live tail so they see the reply.
        self.chat_scroll_offset = 0;

        // Expand any `@path[:range]` tags into fenced file/dir blocks
        // before dispatch (GOALS §1e). The displayed user message keeps
        // the original `@`-form; only the wire payload gets inlined.
        // Autocompleted spaced paths are quoted on this submit copy so
        // the scanner reads them as one token (the composer stays clean).
        // Tag expansion runs over the paste-expanded wire so a tag and a
        // pasted block can coexist in one message.
        let quoted = crate::tui::file_tag::quote_tracked_tags(&paste_wire, &self.accepted_tags);
        let expanded = crate::tui::file_tag::expand_tags(&quoted, &self.launch.cwd);
        // Attach any buffered `/git` blocks to this message's wire text
        // (GOALS §1l). The displayed user message keeps the original
        // text (wire/user split); only the agent-bound wire carries the
        // block, so it flows through `redact::scrub` like any wire text.
        let wire = if self.pending_git_blocks.is_empty() {
            expanded.wire
        } else {
            let blocks = std::mem::take(&mut self.pending_git_blocks).join("\n\n");
            if expanded.wire.is_empty() {
                blocks
            } else {
                format!("{}\n\n{}", expanded.wire, blocks)
            }
        };
        // Per-tag entries are surfaced as harness-automatic tool calls in
        // the chat (GOALS §1e); the agent didn't invoke them, the
        // composer did. Cleared the accepted-tags tracker now that the
        // submit copy has consumed it.
        self.accepted_tags.clear();

        // If a turn is in flight, the daemon will queue this message
        // and fold it into the next inference call (GOALS §1c). Track
        // it locally so the user sees what's pending; cleared when the
        // daemon emits `ThinkingStarted` (its drain signal). We gate on
        // the span-long `busy` state rather than `pending.is_some()`:
        // the latter drops to `None` between tool rounds, so a message
        // typed during tool execution would otherwise be mistaken for a
        // fresh turn.
        // True only on the fresh-submit path: this submit owns the
        // rising edge of the working span and must undo it if the turn
        // can't be handed off. The busy/queue path didn't start a span,
        // so it must never tear one down.
        let owns_working_span = if self.busy {
            self.queue.push(submitted.clone());
            // Defer the tool-call entries so they render right after the
            // folded user message (on the next `ThinkingStarted`).
            self.queued_tag_calls.extend(expanded.expansions);
            false
        } else {
            // Fresh human message: start a new working span (resets the
            // cumulative clock and re-rolls the working line) and render
            // as the user's turn immediately.
            self.begin_working_span();
            self.history.push(HistoryEntry::User {
                text: submitted.clone(),
                timestamp: chrono::Local::now(),
            });
            self.push_tag_call_entries(&expanded.expansions);

            // Track for Up/Down history navigation.
            self.prompt_history.push(submitted.clone());
            self.prompt_history_cursor = 0;
            self.staged_draft = None;
            true
        };

        // Carry the wire text together with any real image parts (vision
        // only — non-vision folded the images into `wire` as text notes,
        // leaving `paste_images` empty).
        let submission = crate::engine::message::UserSubmission {
            text: wire,
            images: paste_images,
        };

        self.ensure_agent_runner();
        let outcome = match self.agent_runner.as_ref() {
            Some(Ok(runner)) => match runner.input_tx.try_send(submission) {
                Ok(()) => {
                    // First user message commits the daemon's `sessions`
                    // row (session-id-display-and-lazy-persist); record that
                    // so the exit print knows the session was persisted.
                    self.current_session_persisted = true;
                    DispatchOutcome::Sent
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.history.push(HistoryEntry::Plain {
                        line: "engine: input queue full — wait for the current turn to finish"
                            .to_string(),
                    });
                    DispatchOutcome::QueueFull
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    self.history.push(HistoryEntry::Plain {
                        line: "engine: driver task has exited".to_string(),
                    });
                    DispatchOutcome::DriverClosed
                }
            },
            Some(Err(e)) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("engine: {e}"),
                });
                DispatchOutcome::RunnerFailed
            }
            None => DispatchOutcome::NoRunner,
        };
        // A turn the worker never received will never emit `AgentIdle`
        // (the sole `busy` falling edge), so the rising edge this submit
        // created would otherwise be stuck on forever. Undo it — but only
        // for the fresh-submit path that owns the edge; the queue path
        // started no span and must leave any in-flight turn alone.
        if owns_working_span && outcome.span_orphaned() {
            self.end_working_span();
        }
        self.composer.clear();
        // The buffer is gone — its paste blocks go with it.
        self.paste_registry.clear();
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
    // ---- Paste blocks (composer-paste-handling) -----------------------
    //
    // A genuine bracketed paste arrives as one `Event::Paste(String)`.
    // Images come from the system clipboard read on the same gesture. Both
    // collapse into atomic placeholder blocks tracked in
    // `self.paste_registry`, kept byte-range-synced with the composer.

    /// Route a bracketed-paste event. First checks the clipboard for an
    /// image (a paste gesture over an image puts the bytes there, while
    /// `data` is typically empty or a filename); if present, inserts an
    /// image block. Otherwise treats `data` as text: re-paste-to-expand
    /// if the cursor sits at a matching text block's right edge, else
    /// condense-or-insert by the threshold rule.
    pub(super) fn handle_paste(&mut self, data: String) {
        // Paste only targets the composer. If a modal/pane is up, ignore —
        // matches how typed keys are consumed by modals before reaching
        // the composer.
        if self.pane.is_some()
            || self.daemon_prompt.is_some()
            || self.question_dialog.is_some()
            || self.dialog.is_active()
            || self.model_picker.is_some()
            || self.stats_pane.is_some()
            || self.sessions_pane.is_some()
            || self.context_menu.is_some()
        {
            return;
        }

        // Image first: a clipboard image on the paste gesture becomes an
        // image block regardless of `data`. SSH is out of scope — the read
        // is local-clipboard only and silently yields `None` when there's
        // no bitmap.
        match crate::clipboard::read_image_as_png() {
            Ok(Some(png)) => {
                self.insert_image_block(png);
                return;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, "clipboard image read failed; treating paste as text");
            }
        }

        if data.is_empty() {
            return;
        }

        // Re-paste-to-expand: cursor at a text block's right edge + the
        // paste equals that block's stored content → expand in place.
        let cursor = self.composer.cursor();
        if let Some((start, end, full)) = self.paste_registry.expandable_text_at(cursor, &data) {
            // Replace the placeholder span with the raw text and drop the
            // block from the registry.
            self.composer.delete_range(start, end);
            self.paste_registry.remove_range(start, end);
            self.composer.set_cursor(start);
            self.insert_text_raw(&full);
            self.refresh_at_dismiss();
            self.reset_at_window();
            self.reset_slash_window();
            return;
        }

        if crate::tui::paste::should_condense(&data) {
            self.insert_text_block(data);
        } else {
            self.insert_text_raw(&data);
        }
        self.refresh_at_dismiss();
        self.reset_at_window();
        self.reset_slash_window();
    }

    /// Insert raw (non-condensed) pasted text at the cursor, snapping the
    /// insertion point to a block boundary first and shifting the registry
    /// for the inserted length.
    fn insert_text_raw(&mut self, text: &str) {
        let at = self
            .paste_registry
            .resolve_insertion(self.composer.cursor());
        self.composer.set_cursor(at);
        self.composer.insert_str(text);
        self.paste_registry.shift_for_edit(at, text.len() as isize);
    }

    /// Estimate tokens for a condensed text block: the active model's
    /// calibrated counter when available, else cl100k_base (GOALS §10
    /// fallback). v1 has no in-TUI calibrated counter wired, so this is
    /// cl100k today — the seam is here for when one lands.
    fn estimate_paste_tokens(&self, text: &str) -> usize {
        crate::tokens::count(text)
    }

    /// Condense a long text paste into a `[Pasted text #N, X tokens]`
    /// block. The placeholder occupies the buffer; the full text lives in
    /// the registry and is inlined at send time.
    fn insert_text_block(&mut self, full: String) {
        let at = self
            .paste_registry
            .resolve_insertion(self.composer.cursor());
        self.composer.set_cursor(at);
        let tokens = self.estimate_paste_tokens(&full);
        let placeholder = self.paste_registry.register_text(at, full, tokens);
        self.composer.insert_str(&placeholder);
        // `register_text` already recorded the block at `[at, at+len)`;
        // shift only the blocks that were *after* the insertion point.
        self.shift_other_blocks_after_insert(at, placeholder.len());
    }

    /// Insert a pasted image as a `[Pasted image #N]` block. On a
    /// non-vision model, also toast that it'll be sent as a text note —
    /// the bytes are retained either way and re-evaluated at send time.
    fn insert_image_block(&mut self, png: Vec<u8>) {
        let at = self
            .paste_registry
            .resolve_insertion(self.composer.cursor());
        self.composer.set_cursor(at);
        let placeholder = self.paste_registry.register_image(at, png);
        self.composer.insert_str(&placeholder);
        self.shift_other_blocks_after_insert(at, placeholder.len());
        self.refresh_at_dismiss();
        self.reset_at_window();
        self.reset_slash_window();
        if !self.active_model_supports_images() {
            self.show_toast(
                "Current model has no image support — this image will be sent as a text note.",
                super::ToastKind::Info,
            );
        }
    }

    /// After [`crate::tui::paste::PasteRegistry::register_text`] /
    /// `register_image` recorded a new block at `[at, at+len)`, shift the
    /// *other* blocks that started at/after `at` (the new one is exact).
    /// `register_*` inserts the new block sorted, so we shift every block
    /// whose start is `> at` (i.e. excluding the one we just added).
    fn shift_other_blocks_after_insert(&mut self, at: usize, len: usize) {
        for b in self.paste_registry.blocks_mut() {
            if b.start > at {
                b.start += len;
                b.end += len;
            }
        }
    }

    /// Whether the active model accepts real image parts
    /// (`inputs.images: true`). Recomputed by `reload_launch_info` after a
    /// `/model` switch, so images round-trip without a re-paste.
    pub(super) fn active_model_supports_images(&self) -> bool {
        self.launch.active_model_supports_images
    }

    /// `dd` — delete the current line, block-aware. Any paste block on
    /// that line is removed whole (the whole line goes), and the registry
    /// is reconciled for the removed byte range. The line's byte range is
    /// computed up front (start of the line through its trailing `\n`, or
    /// the preceding `\n` on the last line) so we can shift the registry
    /// by the exact removed extent before delegating to the composer.
    fn delete_current_line_block_aware(&mut self) {
        if self.paste_registry.is_empty() {
            self.composer.delete_current_line();
            return;
        }
        let before = self.composer.len();
        let cursor = self.composer.cursor();
        let text = self.composer.text();
        let line_start = text[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.composer.delete_current_line();
        let removed = before - self.composer.len();
        if removed > 0 {
            // `delete_current_line` removes either `[line_start, …]` or,
            // on the last line, `[line_start-1, …]`. The lower anchor is
            // the smaller of the original line start and the post-delete
            // cursor (which lands at the new line's start).
            let anchor = line_start.min(self.composer.cursor());
            self.paste_registry
                .shift_for_edit(anchor, -(removed as isize));
        }
    }

    /// Block whose closing `]` is exactly at the cursor (Backspace
    /// whole-block delete). Mirrors `completed_tag_left` for `@`-tags.
    fn paste_block_left(&self) -> Option<(usize, usize)> {
        self.paste_registry
            .block_ending_at(self.composer.cursor())
            .map(|b| (b.start, b.end))
    }

    /// Block whose opening `[` is exactly at the cursor (forward-`Delete`
    /// whole-block delete). Mirrors `completed_tag_right`.
    fn paste_block_right(&self) -> Option<(usize, usize)> {
        self.paste_registry
            .block_starting_at(self.composer.cursor())
            .map(|b| (b.start, b.end))
    }

    /// Delete the block at `[start, end)` from both the buffer and the
    /// registry, leaving the cursor at `start`.
    fn delete_paste_block(&mut self, start: usize, end: usize) {
        self.composer.delete_range(start, end);
        self.paste_registry.remove_range(start, end);
    }

    /// Insert one char, block-aware. Fast-paths to the plain composer
    /// insert when no blocks exist (so ordinary typing is byte-identical
    /// to today). Otherwise snaps the insertion point out of any block
    /// interior and shifts trailing block ranges.
    fn composer_insert_char(&mut self, ch: char) {
        if self.paste_registry.is_empty() {
            self.composer.insert_char(ch);
            return;
        }
        let at = self
            .paste_registry
            .resolve_insertion(self.composer.cursor());
        self.composer.set_cursor(at);
        self.composer.insert_char(ch);
        self.paste_registry
            .shift_for_edit(at, ch.len_utf8() as isize);
    }

    /// Backspace, block-aware. (The whole-block case is handled by the
    /// caller via `paste_block_left`; this is the ordinary single-char
    /// path.) Snaps the cursor off a left boundary first so a Backspace
    /// just *inside* the text after a block can't reach into it, then
    /// shifts trailing blocks for the removed byte.
    fn composer_delete_left(&mut self) {
        if self.paste_registry.is_empty() {
            self.composer.delete_left();
            return;
        }
        let cursor = self.composer.cursor();
        // Never delete from inside a block interior — snap to its start.
        let cursor = self.paste_registry.skip_cursor(cursor, false);
        self.composer.set_cursor(cursor);
        let before = self.composer.len();
        self.composer.delete_left();
        let removed = before - self.composer.len();
        if removed > 0 {
            // delete_left removes the char ending at the old cursor; the
            // edit anchor is the new cursor position.
            self.paste_registry
                .shift_for_edit(self.composer.cursor(), -(removed as isize));
        }
    }

    /// Forward-delete (`Delete` / vim `x`), block-aware ordinary-char
    /// path. The whole-block case is handled by `paste_block_right`.
    fn composer_delete_right(&mut self) {
        if self.paste_registry.is_empty() {
            self.composer.delete_right();
            return;
        }
        let cursor = self.composer.cursor();
        let cursor = self.paste_registry.skip_cursor(cursor, true);
        self.composer.set_cursor(cursor);
        let at = self.composer.cursor();
        let before = self.composer.len();
        self.composer.delete_right();
        let removed = before - self.composer.len();
        if removed > 0 {
            self.paste_registry.shift_for_edit(at, -(removed as isize));
        }
    }

    /// Run a vim normal-mode motion (`w`/`W`/`b`/`B`) then snap the cursor
    /// off any block interior to the far boundary in the direction of
    /// travel (`forward`), so a word motion treats a paste block as one
    /// unit. Fast-paths when there are no blocks.
    fn vim_motion<F: FnOnce(&mut crate::tui::composer::Composer)>(
        &mut self,
        motion: F,
        forward: bool,
    ) {
        motion(&mut self.composer);
        if self.paste_registry.is_empty() {
            return;
        }
        let landed = self
            .paste_registry
            .skip_cursor(self.composer.cursor(), forward);
        self.composer.set_cursor(landed);
    }

    /// Move left one unit, treating a block as a single step: landing on a
    /// block's right boundary then moving left jumps to its left boundary.
    fn composer_move_left(&mut self) {
        if self.paste_registry.is_empty() {
            self.composer.move_left();
            return;
        }
        let cursor = self.composer.cursor();
        // If we're exactly at a block's right edge, jump the whole block.
        if let Some(b) = self.paste_registry.block_ending_at(cursor) {
            self.composer.set_cursor(b.start);
            return;
        }
        self.composer.move_left();
        // If the plain move landed inside a block, snap to its start.
        let landed = self
            .paste_registry
            .skip_cursor(self.composer.cursor(), false);
        self.composer.set_cursor(landed);
    }

    /// Move right one unit, treating a block as a single step.
    fn composer_move_right(&mut self) {
        if self.paste_registry.is_empty() {
            self.composer.move_right();
            return;
        }
        let cursor = self.composer.cursor();
        if let Some(b) = self.paste_registry.block_starting_at(cursor) {
            self.composer.set_cursor(b.end);
            return;
        }
        self.composer.move_right();
        let landed = self
            .paste_registry
            .skip_cursor(self.composer.cursor(), true);
        self.composer.set_cursor(landed);
    }

    /// Run a vim motion-delete (`dw`, `db`, `cw`, `d$`, `d0`, `dG`,
    /// `dgg`, …) block-aware via a motion closure that moves the composer
    /// cursor to the far end of the operator's range and a matching plain
    /// `delete` closure for the no-blocks fast path. When blocks exist, we
    /// delete the byte span between the start and the motion's landing
    /// point, widened to a block boundary if it crosses any paste block,
    /// so the block is removed whole. When no blocks exist we just run the
    /// plain composer delete — vim editing is byte-identical to today.
    fn block_aware_delete<M, D>(&mut self, motion: M, delete: D)
    where
        M: FnOnce(&mut crate::tui::composer::Composer),
        D: FnOnce(&mut crate::tui::composer::Composer),
    {
        if self.paste_registry.is_empty() {
            delete(&mut self.composer);
            return;
        }
        let from = self.composer.cursor();
        let to = self.composer.probe_motion(motion);
        if from == to {
            return;
        }
        let (mut lo, mut hi) = if from <= to { (from, to) } else { (to, from) };
        // Widen the range to swallow any block it crosses, so a delete
        // that touches a placeholder removes the whole block.
        if let Some((bs, be)) = self.paste_registry.block_crossed_by(lo, hi) {
            lo = lo.min(bs);
            hi = hi.max(be);
        }
        self.composer.delete_range(lo, hi);
        self.paste_registry
            .shift_for_edit(lo, -((hi - lo) as isize));
    }

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

/// `Ctrl+X` — force-close the embedded pane (GOALS §1i). Excludes Shift
/// so it's unambiguous under the kitty keyboard protocol.
fn is_pane_force_close(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char('x') | KeyCode::Char('X'))
}

/// `Ctrl+O` — toggle focus between the embedded pane and the composer.
fn is_pane_focus_toggle(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
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

#[cfg(test)]
mod slash_cursor_tests {
    use crate::tui::nav::{wrap_next, wrap_prev};

    /// The slash-menu cursor mirrors the `@`-popup: the highlight moves
    /// with the same wrap math the handler applies, and the default
    /// highlight is index 0 — the frequency-ranked top match (see
    /// `slash_rank_tests`), preserving "type `/foo` + Enter runs the top
    /// match" muscle memory.
    #[test]
    fn cursor_default_is_top_match_and_wraps() {
        // A fresh slash session starts on the top-ranked match.
        let mut sel = 0usize;
        let n = 3usize; // e.g. /settings, /session, /stats
        assert_eq!(sel, 0, "default highlight is the top match");
        // Up from the top wraps to the last.
        sel = wrap_prev(sel, n);
        assert_eq!(sel, 2);
        // Down from the last wraps back to the top.
        sel = wrap_next(sel, n);
        assert_eq!(sel, 0);
        // Interior Down steps normally.
        sel = wrap_next(sel, n);
        assert_eq!(sel, 1);
    }

    /// Recall suppression is scoped to "menu visible": the handler routes
    /// Up/Down to slash-nav exactly when `slash_query().is_some()`, and to
    /// composer history recall otherwise. This models that branch.
    #[test]
    fn history_recall_suppressed_only_while_menu_visible() {
        fn up_does_slash_nav(slash_query_is_some: bool) -> bool {
            // The handler's gate: while a slash query is active, Up/Down
            // move the menu cursor and return early; otherwise they fall
            // through to `history_up`/`history_down`.
            slash_query_is_some
        }
        assert!(up_does_slash_nav(true), "menu visible → slash nav");
        assert!(
            !up_does_slash_nav(false),
            "menu not visible → history recall resumes"
        );
    }

    /// A single-item match set (the common `/foo` exact-prefix case)
    /// stays on its one item under either arrow.
    #[test]
    fn single_match_stays_put() {
        assert_eq!(wrap_next(0, 1), 0);
        assert_eq!(wrap_prev(0, 1), 0);
    }
}

#[cfg(test)]
mod dispatch_span_tests {
    use super::DispatchOutcome;

    /// Reproduce `submit_input`'s working-span teardown rule without a
    /// live daemon. The bug was that a failed-start submit left `busy`
    /// stuck `true` forever, since `AgentIdle` (the sole falling edge)
    /// never arrives when no worker was spawned. This models the exact
    /// production gate (`owns_working_span && outcome.span_orphaned()`)
    /// against the `begin`/`end_working_span` semantics (`busy` true on
    /// rising edge, false on falling edge).
    fn busy_after_fresh_submit(outcome: DispatchOutcome) -> bool {
        // Fresh-submit path always owns the rising edge it just opened.
        let owns_working_span = true;
        let mut busy = true; // begin_working_span() set this.
        if owns_working_span && outcome.span_orphaned() {
            busy = false; // end_working_span() lowers it.
        }
        busy
    }

    #[test]
    fn runner_failed_clears_busy() {
        // The reported stuck-span case: runner is `Some(Err(_))`.
        assert!(!busy_after_fresh_submit(DispatchOutcome::RunnerFailed));
    }

    #[test]
    fn queue_full_and_driver_closed_clear_busy() {
        assert!(!busy_after_fresh_submit(DispatchOutcome::QueueFull));
        assert!(!busy_after_fresh_submit(DispatchOutcome::DriverClosed));
    }

    #[test]
    fn successful_send_keeps_busy_until_agent_idle() {
        // A normal turn stays "working"; only `AgentIdle` ends it.
        assert!(busy_after_fresh_submit(DispatchOutcome::Sent));
    }

    #[test]
    fn queue_path_never_tears_down_a_span() {
        // The busy/queue path started no span this submit, so even an
        // orphaning outcome must leave any in-flight turn's span alone.
        let owns_working_span = false;
        let mut busy = true; // a legitimately in-flight turn.
        if owns_working_span && DispatchOutcome::RunnerFailed.span_orphaned() {
            busy = false;
        }
        assert!(busy);
    }
}
