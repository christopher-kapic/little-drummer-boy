//! Top-level TUI state and event loop.
//!
//! Mouse capture is intentionally **not** enabled: leaving it off lets
//! the terminal/tmux handle the scroll wheel natively, so the user can
//! scroll up through chat history and the launch header even after they
//! spill into terminal scrollback. When we eventually need mouse-driven
//! interactions (clicking buttons, drag-to-select, etc.) we'll switch on
//! `EnableMouseCapture` and route `MouseEvent`s in the event loop —
//! revisit the scroll path when that happens.

use std::io::{Write, stdout};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{Clear, ClearType, size as terminal_size};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, TerminalOptions, Viewport};

use crate::engine::TurnEvent;
use crate::git::{self, RepoStatus};
use crate::tui::agent_runner::{self, AgentRunner};
use crate::tui::chrome;
use crate::tui::composer::{Composer, INPUT_PREFIX, VimMode, input_prefix_width};
use crate::tui::geometry::PaneGeometry;
use crate::tui::history::{
    HistoryEntry, PendingMsg, Rendered, render_entry, render_pending, route_text_delta,
    thinking_dots,
};
use crate::tui::settings::{self, Dialog};
use crate::tui::theme::MUTED_COLOR_INDEX;
use crate::welcome::{self, LaunchInfo};

const MIN_INPUT_CONTENT: u16 = 1;
const MAX_INPUT_CONTENT: u16 = 6;
const INPUT_BORDER: u16 = 2;
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const EVENT_TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
struct SlashCommand {
    name: &'static str,
    description: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "compact",
        description: "Compress the conversation to save context",
    },
    SlashCommand {
        name: "exit",
        description: "Quit cockpit",
    },
    SlashCommand {
        name: "favorite",
        description: "Mark the active model as a favorite",
    },
    SlashCommand {
        name: "fetch-models",
        description: "Refresh model lists from every configured provider",
    },
    SlashCommand {
        name: "model",
        description: "Switch the active model",
    },
    SlashCommand {
        name: "prune",
        description: "Drop the oldest messages",
    },
    SlashCommand {
        name: "settings",
        description: "Open the settings dialog",
    },
];

pub struct App {
    launch: LaunchInfo,
    composer: Composer,
    history: Vec<HistoryEntry>,
    /// In-flight assistant turn (between `ThinkingStarted` and the
    /// matching `AssistantText`/tool boundary). When `Some`, the
    /// renderer appends a live entry to the bottom of the history
    /// pane.
    pending: Option<PendingMsg>,
    /// Reference point for the animated `Thinking…` dots. Set once at
    /// `App::new` time; the renderer derives the dot count from the
    /// elapsed time so the animation advances each tick.
    started_at: Instant,
    /// Live git status; updated by a background tokio task spawned in
    /// `run`. The event loop syncs this into `launch.repo_status` once
    /// per tick.
    repo_status: Arc<Mutex<Option<RepoStatus>>>,
    /// Current pane height. Monotonically non-decreasing: when the chat
    /// or composer needs more room we grow the pane (and scroll prior
    /// terminal content up into scrollback so it stays mouse-reachable),
    /// but we never shrink it.
    pane_height: u16,
    dialog: Dialog,
    /// `/model` picker. Mutually exclusive with `dialog` (we never show
    /// both); kept separate so the picker doesn't clutter the settings
    /// state machine.
    model_picker: Option<crate::tui::model_picker::ModelPickerDialog>,
    /// "Daemon not running" prompt shown at startup. Once the user picks,
    /// this is taken and the prompt closes.
    daemon_prompt: Option<crate::tui::daemon_prompt::DaemonPromptDialog>,
    /// True after we've successfully connected to (or started) the daemon.
    daemon_connected: bool,
    /// Lines emitted by an in-flight `/fetch-models` task. The event
    /// loop drains this each tick and appends to history.
    fetch_models_progress: Arc<Mutex<Vec<String>>>,
    /// Lazily-initialized agent runner. None until the first user
    /// submit; populated by [`Self::ensure_agent_runner`]. Stored as
    /// `Result<AgentRunner, String>` so a failed init keeps the error
    /// around for next-time visibility.
    agent_runner: Option<Result<AgentRunner, String>>,
    /// Last-rendered chat area `Rect`. Used to translate absolute
    /// terminal mouse coordinates into chat-relative coordinates so
    /// click-to-expand works on thinking blocks.
    chat_area: Option<Rect>,
    /// Click hit map: for each *visible* row in `chat_area`, the index
    /// (within `self.history`) of the agent entry whose thinking chip
    /// lives there — or `None` for non-clickable rows. Refreshed every
    /// render.
    clickable_rows: Vec<Option<usize>>,
}

impl App {
    pub fn new(project: Option<&Path>) -> Self {
        let mut composer = Composer::new(true);
        composer.set_vim_mode(VimMode::Insert);

        let launch = welcome::load(project);
        let repo_status = Arc::new(Mutex::new(launch.repo_status.clone()));

        // Probe the daemon synchronously up front so the prompt shows
        // immediately when we open the TUI rather than after a tick.
        let (daemon_prompt, daemon_connected) = match crate::daemon::DaemonPaths::resolve() {
            Ok(paths) => match crate::daemon::probe_blocking(&paths) {
                crate::daemon::DaemonStatus::Running => (None, true),
                status => (
                    Some(crate::tui::daemon_prompt::DaemonPromptDialog::new(
                        status, paths,
                    )),
                    false,
                ),
            },
            Err(_) => (None, false),
        };

        let mut app = Self {
            launch,
            composer,
            history: Vec::new(),
            pending: None,
            started_at: Instant::now(),
            repo_status,
            pane_height: 0,
            dialog: Dialog::None,
            model_picker: None,
            daemon_prompt,
            daemon_connected,
            fetch_models_progress: Arc::new(Mutex::new(Vec::new())),
            agent_runner: None,
            chat_area: None,
            clickable_rows: Vec::new(),
        };
        app.pane_height = app.geometry().desired_pane_height();
        app
    }

    fn geometry(&self) -> PaneGeometry {
        let dialog = if self.daemon_prompt.is_some() {
            crate::tui::daemon_prompt::DIALOG_HEIGHT
        } else if self.dialog.is_active() {
            settings::DIALOG_HEIGHT
        } else if self.model_picker.is_some() {
            crate::tui::model_picker::DIALOG_HEIGHT
        } else {
            0
        };
        PaneGeometry::compute(
            self.input_height(),
            self.popup_lines(),
            self.total_history_lines(),
            dialog,
        )
    }

    pub async fn run(&mut self) -> Result<()> {
        // Print the header to normal terminal output. It lives in scrollback
        // from this point on — once enough messages arrive it scrolls up
        // off the top of the terminal, recoverable with the mouse wheel.
        welcome::print_header(&self.launch);

        reserve_fixed_pane_space(self.pane_height)?;

        let (width, height) = terminal_size()?;
        let options = TerminalOptions {
            viewport: Viewport::Fixed(viewport_rect(self.pane_height, width, height)),
        };
        let mut terminal = ratatui::try_init_with_options(options)?;

        let kbd_enhanced = crossterm::execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )
        .is_ok();

        // Mouse capture is on so we can route clicks on a thinking
        // chip to expand the reasoning block. Trade-off: when mouse
        // capture is enabled, the terminal forwards scroll-wheel
        // events to us instead of scrolling its own scrollback — so
        // users lose native scroll-wheel through the terminal. We
        // accept the trade because click-to-expand was an explicit
        // user request; the prior `Ctrl+R` shortcut still works.
        let mouse_enabled = crossterm::execute!(stdout(), EnableMouseCapture).is_ok();

        let refresh_handle = spawn_git_refresh(self.launch.cwd.clone(), self.repo_status.clone());

        let result = self.event_loop(&mut terminal);

        refresh_handle.abort();

        // Spill any remaining in-viewport chat into terminal scrollback
        // before we tear down, so the user can scroll up and copy
        // commands or paths the agent produced (GOALS §1d). We pass
        // *all* history — once the viewport is gone, those rows are
        // the only place this content survives.
        self.spill_remaining_history_for_exit();

        // Wipe the viewport rows before we hand the terminal back. Without
        // this, the input box / popup / status sit forever in the user's
        // scrollback under the last chat line — distracting when scrolling
        // up after exit.
        self.clear_viewport_for_exit().ok();

        if mouse_enabled {
            let _ = crossterm::execute!(stdout(), DisableMouseCapture);
        }
        if kbd_enhanced {
            let _ = crossterm::execute!(stdout(), PopKeyboardEnhancementFlags);
        }
        ratatui::try_restore()?;
        result
    }

    /// Flush every remaining history entry into the terminal area
    /// *above* the viewport so it lands in scrollback. Called once at
    /// shutdown — by the time this runs we don't care about ratatui's
    /// double-buffer because we're about to restore the terminal.
    fn spill_remaining_history_for_exit(&mut self) {
        // Finalize any in-flight pending turn first so its text shows
        // up in the dump.
        self.finalize_pending();
        if self.history.is_empty() {
            return;
        }
        let plain: Vec<String> = self
            .history
            .iter()
            .flat_map(|entry| {
                let mut lines = entry_to_plain_lines(entry);
                // Match the chat-area visual: one blank row after
                // each user/agent block.
                if matches!(entry, HistoryEntry::User { .. } | HistoryEntry::Agent { .. }) {
                    lines.push(String::new());
                }
                lines
            })
            .collect();
        let _ = insert_above_viewport(self.pane_height, &plain);
        self.history.clear();
    }

    fn clear_viewport_for_exit(&self) -> Result<()> {
        let (_, h) = terminal_size()?;
        let viewport_top = h.saturating_sub(self.pane_height);
        let mut out = stdout();
        for row in viewport_top..h {
            crossterm::execute!(out, cursor::MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }
        crossterm::execute!(out, cursor::MoveTo(0, viewport_top))?;
        out.flush()?;
        Ok(())
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.sync_repo_status();
            self.drain_fetch_progress();
            self.drain_agent_events();
            self.sync_active_agent();
            self.dialog.tick();
            self.maybe_grow_pane(terminal)?;
            if self.maybe_spill_history()? {
                terminal.clear()?;
            }
            terminal.draw(|frame| self.render(frame))?;

            if event::poll(EVENT_TICK)? {
                match event::read()? {
                    Event::Key(key) if accepts_key(&key) && self.handle_key(key) => break,
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse);
                    }
                    Event::Resize(width, height) => {
                        terminal.resize(viewport_rect(self.pane_height, width, height))?;
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    fn drain_fetch_progress(&mut self) {
        let drained: Vec<String> = match self.fetch_models_progress.lock() {
            Ok(mut buf) if !buf.is_empty() => buf.drain(..).collect(),
            _ => return,
        };
        let touches_config = drained
            .iter()
            .any(|l| l.contains("model(s)") || l.ends_with(": done"));
        for line in drained {
            self.history.push(HistoryEntry::Plain { line });
        }
        if touches_config {
            self.reload_launch_info();
        }
    }

    fn sync_repo_status(&mut self) {
        if let Ok(guard) = self.repo_status.lock()
            && self.launch.repo_status != *guard
        {
            self.launch.repo_status = guard.clone();
        }
    }

    /// Grow the pane (and the terminal viewport) if more space is now
    /// needed than we've previously reserved. We scroll the terminal up
    /// by the deficit so prior output moves into scrollback rather than
    /// being clipped.
    fn maybe_grow_pane(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let (w, h) = terminal_size()?;
        let desired = self.geometry().desired_pane_height().min(h);
        if desired > self.pane_height {
            let extra = desired - self.pane_height;
            push_terminal_content_up(extra, h)?;
            self.pane_height = desired;
            terminal.resize(viewport_rect(self.pane_height, w, h))?;
        }
        Ok(())
    }

    /// Once the pane has grown to fill the terminal but history still
    /// wants more space, pop the oldest entries off `App.history` and
    /// push them into terminal scrollback. Mouse-wheel scroll preserves
    /// them. Returns true if anything spilled (caller must clear ratatui's
    /// buffer to force a clean redraw).
    fn maybe_spill_history(&mut self) -> Result<bool> {
        let (_, h) = terminal_size()?;
        let geom = self.geometry();
        let max_history = h
            .saturating_sub(geom.chrome_height())
            .max(crate::tui::geometry::MIN_HISTORY_HEIGHT);

        let total = self.total_history_lines();
        if total <= max_history {
            return Ok(false);
        }

        let to_spill = total - max_history;
        let mut spilled = 0u16;
        let mut items = Vec::new();
        while spilled < to_spill && !self.history.is_empty() {
            let entry = self.history.remove(0);
            spilled = spilled.saturating_add(entry_rendered_rows(&entry));
            items.push(entry);
        }
        // Render each spilled entry to plain text (drop styling) for
        // the scrollback area. We lose bg color in scrollback — that's
        // acceptable; the alternative is dumping ANSI escape sequences
        // into the user's terminal scrollback, which is messier.
        let plain: Vec<String> = items
            .iter()
            .flat_map(|e| entry_to_plain_lines(e))
            .collect();
        insert_above_viewport(self.pane_height, &plain)?;
        Ok(true)
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            return true;
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
                // line and header refresh.
                self.dialog = Dialog::None;
                self.reload_launch_info();
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

        // Ctrl+R toggles the most-recent agent message's reasoning
        // block expand/collapse. (See the doc comment on
        // `toggle_recent_reasoning` for why this is a keybind rather
        // than a click handler.)
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('r'))
        {
            self.toggle_recent_reasoning();
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                // Esc never exits — too easy to hit accidentally. It
                // cancels an in-progress slash command; otherwise no-op.
                // Exit paths: `/exit`, Ctrl+C, Ctrl+D.
                if self.slash_query().is_some() {
                    self.composer.clear();
                }
                false
            }
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    self.composer.insert_char('\n');
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
                self.composer.delete_left();
                false
            }
            KeyCode::Delete => {
                self.composer.delete_right();
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
                self.composer.move_up();
                false
            }
            KeyCode::Down => {
                self.composer.move_down();
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
                false
            }
            _ => false,
        }
    }

    fn complete_or_submit(&mut self) -> bool {
        if let Some(query) = self.slash_query() {
            if let Some(cmd) = slash_matches(query).first() {
                return self.execute_slash(**cmd);
            }
            return false;
        }
        self.submit_input()
    }

    fn submit_input(&mut self) -> bool {
        let submitted = self.composer.text().trim().to_string();
        if submitted.is_empty() {
            return false;
        }

        // Persist the user's text as a styled history entry — the
        // renderer applies bg color + padding rows.
        self.history.push(HistoryEntry::User {
            text: submitted.clone(),
            timestamp: chrono::Local::now(),
        });

        self.ensure_agent_runner();
        match self.agent_runner.as_ref() {
            Some(Ok(runner)) => match runner.input_tx.try_send(submitted) {
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
        false
    }

    fn ensure_agent_runner(&mut self) {
        if self.agent_runner.is_some() {
            return;
        }
        self.agent_runner = Some(agent_runner::try_spawn(&self.launch.cwd));
    }

    /// Drain any [`TurnEvent`]s the engine has produced into the
    /// pending+history state machine. Runs each tick.
    fn drain_agent_events(&mut self) {
        let Some(Ok(runner)) = self.agent_runner.as_ref() else {
            return;
        };
        let drained = {
            let mut guard = runner.events.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        for event in drained {
            self.apply_event(event);
        }
    }

    fn apply_event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::ThinkingStarted { agent } => {
                self.finalize_pending();
                self.pending = Some(new_pending(agent));
            }
            TurnEvent::AssistantTextDelta { agent, delta } => {
                let p = self
                    .pending
                    .get_or_insert_with(|| new_pending(agent));
                let wrote = route_text_delta(
                    &delta,
                    &mut p.text,
                    &mut p.reasoning,
                    &mut p.inside_think,
                    &mut p.tag_partial,
                );
                if wrote && p.text_started_at.is_none() {
                    p.text_started_at = Some(Instant::now());
                }
            }
            TurnEvent::ReasoningDelta { agent, delta } => {
                let p = self
                    .pending
                    .get_or_insert_with(|| new_pending(agent));
                p.reasoning.push_str(&delta);
            }
            TurnEvent::AssistantText { .. } => {
                // Mark text-start (non-streaming providers land here
                // without ever emitting a Delta).
                if let Some(p) = &mut self.pending
                    && p.text_started_at.is_none()
                {
                    p.text_started_at = Some(Instant::now());
                }
                self.finalize_pending();
            }
            TurnEvent::ToolStart { tool, args, .. } => {
                self.finalize_pending();
                let short = agent_runner::short_args(&args);
                self.history.push(HistoryEntry::Plain {
                    line: format!("  → {tool}({short})"),
                });
            }
            TurnEvent::ToolEnd {
                tool,
                output,
                truncated,
                ..
            } => {
                let snippet = agent_runner::first_line(&output, 200);
                let mark = if truncated { " (truncated)" } else { "" };
                self.history.push(HistoryEntry::Plain {
                    line: format!("  ✓ {tool}: {snippet}{mark}"),
                });
            }
            TurnEvent::ToolError { tool, error, .. } => {
                self.finalize_pending();
                self.history.push(HistoryEntry::Plain {
                    line: format!("  ✗ {tool}: {error}"),
                });
            }
            TurnEvent::SubagentSpawned {
                parent,
                child,
                prompt,
            } => {
                self.finalize_pending();
                let short = agent_runner::first_line(&prompt, 100);
                self.history.push(HistoryEntry::Plain {
                    line: format!("[{parent} → {child}]: {short}"),
                });
            }
            TurnEvent::SubagentReport { agent, .. } => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("{agent} returned to caller."),
                });
            }
        }
    }

    /// Move the in-flight assistant turn (if any) into permanent history.
    /// Computes `think_duration` from the gap between `started_at` and
    /// the first text delta — that's the *reasoning* time, not the
    /// total turn time.
    fn finalize_pending(&mut self) {
        let Some(mut p) = self.pending.take() else {
            return;
        };
        // Flush any buffered partial tag — it can't be a real tag
        // because we're done streaming.
        if !p.tag_partial.is_empty() {
            let buf = std::mem::take(&mut p.tag_partial);
            if p.inside_think {
                p.reasoning.push_str(&buf);
            } else {
                p.text.push_str(&buf);
            }
        }
        if !p.text.trim().is_empty() {
            let think_duration = p
                .text_started_at
                .map(|ts| ts.saturating_duration_since(p.started_at));
            self.history.push(HistoryEntry::Agent {
                name: p.name,
                text: p.text,
                reasoning: p.reasoning,
                timestamp: p.timestamp,
                expanded: false,
                think_duration,
            });
        }
        // If only reasoning landed (no text), drop it — most-recent
        // streams produce text after reasoning anyway. Adding a
        // ThinkingOnly variant later is cheap.
    }

    /// Toggle the most-recent agent message's `expanded` flag. The
    /// equivalent of clicking the chip; bound to `Ctrl+R` for
    /// keyboard-only use.
    fn toggle_recent_reasoning(&mut self) {
        for entry in self.history.iter_mut().rev() {
            if let HistoryEntry::Agent {
                expanded, reasoning, ..
            } = entry
                && !reasoning.trim().is_empty()
            {
                *expanded = !*expanded;
                return;
            }
        }
    }

    /// Handle a mouse event. Left-click on a thinking chip toggles
    /// expansion; other events are ignored (we don't implement chat
    /// scrolling yet, so scroll-wheel input is a no-op).
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return;
        }
        let Some(area) = self.chat_area else {
            return;
        };
        // crossterm reports row/column as 0-indexed absolute terminal
        // coordinates. Translate to chat-area relative.
        if mouse.row < area.y || mouse.row >= area.y + area.height {
            return;
        }
        if mouse.column < area.x || mouse.column >= area.x + area.width {
            return;
        }
        let rel = (mouse.row - area.y) as usize;
        let Some(Some(entry_idx)) = self.clickable_rows.get(rel).copied() else {
            return;
        };
        if let Some(HistoryEntry::Agent { expanded, .. }) = self.history.get_mut(entry_idx) {
            *expanded = !*expanded;
        }
    }

    fn sync_active_agent(&mut self) {
        let Some(Ok(runner)) = self.agent_runner.as_ref() else {
            return;
        };
        let name = runner.active_agent.lock().unwrap().clone();
        if name != self.launch.agent_name {
            self.launch.agent_name = name;
        }
    }

    fn execute_slash(&mut self, cmd: SlashCommand) -> bool {
        self.composer.clear();
        let msg = match cmd.name {
            "exit" => return true,
            "settings" => {
                self.dialog = Dialog::open(&self.launch.cwd);
                return false;
            }
            "fetch-models" => {
                self.spawn_fetch_models();
                return false;
            }
            "model" => {
                match crate::tui::model_picker::ModelPickerDialog::open(&self.launch.cwd) {
                    Ok(picker) => {
                        self.model_picker = Some(picker);
                    }
                    Err(e) => {
                        self.history.push(HistoryEntry::Plain {
                            line: format!("/model: {e}"),
                        });
                    }
                }
                return false;
            }
            "favorite" => {
                match crate::tui::model_picker::toggle_active_favorite(&self.launch.cwd) {
                    Ok((new, p, m)) => {
                        let verb = if new { "marked" } else { "unmarked" };
                        self.history.push(HistoryEntry::Plain {
                            line: format!("/favorite: {verb} {p}/{m} as favorite"),
                        });
                        self.reload_launch_info();
                    }
                    Err(e) => {
                        self.history.push(HistoryEntry::Plain {
                            line: format!("/favorite: {e}"),
                        });
                    }
                }
                return false;
            }
            "compact" => "/compact: stub — context compaction not wired yet.",
            "prune" => "/prune: stub — history pruning not wired yet.",
            _ => return false,
        };
        self.history.push(HistoryEntry::Plain {
            line: msg.to_string(),
        });
        false
    }

    /// Re-read launch info (provider/model/favorite) from disk and
    /// keep the cwd + repo_status we already have.
    fn reload_launch_info(&mut self) {
        let mut fresh = welcome::load(Some(&self.launch.cwd));
        // Don't clobber the live repo status — it's maintained by the
        // background poller and is fresher than a re-read here.
        fresh.repo_status = self.launch.repo_status.clone();
        self.launch = fresh;
    }

    /// Kick off a non-interactive cross-provider `/models` refresh.
    /// Lines land in `fetch_models_progress`; the event loop drains
    /// them into history.
    fn spawn_fetch_models(&mut self) {
        use crate::config::dirs::discover_config_dirs;
        use crate::config::providers::{ConfigDoc, OnUnlistedModelsFetch};
        use crate::providers::models_fetch::{self, FetchOutcome};
        use std::time::Duration;

        let cwd = self.launch.cwd.clone();
        let progress = Arc::clone(&self.fetch_models_progress);
        self.history.push(HistoryEntry::Plain {
            line: "/fetch-models: starting…".to_string(),
        });

        tokio::spawn(async move {
            let push = |lines: &Arc<Mutex<Vec<String>>>, s: String| {
                if let Ok(mut g) = lines.lock() {
                    g.push(s);
                }
            };

            let dirs = discover_config_dirs(&cwd);
            let Some(dir) = dirs.first() else {
                push(
                    &progress,
                    "/fetch-models: no cockpit config — run /settings to create one".into(),
                );
                return;
            };
            let path = dir.path.join("config.json");
            let mut doc = match ConfigDoc::load(&path) {
                Ok(d) => d,
                Err(e) => {
                    push(&progress, format!("/fetch-models: config load failed: {e}"));
                    return;
                }
            };
            let mut cfg = doc.providers();
            let policy = cfg
                .on_unlisted_models_fetch
                .unwrap_or(OnUnlistedModelsFetch::Keep);

            if cfg.providers.is_empty() {
                push(&progress, "/fetch-models: no providers configured".into());
                return;
            }

            let ids: Vec<String> = cfg.providers.keys().cloned().collect();
            for id in &ids {
                let entry = cfg.providers.get(id).cloned().unwrap();
                let (_, missing) = models_fetch::resolve_headers(&entry.headers);
                if !missing.is_empty() {
                    push(
                        &progress,
                        format!(
                            "/fetch-models: {id} skipped — missing env var(s): {}",
                            missing.join(", ")
                        ),
                    );
                    continue;
                }
                match models_fetch::fetch_models(
                    &entry.url,
                    &entry.headers,
                    Some(Duration::from_secs(15)),
                )
                .await
                {
                    Ok(FetchOutcome::Models(remote)) => {
                        let n = remote.len();
                        let entry_mut = cfg.providers.get_mut(id).unwrap();
                        match policy {
                            OnUnlistedModelsFetch::Remove | OnUnlistedModelsFetch::Ask => {
                                entry_mut.models = remote;
                            }
                            OnUnlistedModelsFetch::Keep => {
                                let mut new = remote;
                                for old in &entry_mut.models {
                                    if !new.iter().any(|n| n.id == old.id) {
                                        new.push(old.clone());
                                    }
                                }
                                entry_mut.models = new;
                            }
                        }
                        entry_mut.models_fetched_at = Some(chrono::Utc::now());
                        push(&progress, format!("/fetch-models: {id} → {n} model(s)"));
                    }
                    Ok(FetchOutcome::Unsupported) => {
                        push(
                            &progress,
                            format!("/fetch-models: {id} has no /models endpoint"),
                        );
                    }
                    Err(e) => {
                        push(&progress, format!("/fetch-models: {id} failed: {e}"));
                    }
                }
            }

            if let Err(e) = doc.write(&cfg) {
                push(&progress, format!("/fetch-models: write failed: {e}"));
            } else {
                push(&progress, "/fetch-models: done".into());
            }
        });
    }

    fn model_summary_history_line(&self) -> String {
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

    fn slash_query(&self) -> Option<&str> {
        let rest = self.composer.text().strip_prefix('/')?;
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        Some(&rest[..end])
    }

    fn popup_lines(&self) -> u16 {
        match self.slash_query() {
            Some(q) => slash_matches(q).len().max(1) as u16,
            None => 0,
        }
    }

    fn input_height(&self) -> u16 {
        (self.composer.line_count() as u16).clamp(MIN_INPUT_CONTENT, MAX_INPUT_CONTENT)
            + INPUT_BORDER
    }

    fn total_history_lines(&self) -> u16 {
        // We can't perfectly compute the rendered line count without
        // the area width, but the history geometry caller doesn't have
        // that yet either. Approximate: 1 row per Plain, 3 rows per
        // User (padding + body + padding; multi-line bodies cost more
        // but for sizing this is fine), 2 rows per Agent, plus pending.
        let mut total: u16 = 0;
        for entry in &self.history {
            total = total.saturating_add(match entry {
                HistoryEntry::Plain { .. } => 1,
                HistoryEntry::User { text, .. } => {
                    let body = text.matches('\n').count() as u16 + 1;
                    body.saturating_add(2)
                }
                HistoryEntry::Agent {
                    text,
                    reasoning,
                    expanded,
                    ..
                } => {
                    let body = text.matches('\n').count() as u16 + 1;
                    let mut rows = body;
                    if !reasoning.trim().is_empty() {
                        rows = rows.saturating_add(1);
                        if *expanded {
                            rows = rows.saturating_add(reasoning.lines().count() as u16);
                        }
                    }
                    rows
                }
            });
        }
        if self.pending.is_some() {
            total = total.saturating_add(1);
        }
        total
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
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
            let cursor_pos = self.render_input(frame, rects.input);
            if geom.popup > 0 {
                self.render_popup(frame, rects.popup);
            }
            frame.set_cursor_position(cursor_pos);
        }
        self.render_status(frame, rects.status);
    }

    fn render_history(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.chat_area = Some(area);
        let area_h = area.height as usize;

        let mut all: Vec<Line<'static>> = Vec::new();
        // `targets[i]` carries the history-entry index whose thinking
        // chip occupies row `i` of `all`, or `None` otherwise.
        let mut targets: Vec<Option<usize>> = Vec::new();
        for (idx, entry) in self.history.iter().enumerate() {
            let Rendered { lines, chip_row } = render_entry(entry, area.width);
            let chip_abs = chip_row.map(|cr| all.len() + cr);
            for i in 0..lines.len() {
                targets.push(if Some(all.len() + i) == chip_abs {
                    Some(idx)
                } else {
                    None
                });
            }
            all.extend(lines);
            // Insert a one-line gap between user/agent messages so the
            // chat breathes. Plain entries (tool calls, errors) belong
            // to the surrounding agent turn and don't get a gap.
            if matches!(entry, HistoryEntry::User { .. } | HistoryEntry::Agent { .. }) {
                all.push(Line::default());
                targets.push(None);
            }
        }
        if let Some(pending) = &self.pending {
            let dots = thinking_dots(self.started_at.elapsed().as_millis());
            let pending_lines = render_pending(pending, dots, area.width);
            for _ in 0..pending_lines.len() {
                targets.push(None);
            }
            all.extend(pending_lines);
        }

        // Bottom-align the visible window over `all`.
        let (visible, visible_targets): (Vec<Line<'static>>, Vec<Option<usize>>) =
            if all.len() < area_h {
                let pad = area_h - all.len();
                let mut v: Vec<Line<'static>> = (0..pad).map(|_| Line::default()).collect();
                let mut t: Vec<Option<usize>> = (0..pad).map(|_| None).collect();
                v.extend(all);
                t.extend(targets);
                (v, t)
            } else {
                let drop = all.len() - area_h;
                let v: Vec<Line<'static>> = all.into_iter().skip(drop).collect();
                let t: Vec<Option<usize>> = targets.into_iter().skip(drop).collect();
                (v, t)
            };
        self.clickable_rows = visible_targets;

        frame.render_widget(Paragraph::new(visible), area);
    }

    fn render_input(&self, frame: &mut ratatui::Frame, area: Rect) -> Position {
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White));
        let input_inner = input_block.inner(area);

        let prefix_width = input_prefix_width();
        let indent: String = " ".repeat(prefix_width);
        let text = self.composer.text();
        let buf_lines: Vec<&str> = if text.is_empty() {
            vec![""]
        } else {
            text.split('\n').collect()
        };
        let lines: Vec<Line<'static>> = buf_lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let prefix = if i == 0 {
                    INPUT_PREFIX
                } else {
                    indent.as_str()
                };
                Line::from(vec![
                    Span::styled(prefix.to_string(), Style::default().fg(Color::White)),
                    Span::styled((*l).to_string(), Style::default().fg(Color::White)),
                ])
            })
            .collect();

        let (cursor_line, cursor_col) = self.composer.cursor_line_col();
        let cursor_line = cursor_line as u16;
        let cursor_col = cursor_col as u16;

        let visible_rows = input_inner.height;
        let scroll_y = cursor_line.saturating_sub(visible_rows.saturating_sub(1));
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
            input_inner.x + prefix_width as u16 + cursor_col,
            input_inner.y + cursor_line.saturating_sub(scroll_y),
        )
    }

    /// Build the chrome's context indicator. Format:
    /// - With known max:   `12% context (max 192k), 0% prunable`
    /// - Without:          `1.2k tokens, 0% prunable`
    /// `prunable` is a placeholder zero until the pruning estimator
    /// (plan §10) lands.
    fn context_indicator_text(&self) -> String {
        let tokens = self.estimate_context_tokens();
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

    /// Cheap chars-÷-4 token estimate over visible chat content. Tools
    /// and system prompts aren't included — they live on the engine
    /// side. For the chrome's "context usage" affordance this rough
    /// number is fine; the engine will surface a real count once the
    /// tokenizer is wired (GOALS §10 / plan §3h).
    fn estimate_context_tokens(&self) -> u32 {
        let mut chars: usize = 0;
        for entry in &self.history {
            chars += match entry {
                HistoryEntry::User { text, .. } => text.chars().count(),
                HistoryEntry::Plain { line } => line.chars().count(),
                HistoryEntry::Agent {
                    text, reasoning, ..
                } => text.chars().count() + reasoning.chars().count(),
            };
        }
        if let Some(p) = &self.pending {
            chars += p.text.chars().count() + p.reasoning.chars().count();
        }
        (chars / 4).min(u32::MAX as usize) as u32
    }

    fn render_popup(&self, frame: &mut ratatui::Frame, area: Rect) {
        let query = self.slash_query().unwrap_or("");
        let matches = slash_matches(query);
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));

        let lines: Vec<Line<'static>> = if matches.is_empty() {
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
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_status(&self, frame: &mut ratatui::Frame, area: Rect) {
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

/// Rough row count for a history entry. Mirrors the breakdown in
/// `total_history_lines` so the spill math is consistent.
fn entry_rendered_rows(entry: &HistoryEntry) -> u16 {
    match entry {
        HistoryEntry::Plain { .. } => 1,
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

fn new_pending(name: String) -> PendingMsg {
    PendingMsg {
        name,
        text: String::new(),
        reasoning: String::new(),
        timestamp: chrono::Local::now(),
        started_at: Instant::now(),
        text_started_at: None,
        inside_think: false,
        tag_partial: String::new(),
    }
}

fn entry_to_plain_lines(entry: &HistoryEntry) -> Vec<String> {
    match entry {
        HistoryEntry::Plain { line } => vec![line.clone()],
        HistoryEntry::User { text, timestamp } => {
            let ts = timestamp.format("%H:%M").to_string();
            let mut out: Vec<String> = Vec::new();
            for (i, line) in text.split('\n').enumerate() {
                if i == 0 {
                    out.push(format!("{line}  [{ts}]"));
                } else {
                    out.push(line.to_string());
                }
            }
            out
        }
        HistoryEntry::Agent {
            name,
            text,
            reasoning,
            timestamp,
            expanded,
            ..
        } => {
            let ts = timestamp.format("%H:%M").to_string();
            let mut out = Vec::new();
            for (i, line) in text.split('\n').enumerate() {
                if i == 0 {
                    out.push(format!("{name}: {line}  [{ts}]"));
                } else {
                    let pad = " ".repeat(name.chars().count() + 2);
                    out.push(format!("{pad}{line}"));
                }
            }
            if !reasoning.trim().is_empty() && *expanded {
                out.push("  thinking:".to_string());
                for raw in reasoning.lines() {
                    out.push(format!("    {raw}"));
                }
            }
            out
        }
    }
}

fn slash_matches(query: &str) -> Vec<&'static SlashCommand> {
    SLASH_COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(query))
        .collect()
}

fn viewport_rect(pane_height: u16, width: u16, height: u16) -> Rect {
    let h = pane_height.min(height.max(1));
    Rect::new(0, height.saturating_sub(h), width.max(1), h)
}

fn reserve_fixed_pane_space(height: u16) -> Result<()> {
    let mut out = stdout();
    for _ in 0..height {
        writeln!(out)?;
    }
    out.flush()?;
    Ok(())
}

/// Scroll the terminal up by `extra` rows by walking the cursor to the
/// bottom row and emitting linefeeds. In raw mode `\n` is plain LF, so
/// each one at the last row makes the terminal scroll: prior output
/// moves into scrollback (recoverable with the mouse wheel) and `extra`
/// blank rows open up at the bottom for the enlarged viewport.
fn push_terminal_content_up(extra: u16, term_h: u16) -> Result<()> {
    if extra == 0 {
        return Ok(());
    }
    let mut out = stdout();
    crossterm::execute!(out, cursor::MoveTo(0, term_h.saturating_sub(1)))?;
    for _ in 0..extra {
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

/// Push `lines` into terminal scrollback just above the viewport.
///
/// Approach: write the lines at the top of the viewport (overwriting
/// the top rows of whatever is currently rendered there), then scroll
/// the terminal up by `lines.len()` rows. The just-written lines slide
/// up into the area above the viewport — visible if pane_height < term_h,
/// or pushed into actual terminal scrollback if pane_height == term_h.
/// Either way the mouse wheel can scroll back to them.
///
/// After calling this, the caller must invoke `terminal.clear()` so
/// ratatui forces a full redraw — otherwise its diff-based renderer
/// will not realize the terminal state has changed underneath it.
fn insert_above_viewport(pane_height: u16, lines: &[String]) -> Result<()> {
    let n = lines.len() as u16;
    if n == 0 {
        return Ok(());
    }
    let (_, h) = terminal_size()?;
    let viewport_top = h.saturating_sub(pane_height);
    let mut out = stdout();

    crossterm::execute!(out, cursor::MoveTo(0, viewport_top))?;
    for (i, line) in lines.iter().enumerate() {
        out.write_all(line.as_bytes())?;
        crossterm::execute!(out, Clear(ClearType::UntilNewLine))?;
        if i + 1 < lines.len() {
            out.write_all(b"\r\n")?;
        }
    }

    crossterm::execute!(out, cursor::MoveTo(0, h.saturating_sub(1)))?;
    for _ in 0..n {
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

fn accepts_key(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

/// Background task that polls `git status` every `GIT_REFRESH_INTERVAL`
/// without blocking the event-loop thread. The result lands in `shared`;
/// the event loop reads it on the next tick.
fn spawn_git_refresh(
    cwd: std::path::PathBuf,
    shared: Arc<Mutex<Option<RepoStatus>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(GIT_REFRESH_INTERVAL);
        // Skip the immediate first tick — `App::new` already populated
        // the initial status synchronously.
        interval.tick().await;
        loop {
            interval.tick().await;
            let cwd = cwd.clone();
            let status = tokio::task::spawn_blocking(move || git::repo_status(&cwd).ok().flatten())
                .await
                .unwrap_or(None);
            if let Ok(mut guard) = shared.lock() {
                *guard = status;
            }
        }
    })
}
