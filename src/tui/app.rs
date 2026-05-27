//! Top-level TUI state and event loop.
//!
//! Mouse capture is intentionally **not** enabled: leaving it off lets
//! the terminal/tmux handle the scroll wheel natively, so the user can
//! scroll up through chat history and the launch header even after they
//! spill into terminal scrollback. When we eventually need mouse-driven
//! interactions (clicking buttons, drag-to-select, etc.) we'll switch on
//! `EnableMouseCapture` and route `MouseEvent`s in the event loop —
//! revisit the scroll path when that happens.

use std::collections::HashMap;
use std::io::{Write, stdout};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::{self, SetCursorStyle};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{Clear, ClearType, size as terminal_size};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::{DefaultTerminal, TerminalOptions, Viewport};

use crate::config::dirs::discover_config_dirs;
use crate::config::extended::{DiffStyle, ExtendedConfig, ThinkingDisplay, VimModeSetting};
use crate::engine::TurnEvent;
use crate::git::{self, RepoStatus};
use crate::tui::agent_runner::{self, AgentRunner};
use crate::tui::chrome;
use crate::tui::composer::{Composer, INPUT_PREFIX, Operator, VimMode, input_prefix_width};
use crate::tui::geometry::PaneGeometry;
use crate::tui::history::{
    HistoryEntry, MarkdownOpts, PendingMsg, Rendered, render_entry, render_pending,
    route_text_delta, thinking_dots,
};
use crate::tui::settings::{self, Dialog};
use crate::tui::theme::MUTED_COLOR_INDEX;
use crate::welcome::{self, LaunchInfo};

const MIN_INPUT_CONTENT: u16 = 1;
const MAX_INPUT_CONTENT: u16 = 6;
const INPUT_BORDER: u16 = 2;
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const EVENT_TICK: Duration = Duration::from_millis(100);

/// Max suggestion rows the slash / @ autocomplete popup ever takes.
/// When fewer matches exist, the popup pads with blank lines so the
/// composer doesn't visibly shift as the user types and the candidate
/// set narrows. Keeps layout pinned to a 6-row reservation.
pub(crate) const AUTOCOMPLETE_ROWS: u16 = 6;

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
        name: "fork",
        description: "Branch a new conversation from the current point",
    },
    SlashCommand {
        name: "model",
        description: "Switch the active model",
    },
    SlashCommand {
        name: "new",
        description: "Clear the chat and start a fresh session",
    },
    SlashCommand {
        name: "prune",
        description: "Drop the oldest messages",
    },
    SlashCommand {
        name: "resume",
        description: "Browse and resume previous sessions (alias of /sessions)",
    },
    SlashCommand {
        name: "session",
        description: "Session subcommands (e.g. /session rename <title>)",
    },
    SlashCommand {
        name: "sessions",
        description: "Browse and resume previous sessions",
    },
    SlashCommand {
        name: "settings",
        description: "Open the settings dialog",
    },
];

pub struct App {
    launch: LaunchInfo,
    composer: Composer,
    /// User's vim_mode setting (hint/enabled/disabled). Drives whether
    /// the Normal-mode hint chip is shown.
    vim_setting: VimModeSetting,
    /// User's thinking-display setting. Drives whether the chip is shown
    /// and whether reasoning is rendered inline.
    thinking_setting: ThinkingDisplay,
    /// User's markdown-rendering preferences. Threaded into each
    /// `render_entry` call so the renderer can pick the markdown path
    /// per kind of entry.
    markdown_opts: MarkdownOpts,
    /// How `edit` / `editunlock` tool calls render in history
    /// (`tui.diff_style`). The narrow-terminal degradation from
    /// side-by-side → inline is per-render, computed from the
    /// rendered pane width.
    diff_style: DiffStyle,
    /// Cached args from `ToolStart` for edit tools that need them at
    /// `ToolEnd` time (to build the `Diff` history entry). Keyed by
    /// `call_id`; entries are popped at `ToolEnd`. Anything left
    /// behind (e.g. a tool that errored before emitting `ToolEnd`)
    /// gets cleaned up on the next `finalize_pending`.
    pending_edit_args: HashMap<String, PendingEditArgs>,
    /// Messages typed and submitted while an agent turn is in flight.
    /// Mirrors the daemon's queue (GOALS §1c) for display; the daemon
    /// is the source of truth — these get cleared on `ThinkingStarted`
    /// because that event implies the daemon just drained the queue
    /// into the next inference round.
    queue: Vec<String>,
    /// Submitted user messages (excluding queued ones). Used for Up/Down
    /// shell-style history navigation in the composer.
    prompt_history: Vec<String>,
    /// Index into `prompt_history` for history navigation. `0` means
    /// "at the live buffer" (no history offset); `1` = most recent, etc.
    prompt_history_cursor: usize,
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
    /// Last cursor-shape we asked the terminal to use. Tracked so we
    /// only re-issue the escape when the desired shape changes (most
    /// terminals tolerate redundant `SetCursorStyle` writes but a few
    /// blink visibly).
    last_cursor_shape: Option<CursorShape>,
    /// Highlighted index in the `@`-popup. Reset to 0 whenever the
    /// composer's at-query changes; bumped by Up/Down while the popup
    /// is open.
    at_selected: usize,
    /// True once the user dismissed the `@`-popup with `Esc`. Stays
    /// suppressed until the active `@partial` token is dropped (e.g.
    /// whitespace appears after `@` or the `@` is deleted).
    at_dismissed: bool,
    /// `/new` was invoked; the event loop services it on the next tick
    /// (needs the terminal handle for `insert_before` so the existing
    /// history spills to scrollback before the welcome header is
    /// reprinted above the viewport).
    pending_new_session: bool,
    /// Provider-reported usage from the most recent round-trip.
    /// Preferred over the local tiktoken estimate in the context
    /// indicator; `None` until the first call returns.
    last_usage: Option<crate::tokens::TokenUsage>,
}

/// Args cached at `ToolStart` for an `edit` / `editunlock` call so the
/// matching `ToolEnd` can build a `HistoryEntry::Diff`. We don't keep
/// the whole `Value` because we only need three fields.
#[derive(Debug, Clone)]
struct PendingEditArgs {
    path: String,
    old: String,
    new: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorShape {
    /// Steady vertical bar — used in Insert mode (and when vim is
    /// disabled). Explicit rather than `DefaultUserShape` because many
    /// modern terminals default to a block cursor; without an explicit
    /// bar, Insert mode would visually match Normal.
    Bar,
    /// Solid block — used in Normal / Operator-pending mode.
    Block,
}

impl App {
    pub fn new(project: Option<&Path>) -> Self {
        let launch = welcome::load(project);
        let tui_cfg = load_tui_config(&launch.cwd);
        let vim_setting = tui_cfg.vim_mode;
        let thinking_setting = tui_cfg.thinking;
        let markdown_opts = MarkdownOpts {
            agent: tui_cfg.render_agent_markdown,
            user: tui_cfg.render_user_markdown,
        };
        let mut composer = Composer::new(vim_setting.vim_enabled());
        // We start in Insert mode regardless — landing in Normal on
        // first keystroke is jarring for users new to the TUI. The
        // hint (when enabled) tells them how to switch back if they
        // Esc out.
        composer.set_vim_mode(VimMode::Insert);

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

        let diff_style = tui_cfg.diff_style;
        let mut app = Self {
            launch,
            composer,
            vim_setting,
            thinking_setting,
            markdown_opts,
            diff_style,
            pending_edit_args: HashMap::new(),
            queue: Vec::new(),
            prompt_history: Vec::new(),
            prompt_history_cursor: 0,
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
            last_cursor_shape: None,
            at_selected: 0,
            at_dismissed: false,
            pending_new_session: false,
            last_usage: None,
        };
        app.pane_height = app.geometry().desired_pane_height();
        // First-run convenience: if the daemon prompt doesn't gate
        // startup, open the Add-Provider wizard immediately when no
        // providers are configured. The prompt-resolution branches
        // call this same helper after the user dismisses the daemon
        // prompt.
        if app.daemon_prompt.is_none() {
            app.maybe_open_add_provider_wizard();
        }
        app
    }

    /// If the user has no providers configured in the active config
    /// layer, open `/settings → Providers → Add` directly. No-op when
    /// providers already exist or when the settings dialog is already
    /// open. Evaluated each launch so emptying the providers list
    /// re-triggers the wizard on the next start.
    fn maybe_open_add_provider_wizard(&mut self) {
        if self.dialog.is_active() {
            return;
        }
        if !crate::tui::settings::Dialog::has_no_providers(&self.launch.cwd) {
            return;
        }
        self.dialog = crate::tui::settings::Dialog::open_providers_add(&self.launch.cwd);
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
            self.queue_lines(),
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

        // We *don't* enable mouse capture. Capturing mouse events lets
        // us route chip clicks to expand reasoning, but it also steals
        // scroll-wheel events from the terminal (no native scrollback
        // scroll) and breaks native text selection / copy-on-release.
        // Users care more about scroll + select than about click-to-
        // expand, so we keep mouse off and rely on the `Ctrl+R`
        // shortcut for expanding the most-recent reasoning block (see
        // [`Self::toggle_recent_reasoning`]).

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

        // Mouse capture was never enabled (see comment in run()); no
        // need to disable it on the way out.
        if kbd_enhanced {
            let _ = crossterm::execute!(stdout(), PopKeyboardEnhancementFlags);
        }
        // Always restore the user's default cursor shape on exit —
        // otherwise we'd leak a steady-block cursor into their shell
        // if they quit while in Normal mode.
        let _ = crossterm::execute!(stdout(), SetCursorStyle::DefaultUserShape);
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
                if matches!(
                    entry,
                    HistoryEntry::User { .. } | HistoryEntry::Agent { .. }
                ) {
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
            // `insert_before` handles the scroll region itself; we no
            // longer need to follow it with `terminal.clear()` (the
            // old hand-rolled spill path's flicker source).
            self.maybe_spill_history(terminal)?;
            self.maybe_service_new_session(terminal)?;
            terminal.draw(|frame| self.render(frame))?;
            self.sync_cursor_shape();

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
    /// push them into terminal scrollback via
    /// [`ratatui::Terminal::insert_before`]. The terminal-side scroll
    /// region avoids the flicker the previous hand-rolled implementation
    /// produced (overwriting viewport rows, scrolling, then forcing a
    /// full `terminal.clear()` — three repaints per spill).
    fn maybe_spill_history(&mut self, terminal: &mut DefaultTerminal) -> Result<bool> {
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
        let plain: Vec<String> = items.iter().flat_map(|e| entry_to_plain_lines(e)).collect();
        let n = plain.len() as u16;
        if n > 0 {
            terminal.insert_before(n, |buf| {
                use ratatui::text::Line;
                use ratatui::widgets::{Paragraph, Widget};
                let lines: Vec<Line<'static>> =
                    plain.iter().map(|s| Line::raw(s.clone())).collect();
                Paragraph::new(lines).render(buf.area, buf);
            })?;
        }
        Ok(true)
    }

    /// `/new` was invoked: spill the current chat into terminal
    /// scrollback, reprint the welcome header above the viewport, and
    /// drop the daemon-attached runner so the next user message creates
    /// a fresh session.
    fn maybe_service_new_session(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        if !self.pending_new_session {
            return Ok(());
        }
        self.pending_new_session = false;

        // Spill the visible history first so the user can scroll up to
        // see what was on screen before the reset.
        self.finalize_pending();
        if !self.history.is_empty() {
            let plain: Vec<String> = self
                .history
                .iter()
                .flat_map(|entry| {
                    let mut lines = entry_to_plain_lines(entry);
                    if matches!(
                        entry,
                        HistoryEntry::User { .. } | HistoryEntry::Agent { .. }
                    ) {
                        lines.push(String::new());
                    }
                    lines
                })
                .collect();
            let n = plain.len() as u16;
            if n > 0 {
                terminal.insert_before(n, |buf| {
                    use ratatui::text::Line;
                    use ratatui::widgets::{Paragraph, Widget};
                    let lines: Vec<Line<'static>> =
                        plain.iter().map(|s| Line::raw(s.clone())).collect();
                    Paragraph::new(lines).render(buf.area, buf);
                })?;
            }
        }

        // Reset transcript state.
        self.history.clear();
        self.queue.clear();
        self.pending = None;
        self.clickable_rows.clear();
        self.chat_area = None;
        // prompt_history is shell-style across sessions — keep it.
        self.prompt_history_cursor = 0;
        // Reload from disk in case settings changed and refresh the
        // greeting.
        self.reload_launch_info();
        self.reload_tui_config();

        // Reprint the welcome header into scrollback. Use raw stdout
        // writes so the ANSI styling survives (ratatui's `Paragraph`
        // would render the escapes as literal characters).
        let header = welcome::header_lines(&self.launch);
        insert_above_viewport(self.pane_height, &header)?;
        // ratatui's buffer no longer reflects the actual terminal —
        // force a full repaint of the viewport on the next draw.
        terminal.clear()?;

        // Drop the runner so the next submit re-attaches the daemon
        // with `session_id: None`, opening a fresh session.
        self.agent_runner = None;

        Ok(())
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

        // Ctrl+R toggles the most-recent agent message's reasoning
        // block expand/collapse. (See the doc comment on
        // `toggle_recent_reasoning` for why this is a keybind rather
        // than a click handler.)
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('r')) {
            self.toggle_recent_reasoning();
            return false;
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

    fn handle_key_insert(&mut self, key: KeyEvent) -> bool {
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
                        self.at_selected = (self.at_selected + n - 1) % n;
                    }
                    return false;
                }
                KeyCode::Down => {
                    let n = self.at_suggestions().len();
                    if n > 0 {
                        self.at_selected = (self.at_selected + 1) % n;
                    }
                    return false;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    if self.accept_at_suggestion() {
                        return false;
                    }
                    // Fall through to default Enter handling if accept
                    // failed (e.g. no suggestions to take).
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
                self.composer.delete_left();
                self.refresh_at_dismiss();
                false
            }
            KeyCode::Delete => {
                self.composer.delete_right();
                self.refresh_at_dismiss();
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
                self.prompt_history_cursor = 0;
                self.refresh_at_dismiss();
                self.at_selected = 0;
                false
            }
            _ => false,
        }
    }

    /// Shell-style "go back through prompt history" — the Up key.
    ///
    /// State machine:
    /// - Already navigating history (`prompt_history_cursor > 0`):
    ///   step one entry older, if any.
    /// - Buffer empty *and* queue non-empty: unqueue (matches the
    ///   existing pop-from-queue behavior).
    /// - Cursor at top of buffer (first line, column 0): enter history
    ///   mode and load the most-recent prior message.
    /// - Otherwise: move cursor up within the buffer.
    ///
    /// Once in history mode, we *don't* fall back to cursor-move
    /// behavior even if `set()` placed the cursor at end-of-buffer —
    /// the loaded message replaces the live edit, and the user expects
    /// the next Up to keep going back, not to position within the
    /// recalled message. That was the previous bug.
    fn history_up(&mut self) {
        if self.prompt_history_cursor > 0 {
            if self.prompt_history_cursor < self.prompt_history.len() {
                self.prompt_history_cursor += 1;
                let idx = self.prompt_history.len() - self.prompt_history_cursor;
                self.composer.set(self.prompt_history[idx].clone());
            }
            return;
        }
        if self.composer.is_empty() && !self.queue.is_empty() {
            self.composer.set(self.queue.pop().unwrap());
            return;
        }
        if cursor_on_first_line(self.composer.text(), self.composer.cursor())
            && !self.prompt_history.is_empty()
        {
            self.prompt_history_cursor = 1;
            let idx = self.prompt_history.len() - 1;
            self.composer.set(self.prompt_history[idx].clone());
            return;
        }
        self.composer.move_up();
    }

    /// Counterpart to [`Self::history_up`]. When in history mode, step
    /// toward newer entries; at the newest, clear back to an empty
    /// composer. Otherwise just move the cursor down within the buffer.
    fn history_down(&mut self) {
        if self.prompt_history_cursor > 0 {
            self.prompt_history_cursor -= 1;
            if self.prompt_history_cursor == 0 {
                self.composer.clear();
            } else {
                let idx = self.prompt_history.len() - self.prompt_history_cursor;
                self.composer.set(self.prompt_history[idx].clone());
            }
            return;
        }
        self.composer.move_down();
    }

    /// If the composer no longer has an active `@partial` token, clear
    /// the dismissal latch so the next `@` reopens the popup. Otherwise
    /// (token still present) keep the existing state untouched.
    fn refresh_at_dismiss(&mut self) {
        if self.composer.at_query().is_none() {
            self.at_dismissed = false;
            self.at_selected = 0;
        }
    }

    /// Accept the currently-highlighted `@`-suggestion: replace the
    /// active `@partial` with the chosen path (trailing `/` for dirs).
    /// Returns true if a replacement was applied.
    fn accept_at_suggestion(&mut self) -> bool {
        let suggestions = self.at_suggestions();
        if suggestions.is_empty() {
            return false;
        }
        let idx = self.at_selected.min(suggestions.len() - 1);
        let sug = &suggestions[idx];
        self.composer.replace_at_token(&sug.replacement);
        self.at_selected = 0;
        // If this was a file, the popup auto-closes (no further token to
        // expand). For directories we leave the `@dir/` open so the user
        // can keep narrowing — `at_query` will return the new partial.
        if !sug.is_dir {
            self.at_dismissed = true;
        }
        true
    }

    fn handle_key_normal(&mut self, key: KeyEvent) -> bool {
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
                // Enter submits even from Normal mode — matches what
                // most TUIs do, so users don't have to switch to
                // Insert to send.
                self.composer.set_pending_g(false);
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
                // Default: any char key clears the pending `g`; the
                // `g` arm below re-arms it if applicable.
                self.composer.set_pending_g(false);
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
    fn handle_key_operator(&mut self, key: KeyEvent, op: Operator) -> bool {
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

        // Expand any `@path[:range]` tags into fenced file/dir blocks
        // before dispatch (GOALS §1e). The displayed user message keeps
        // the original `@`-form; only the wire payload gets inlined.
        let wire = crate::tui::file_tag::expand_tags(&submitted, &self.launch.cwd);

        // If a turn is in flight, the daemon will queue this message
        // and fold it into the next inference call (GOALS §1c). Track
        // it locally so the user sees what's pending; cleared when the
        // daemon emits `ThinkingStarted` (its drain signal).
        let agent_busy = self.pending.is_some();
        if agent_busy {
            self.queue.push(submitted.clone());
        } else {
            // No queueing — render as the user's turn immediately.
            self.history.push(HistoryEntry::User {
                text: submitted.clone(),
                timestamp: chrono::Local::now(),
            });

            // Track for Up/Down history navigation.
            self.prompt_history.push(submitted.clone());
            self.prompt_history_cursor = 0;
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
        // Re-enter Normal mode on submit when vim is enabled, so the
        // composer is ready to be navigated without typing into it.
        // Mirror Insert otherwise.
        if self.composer.vim_enabled() {
            self.composer.set_vim_mode(VimMode::Insert);
        }
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
                // Daemon drains its queue right before opening the next
                // inference round (driver.rs). Mirror that here so the
                // queued messages now appear as the user's next turn
                // in history rather than silently vanishing.
                if !self.queue.is_empty() {
                    let folded = self.queue.join("\n\n");
                    self.queue.clear();
                    self.history.push(HistoryEntry::User {
                        text: folded,
                        timestamp: chrono::Local::now(),
                    });
                }
                self.pending = Some(new_pending(agent));
            }
            TurnEvent::AssistantTextDelta { agent, delta } => {
                let p = self.pending.get_or_insert_with(|| new_pending(agent));
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
                let p = self.pending.get_or_insert_with(|| new_pending(agent));
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
            TurnEvent::ToolStart {
                tool,
                args,
                call_id,
                ..
            } => {
                self.finalize_pending();
                if is_edit_tool(&tool) {
                    if let Some(captured) = extract_edit_args(&args) {
                        self.pending_edit_args.insert(call_id, captured);
                        // Diff replaces both the placeholder and the
                        // result line — wait for ToolEnd to push the
                        // entry.
                        return;
                    }
                }
                let short = agent_runner::short_args(&args);
                self.history.push(HistoryEntry::Plain {
                    line: format!("  → {tool}({short})"),
                });
            }
            TurnEvent::ToolEnd {
                tool,
                output,
                truncated,
                call_id,
                ..
            } => {
                if let Some(args) = self.pending_edit_args.remove(&call_id) {
                    self.history.push(HistoryEntry::Diff {
                        tool,
                        path: args.path,
                        old: args.old,
                        new: args.new,
                    });
                    return;
                }
                let snippet = agent_runner::first_line(&output, 200);
                let mark = if truncated { " (truncated)" } else { "" };
                self.history.push(HistoryEntry::Plain {
                    line: format!("  ✓ {tool}: {snippet}{mark}"),
                });
            }
            TurnEvent::ToolError {
                tool,
                error,
                call_id,
                ..
            } => {
                self.finalize_pending();
                // Drop any cached args from a paired ToolStart that
                // never produced a ToolEnd — the diff would be
                // misleading on a hard failure.
                self.pending_edit_args.remove(&call_id);
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
            TurnEvent::Usage { usage, .. } => {
                self.last_usage = Some(usage);
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
                expanded,
                reasoning,
                ..
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

    /// Push the right cursor shape to the terminal based on vim mode.
    /// Idempotent — only writes when the desired shape changes.
    fn sync_cursor_shape(&mut self) {
        let desired = if self.composer.vim_enabled()
            && !matches!(self.composer.vim_mode(), VimMode::Insert)
        {
            CursorShape::Block
        } else {
            CursorShape::Bar
        };
        if self.last_cursor_shape == Some(desired) {
            return;
        }
        let style = match desired {
            CursorShape::Block => SetCursorStyle::SteadyBlock,
            CursorShape::Bar => SetCursorStyle::SteadyBar,
        };
        let _ = crossterm::execute!(stdout(), style);
        self.last_cursor_shape = Some(desired);
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
            "new" => {
                self.pending_new_session = true;
                return false;
            }
            "compact" => "/compact: stub — context compaction not wired yet.",
            "prune" => "/prune: stub — history pruning not wired yet.",
            "sessions" | "resume" => {
                "/sessions: stub — session-picker UI not wired yet. The wire RPCs \
                 (ListSessions with project_id/parent filters, ForkSession, \
                 RenameSession, DeleteSession) are live in the daemon."
            }
            "fork" => {
                "/fork: stub — the ForkSession RPC is live in the daemon; the TUI \
                 re-attach flow on top of it ships in a later cut."
            }
            "session" => {
                "/session: subcommand router not wired yet. `/session rename <title>` \
                 will call the RenameSession RPC once the AgentRunner exposes it."
            }
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

    /// Re-read the TUI-side config (vim mode, thinking display,
    /// markdown rendering) so changes made via `/settings` take effect
    /// immediately on dialog close.
    fn reload_tui_config(&mut self) {
        let tui_cfg = load_tui_config(&self.launch.cwd);
        self.vim_setting = tui_cfg.vim_mode;
        self.thinking_setting = tui_cfg.thinking;
        self.markdown_opts = MarkdownOpts {
            agent: tui_cfg.render_agent_markdown,
            user: tui_cfg.render_user_markdown,
        };
        self.diff_style = tui_cfg.diff_style;
        let vim_enabled = self.vim_setting.vim_enabled();
        if self.composer.vim_enabled() != vim_enabled {
            self.composer.set_vim_enabled(vim_enabled);
            // Mode stays whatever the composer was in; if vim flipped
            // off the composer will treat further input as Insert.
        }
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
                let resolved = match models_fetch::resolve_provider_request(id, &entry) {
                    Ok(r) => r,
                    Err(e) => {
                        push(&progress, format!("/fetch-models: {id} skipped — {e}"));
                        continue;
                    }
                };
                match models_fetch::fetch_models(
                    &resolved.base_url,
                    &resolved.headers,
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

    /// True when the `@`-popup should be drawn: the composer reports an
    /// active `@partial` token and the user hasn't dismissed it via Esc.
    fn at_popup_active(&self) -> bool {
        !self.at_dismissed && self.composer.at_query().is_some()
    }

    fn at_suggestions(&self) -> Vec<crate::tui::file_tag::Suggestion> {
        match self.composer.at_query() {
            Some(q) => crate::tui::file_tag::suggestions(&self.launch.cwd, q),
            None => Vec::new(),
        }
    }

    fn popup_lines(&self) -> u16 {
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
    fn show_vim_hint(&self) -> bool {
        self.vim_setting.show_hint()
            && self.composer.vim_enabled()
            && self.composer.vim_mode() == VimMode::Normal
    }

    /// Height of the queued-messages strip above the input box. Zero
    /// when nothing's queued; otherwise top border (1) + N messages +
    /// shared bottom (1). The shared bottom is the queue's bottom AND
    /// the input's top, with T-joins where the inset side rails meet
    /// the input's wider top edge.
    fn queue_lines(&self) -> u16 {
        if self.queue.is_empty() {
            0
        } else {
            2 + self.queue.len() as u16
        }
    }

    fn input_height(&self) -> u16 {
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

    fn total_history_lines(&self) -> u16 {
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
    }

    /// Queued-messages box. Inset one column from each side of the
    /// input box; rounded top corners (`╭ ╮`); white border throughout;
    /// shared bottom row with the input box rendered as `╭┴────┴╮`
    /// (input's rounded top corners with `┴` T-joins where the queue's
    /// inset side rails terminate). The shared row counts as the
    /// queue's bottom border AND the input's top border.
    fn render_queue(&self, frame: &mut ratatui::Frame, area: Rect) {
        if area.height < 2 || area.width < 5 || self.queue.is_empty() {
            return;
        }
        let white = Color::White;
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
            Span::styled(format!("╭{top_bar}╮"), Style::default().fg(white)),
            Span::raw(" ".repeat(inset)),
        ]));

        // Content rows: `  │ message │  `.
        for msg in &self.queue {
            let body = first_line_truncated(msg, inner_w);
            let body_w = body.chars().count();
            let trailing = inner_w.saturating_sub(body_w);
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(inset)),
                Span::styled("│", Style::default().fg(white)),
                Span::raw(" "),
                Span::styled(body, Style::default().fg(dim_white)),
                Span::raw(" ".repeat(trailing)),
                Span::raw(" "),
                Span::styled("│", Style::default().fg(white)),
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
            Style::default().fg(white),
        )]));

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_history(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.chat_area = Some(area);
        let area_h = area.height as usize;

        let mut all: Vec<Line<'static>> = Vec::new();
        // `targets[i]` carries the history-entry index whose thinking
        // chip occupies row `i` of `all`, or `None` otherwise.
        let mut targets: Vec<Option<usize>> = Vec::new();
        for (idx, entry) in self.history.iter().enumerate() {
            let Rendered { lines, chip_row } = render_entry(
                entry,
                area.width,
                self.thinking_setting,
                self.markdown_opts,
                self.diff_style,
            );
            let chip_abs = chip_row.map(|cr| all.len() + cr);
            for i in 0..lines.len() {
                targets.push(if Some(all.len() + i) == chip_abs {
                    Some(idx)
                } else {
                    None
                });
            }
            all.extend(lines);
            // Insert a one-line gap after agent entries to separate from
            // the next user message or pending turn. Skip consecutive
            // agents so multi-turn blocks read as a single block.
            if matches!(entry, HistoryEntry::User { .. }) {
                all.push(Line::default());
                targets.push(None);
            } else if matches!(entry, HistoryEntry::Agent { .. }) {
                let prev_is_agent = idx
                    .checked_sub(1)
                    .map(|i| matches!(self.history[i], HistoryEntry::Agent { .. }))
                    .unwrap_or(false);
                if !prev_is_agent {
                    all.push(Line::default());
                    targets.push(None);
                }
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

        frame.render_widget(Paragraph::new(visible).wrap(Wrap { trim: false }), area);
    }

    fn render_input(&self, frame: &mut ratatui::Frame, area: Rect, queue_above: bool) -> Position {
        // When the queue strip is above, its shared bottom row IS our
        // top border — render only sides + bottom here.
        let borders = if queue_above {
            Borders::LEFT | Borders::RIGHT | Borders::BOTTOM
        } else {
            Borders::ALL
        };
        let input_block = Block::default()
            .borders(borders)
            .border_type(BorderType::Rounded)
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
        // Wrap-aware visual cursor position: a single logical line
        // that's wider than the inner width spans multiple visible
        // rows, and we need the cursor to follow.
        let inner_w = input_inner.width as usize;
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
        let para = Paragraph::new(lines)
            .block(input_block)
            .wrap(Wrap { trim: false })
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
    fn context_indicator_text(&self) -> String {
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
    fn context_tokens(&self) -> u32 {
        if let Some(u) = self.last_usage {
            return u.total().min(u32::MAX as u64) as u32;
        }
        self.estimate_context_tokens()
    }

    /// cl100k_base token count over visible chat content. Tools and
    /// system prompts aren't included — they live on the engine side.
    /// Provider-native counts will replace this where available
    /// (GOALS §10 / plan §3h); cl100k_base is the documented fallback.
    fn estimate_context_tokens(&self) -> u32 {
        let mut tokens: usize = 0;
        for entry in &self.history {
            tokens += match entry {
                HistoryEntry::User { text, .. } => crate::tokens::count(text),
                HistoryEntry::Plain { line } => crate::tokens::count(line),
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

    fn render_popup(&self, frame: &mut ratatui::Frame, area: Rect) {
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
        let mut matches = slash_matches(query);
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

    fn render_at_popup(&self, frame: &mut ratatui::Frame, area: Rect) {
        let suggestions = self.at_suggestions();
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let mut lines: Vec<Line<'static>> = if suggestions.is_empty() {
            vec![Line::from(vec![
                Span::raw("  "),
                Span::styled("no matching file", Style::default().fg(Color::Red)),
            ])]
        } else {
            let selected = self.at_selected.min(suggestions.len().saturating_sub(1));
            suggestions
                .iter()
                .take(AUTOCOMPLETE_ROWS as usize)
                .enumerate()
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
    let mut visual_row: usize = 0;
    let lines: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.split('\n').collect()
    };
    for (i, line) in lines.iter().enumerate().take(cursor_line) {
        // Every logical line carries the same `prefix` width because
        // the renderer inserts the prefix-width indent on lines after
        // the first too. Result: line wraps identically.
        let visual_chars = prefix + line.chars().count();
        let n = if visual_chars == 0 {
            1
        } else {
            visual_chars.div_ceil(wrap_width)
        };
        visual_row += n.max(1);
        let _ = i;
    }
    let offset = prefix + cursor_col;
    let row_within = offset / wrap_width;
    let col_within = offset % wrap_width;
    (visual_row + row_within, col_within)
}

/// True for tools that take an `old_string` / `new_string` pair we
/// can render as a diff. `write` / `writeunlock` aren't in here yet
/// because the engine doesn't surface the pre-write file content (see
/// `flagged-for-christopher.md`).
fn is_edit_tool(tool: &str) -> bool {
    matches!(tool, "edit" | "editunlock")
}

/// Approximate row count for a `Diff` entry, used by the chat-pane
/// sizing math. SideBySide ≈ max(old, new); Inline ≈ old + new. The
/// chat sizer doesn't know which mode is active at this point, so
/// we use the inline (upper-bound) estimate to avoid undersized
/// panes — slight over-allocation is cheaper than clipping.
fn diff_row_estimate(old: &str, new: &str) -> u16 {
    let old_lines = old.matches('\n').count() as u16 + 1;
    let new_lines = new.matches('\n').count() as u16 + 1;
    old_lines.saturating_add(new_lines).saturating_add(1) // +1 for header
}

/// Pull `(path, old, new)` out of an edit tool's args. Returns
/// `None` when any field is missing; the caller falls back to the
/// generic Plain rendering in that case.
fn extract_edit_args(args: &serde_json::Value) -> Option<PendingEditArgs> {
    let path = args.get("path")?.as_str()?.to_string();
    let old = args.get("old_string")?.as_str()?.to_string();
    let new = args.get("new_string")?.as_str()?.to_string();
    Some(PendingEditArgs { path, old, new })
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
        HistoryEntry::Diff {
            tool,
            path,
            old,
            new,
        } => {
            // Plain-lines is what the "spill to scrollback" path uses
            // on `/new`. Reduce the diff to a tool-result-style
            // summary plus the textual diff body in unified form —
            // anything fancier would need ratatui Lines which the
            // plain-text dump can't render.
            let added = new.lines().count();
            let removed = old.lines().count();
            let mut out = vec![format!("  ✓ {tool}: {path} (+{added} −{removed})")];
            let diff = similar::TextDiff::from_lines(old.as_str(), new.as_str());
            for group in diff.grouped_ops(3) {
                if out.len() > 1 {
                    out.push("    …".to_string());
                }
                for op in group {
                    for change in diff.iter_changes(&op) {
                        let v = change.value().trim_end_matches('\n');
                        let prefix = match change.tag() {
                            similar::ChangeTag::Delete => "- ",
                            similar::ChangeTag::Insert => "+ ",
                            similar::ChangeTag::Equal => "  ",
                        };
                        out.push(format!("  {prefix}{v}"));
                    }
                }
            }
            out
        }
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

/// Walk the layered-config discovery and return the `tui` slice from
/// the first `extended-config.json` we find. Defaults to
/// [`crate::config::extended::TuiConfig::default`] when no config
/// exists or the file is unreadable / malformed (mirroring the rest of
/// `extended.rs`'s tolerant loading).
fn load_tui_config(cwd: &Path) -> crate::config::extended::TuiConfig {
    for dir in discover_config_dirs(cwd) {
        let path = dir.path.join("extended-config.json");
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(cfg) = serde_json::from_slice::<ExtendedConfig>(&bytes)
        {
            return cfg.tui;
        }
    }
    crate::config::extended::TuiConfig::default()
}

/// True when `cursor` falls on the first line of `text` (i.e. there's
/// no `\n` in `text[..cursor]`). Used by history navigation to decide
/// "is the user at the top of the buffer?" — only then does Up step
/// into prompt history, otherwise it moves the cursor up one line.
fn cursor_on_first_line(text: &str, cursor: usize) -> bool {
    !text[..cursor.min(text.len())].contains('\n')
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
