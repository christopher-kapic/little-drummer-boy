//! Top-level TUI state and event loop.
//!
//! Mouse capture is gated by `tui.mouse_capture` (default on, plan.md
//! T8.c). With capture on: clickable chips, click-to-position-cursor
//! in the composer, and drag-select in chat history (T8.f). Native
//! terminal selection still works under capture if the user holds the
//! terminal's bypass modifier (Shift on most Linux/Windows Terminal,
//! Option on iTerm2, Fn on macOS Terminal). With capture off: the
//! terminal handles wheel/select/copy natively and `MouseEvent`s
//! never reach this loop — the user falls back to `Ctrl+J` to expand
//! the most-recent reasoning block.

use std::collections::HashMap;
use std::io::stdout;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

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
        name: "mouse",
        description: "Toggle mouse capture (click-to-position, drag-select) on/off",
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
    /// In-progress composer text saved when the user first pressed
    /// Up to enter history mode. Restored when they walk back past
    /// the newest entry (cursor going `1 → 0`). `None` when not in
    /// history mode or when entry happened from an empty composer.
    staged_draft: Option<String>,
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
    /// Last-rendered composer-input `Rect` (the outer rect — block
    /// border included). Used by `handle_mouse` to route clicks into
    /// click-to-position-cursor (plan.md T8.d).
    input_area: Option<Rect>,
    /// Logical-line scroll offset for the chat history pane. `0` =
    /// pinned to the bottom (live). Higher = scrolled further back in
    /// time. Bumped by mouse wheel when capture is on; clamped by
    /// `render_history` so we never scroll past the top.
    chat_scroll_offset: usize,
    /// How tall (logical lines) the full chat content was at the last
    /// render. Updated each `render_history` and consulted by the
    /// mouse-wheel handler to clamp scroll-back to a valid maximum.
    chat_total_lines: usize,
    /// How many logical lines fit in the chat pane at the last render.
    /// Same purpose — clamp scrollback so the bottom of the visible
    /// window can't go below the top of the content.
    chat_visible_lines: usize,
    /// In-app drag-select state for chat content (plan.md T8.f). Set
    /// when the user mouse-downs in the chat area; updated on drag;
    /// committed on release. `Ctrl+Shift+C` copies the underlying
    /// plaintext via `clipboard::copy_plain` (OSC52 → SSH-safe).
    selection: Option<Selection>,
    /// Snapshot of the chat area's rendered cells, one row per outer
    /// element, one cell per inner element. Each cell's `String` is
    /// the cell's `symbol()` — typically one char, but multi-byte for
    /// non-ASCII and an empty marker for the continuation cell of a
    /// wide glyph. Populated by `render_history` after the paragraph
    /// widget writes to the buffer. Used by the copy path so we don't
    /// have to redo ratatui's wrap math to extract the selected
    /// plaintext.
    chat_text_grid: Vec<Vec<String>>,
    /// Parallel to `chat_text_grid`: `chat_cont_rows[i]` is `true`
    /// when visible row `i` is a soft-wrap continuation of the
    /// previous logical line. The copy path joins continuations with
    /// a space, real line boundaries with a newline — so pasted
    /// agent text reconstructs the original paragraphs rather than
    /// preserving the screen-level wraps.
    chat_cont_rows: Vec<bool>,
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
    /// Ctrl+G was pressed — the event loop suspends ratatui, runs
    /// `$EDITOR` against the composer text, then reloads the file back
    /// into the composer.
    pending_external_edit: bool,
    /// Whether crossterm mouse capture is currently enabled. Tracks the
    /// real terminal state so the settings toggle can push/pop the
    /// escape sequence without double-enabling. Sourced from
    /// `tui.mouse_capture` at startup; mutated when the user toggles
    /// the setting mid-session.
    mouse_capture: bool,
    /// User's `tui.exit_tail_lines` setting (GOALS §1d). Cached at
    /// startup so the exit-tail dump survives the dialog being closed.
    exit_tail_lines: i32,
    /// User's `tui.rich_text_copy` setting. Gates the `Ctrl+Shift+Y`
    /// keybind that copies the last agent message as HTML to the
    /// system clipboard (plan.md T8.g).
    rich_text_copy: bool,
    /// Active right-click context menu in the chat area. Modal while
    /// `Some` — intercepts every key + mouse event.
    context_menu: Option<crate::tui::context_menu::ContextMenu>,
    /// Transient FYI message overlaid on the status line
    /// (TUI-design-philosophy §7). 3-second TTL; dismissed early by
    /// any user interaction (keystroke or mouse click/wheel).
    toast: Option<Toast>,
}

/// Toast intent — drives the message's foreground color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Success,
    Error,
    Info,
}

#[derive(Debug, Clone)]
struct Toast {
    text: String,
    kind: ToastKind,
    expires_at: Instant,
}

/// Default toast lifetime per TUI-design-philosophy §7.
const TOAST_TTL: Duration = Duration::from_secs(3);

/// Args cached at `ToolStart` for an `edit` / `editunlock` call so the
/// matching `ToolEnd` can build a `HistoryEntry::Diff`. We don't keep
/// the whole `Value` because we only need three fields.
#[derive(Debug, Clone)]
struct PendingEditArgs {
    path: String,
    old: String,
    new: String,
}

/// Drag-select state for the chat area (plan.md T8.f). Coordinates
/// are absolute terminal cells; we re-derive chat-relative positions
/// at render time so resize / scroll changes don't desync.
#[derive(Debug, Clone, Copy)]
struct Selection {
    /// Where the drag started.
    anchor: (u16, u16),
    /// Where the drag is right now (or where it ended on mouse-up).
    focus: (u16, u16),
    /// True while the left button is still held. False once released
    /// (selection persists for copy until Esc or a new selection).
    active: bool,
}

impl Selection {
    /// Normalize into reading-order `(start, end)` cells, both
    /// inclusive. When the user drags right-to-left or bottom-to-top,
    /// anchor > focus; this swaps them so callers can iterate the
    /// selection in a single direction.
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let (a_col, a_row) = self.anchor;
        let (f_col, f_row) = self.focus;
        if (a_row, a_col) <= (f_row, f_col) {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
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
        let mouse_capture = tui_cfg.mouse_capture;
        let exit_tail_lines = tui_cfg.exit_tail_lines;
        let rich_text_copy = tui_cfg.rich_text_copy;
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
            staged_draft: None,
            history: Vec::new(),
            pending: None,
            started_at: Instant::now(),
            repo_status,
            dialog: Dialog::None,
            model_picker: None,
            daemon_prompt,
            daemon_connected,
            fetch_models_progress: Arc::new(Mutex::new(Vec::new())),
            agent_runner: None,
            chat_area: None,
            input_area: None,
            chat_scroll_offset: 0,
            chat_total_lines: 0,
            chat_visible_lines: 0,
            selection: None,
            chat_text_grid: Vec::new(),
            chat_cont_rows: Vec::new(),
            clickable_rows: Vec::new(),
            last_cursor_shape: None,
            at_selected: 0,
            at_dismissed: false,
            pending_new_session: false,
            last_usage: None,
            pending_external_edit: false,
            mouse_capture,
            exit_tail_lines,
            rich_text_copy,
            context_menu: None,
            toast: None,
        };
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
        // Print the welcome header to normal terminal output *before*
        // we enter the alt screen. It lands in the regular terminal
        // scrollback so the user can scroll back to see it after the
        // TUI exits. During the session the alt screen overlays it.
        welcome::print_header(&self.launch);

        // `try_init` enters the alternate screen and uses a full-
        // terminal viewport by default. GOALS §1d: alt screen during
        // the session for the clean full-screen experience; on exit
        // we leave alt screen and print the tail to stdout.
        let mut terminal = ratatui::try_init()?;

        let kbd_enhanced = crossterm::execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )
        .is_ok();

        // Mouse capture is configurable (tui.mouse_capture, GOALS §1
        // T8.c). On: click-to-position in composer, clickable chips,
        // drag-select in chat. Off: native terminal select + copy +
        // scroll-wheel via alternate-scroll translation. Native
        // selection still works under capture if the user holds the
        // terminal's bypass modifier (Shift / Option / Fn).
        if self.mouse_capture {
            let _ = crossterm::execute!(stdout(), EnableMouseCapture);
        }

        let refresh_handle = spawn_git_refresh(self.launch.cwd.clone(), self.repo_status.clone());

        let result = self.event_loop(&mut terminal);

        refresh_handle.abort();

        // Build the exit-tail text while we still own the alt screen
        // (history is in memory; rendering is irrelevant — we want
        // the plaintext projection of recent entries).
        let tail = self.build_exit_tail_lines();

        if self.mouse_capture {
            let _ = crossterm::execute!(stdout(), DisableMouseCapture);
        }
        if kbd_enhanced {
            let _ = crossterm::execute!(stdout(), PopKeyboardEnhancementFlags);
        }
        // Always restore the user's default cursor shape on exit —
        // otherwise we'd leak a steady-block cursor into their shell
        // if they quit while in Normal mode.
        let _ = crossterm::execute!(stdout(), SetCursorStyle::DefaultUserShape);
        // `try_restore` disables raw mode and leaves the alternate
        // screen — terminal scrollback is now visible again.
        ratatui::try_restore()?;
        // Print the tail to normal stdout. Lands in regular terminal
        // scrollback right after the welcome header that was printed
        // pre-alt-screen, so the user can scroll back through both.
        for line in tail {
            println!("{line}");
        }
        result
    }

    /// Build the tail of history as plain text lines for the post-
    /// alt-screen dump (GOALS §1d). Capped by `tui.exit_tail_lines`
    /// (default 100). `0` disables the dump entirely; `-1` returns
    /// the whole session. Returns an empty `Vec` when nothing should
    /// be printed.
    fn build_exit_tail_lines(&mut self) -> Vec<String> {
        // Finalize any in-flight pending turn first so its text shows
        // up in the dump.
        self.finalize_pending();
        if self.history.is_empty() || self.exit_tail_lines == 0 {
            return Vec::new();
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
        if self.exit_tail_lines < 0 {
            plain
        } else {
            let n = self.exit_tail_lines as usize;
            if plain.len() > n {
                plain[plain.len() - n..].to_vec()
            } else {
                plain
            }
        }
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.sync_repo_status();
            self.drain_fetch_progress();
            self.drain_agent_events();
            self.sync_active_agent();
            self.sync_mouse_capture_from_dialog();
            self.tick_toast();
            self.dialog.tick();
            // In alt-screen mode the viewport is always the full
            // terminal; no need to grow it or spill history into
            // scrollback (alt screen doesn't have scrollback). The
            // wheel-scroll path handles in-app scrollback instead.
            self.maybe_service_new_session(terminal)?;
            self.maybe_service_external_edit(terminal)?;
            terminal.draw(|frame| self.render(frame))?;
            self.sync_cursor_shape();

            if event::poll(EVENT_TICK)? {
                match event::read()? {
                    Event::Key(key) if accepts_key(&key) && self.handle_key(key) => break,
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse);
                    }
                    Event::Resize(_, _) => {
                        // Alt-screen viewport tracks the terminal
                        // size automatically; the next draw picks up
                        // the new dimensions via frame.area().
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    /// Show a transient toast (TUI-design-philosophy §7). Replaces
    /// any existing toast — newest wins, the older one is gone.
    /// 3-second TTL; cleared early by any user interaction (see the
    /// `dismiss_toast_on_interaction` hooks in handle_key and
    /// handle_mouse).
    fn show_toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast {
            text: text.into(),
            kind,
            expires_at: Instant::now() + TOAST_TTL,
        });
    }

    /// Drop the toast if it has expired. Called once per event-loop
    /// tick so a toast left untouched for 3 seconds cleans itself
    /// up without needing a new event to fire.
    fn tick_toast(&mut self) {
        if let Some(toast) = &self.toast
            && Instant::now() > toast.expires_at
        {
            self.toast = None;
        }
    }

    /// Flip `tui.mouse_capture` on disk, push/pop the live terminal
    /// state, and return a status line for the chat log. Used by the
    /// `/mouse` slash command (T8.c). Save errors degrade gracefully:
    /// we still flip the live state and report the error in the
    /// status line so the user knows the change isn't persistent.
    /// Toggle the *live* mouse-capture state and surface a toast.
    /// `/mouse` is intentionally non-persistent — useful for "try
    /// capture off for one operation" without affecting the
    /// configured default for the next session. The persistent
    /// toggle lives in `/settings → ui`.
    fn toggle_mouse_capture_inline(&mut self) {
        let new_value = !self.mouse_capture;
        let exec_ok = if new_value {
            crossterm::execute!(stdout(), EnableMouseCapture).is_ok()
        } else {
            crossterm::execute!(stdout(), DisableMouseCapture).is_ok()
        };
        if exec_ok {
            self.mouse_capture = new_value;
            let state = if new_value { "on" } else { "off" };
            self.show_toast(
                format!("/mouse: capture {state} (this session only)"),
                ToastKind::Info,
            );
        } else {
            self.show_toast(
                "/mouse: terminal rejected the capture toggle",
                ToastKind::Error,
            );
        }
    }

    /// Pick up a pending mouse-capture toggle from the settings dialog
    /// (UI page) and push/pop the crossterm capture state to match.
    /// The setting itself is persisted by the dialog's save path; this
    /// just keeps the live terminal state in sync.
    fn sync_mouse_capture_from_dialog(&mut self) {
        let Some(want) = self.dialog.take_pending_mouse_capture() else {
            return;
        };
        if want == self.mouse_capture {
            return;
        }
        let res = if want {
            crossterm::execute!(stdout(), EnableMouseCapture)
        } else {
            crossterm::execute!(stdout(), DisableMouseCapture)
        };
        if res.is_ok() {
            self.mouse_capture = want;
        }
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

    /// `/new` was invoked: clear chat history and drop the daemon-
    /// attached runner so the next user message opens a fresh session.
    /// In alt-screen mode the chat pane is the whole canvas, so the
    /// "fresh session" visual is simply an empty pane.
    fn maybe_service_new_session(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        if !self.pending_new_session {
            return Ok(());
        }
        self.pending_new_session = false;

        // Alt-screen mode: the chat pane is the whole canvas, and
        // there's no terminal scrollback to spill into. Clearing
        // history makes the chat pane empty — that's the "new
        // session" visual.
        self.finalize_pending();

        // Reset transcript state.
        self.history.clear();
        self.queue.clear();
        self.pending = None;
        self.clickable_rows.clear();
        self.chat_area = None;
        self.chat_text_grid.clear();
        self.chat_cont_rows.clear();
        self.chat_scroll_offset = 0;
        self.selection = None;
        // prompt_history is shell-style across sessions — keep it.
        self.prompt_history_cursor = 0;
        self.staged_draft = None;
        // Reload from disk in case settings changed.
        self.reload_launch_info();
        self.reload_tui_config();

        // Repaint the cleared canvas on the next draw.
        terminal.clear()?;

        // Drop the runner so the next submit re-attaches the daemon
        // with `session_id: None`, opening a fresh session.
        self.agent_runner = None;

        Ok(())
    }

    /// Ctrl+G was pressed: pop the composer text out into `$EDITOR`,
    /// then reload whatever the user wrote back into the buffer. Quits
    /// raw mode for the duration so the editor owns the terminal.
    fn maybe_service_external_edit(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        if !self.pending_external_edit {
            return Ok(());
        }
        self.pending_external_edit = false;

        let Some(editor) = std::env::var_os("EDITOR") else {
            // Defensive — we re-check here because env state can shift
            // between the keypress and now. The handler already
            // surfaced a toast when EDITOR was unset, so just bail.
            return Ok(());
        };

        // Stash the buffer in a sibling-tempfile-named so the editor's
        // syntax detection (if any) picks Markdown.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("cockpit-prompt-{}.md", std::process::id()));
        if let Err(e) = std::fs::write(&path, self.composer.text()) {
            self.history.push(HistoryEntry::Plain {
                line: format!("editor: failed to write temp file: {e}"),
            });
            return Ok(());
        }

        // Suspend ratatui's input handling for the editor invocation.
        // We disable the keyboard-enhancement flags / cursor styles
        // crossterm pushed for us, leave raw mode, and let the editor
        // own the TTY. Re-enable everything after it exits.
        use crossterm::terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        };
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();

        let status = std::process::Command::new(&editor).arg(&path).status();

        let _ = enable_raw_mode();
        let _ = crossterm::execute!(stdout(), EnterAlternateScreen);
        terminal.clear()?;

        match status {
            Ok(s) if s.success() => match std::fs::read_to_string(&path) {
                Ok(text) => {
                    // Drop a single trailing newline — most editors
                    // write one even when the user didn't add one.
                    let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
                    self.composer.set(text);
                }
                Err(e) => {
                    self.history.push(HistoryEntry::Plain {
                        line: format!("editor: failed to read temp file back: {e}"),
                    });
                }
            },
            Ok(s) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("editor: exited with {s}"),
                });
            }
            Err(e) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("editor: invoking `{}`: {e}", editor.to_string_lossy()),
                });
            }
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
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
                // Note: we deliberately do NOT reset
                // `prompt_history_cursor` here. Edits made while in
                // recall mode stay in the buffer, but pressing Down
                // back to cursor 0 still restores the original
                // staged draft — matching the user-visible spec for
                // history navigation.
                self.refresh_at_dismiss();
                self.at_selected = 0;
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
    fn history_up(&mut self) {
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
    fn history_down(&mut self) {
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

        // Submitting a new turn implies the user has finished reading
        // history — jump back to the live tail so they see the reply.
        self.chat_scroll_offset = 0;

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

    /// True when the key event represents `Ctrl+Shift+Y`. Matches both
    /// terminal-protocol shapes (legacy `Char('Y')` with Ctrl; kitty
    /// keyboard protocol `Char('y')` with both Ctrl and Shift bits).
    fn is_ctrl_shift_y(&self, key: &KeyEvent) -> bool {
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
    fn is_ctrl_shift_c(&self, key: &KeyEvent) -> bool {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match key.code {
            KeyCode::Char('C') => true,
            KeyCode::Char('c') => key.modifiers.contains(KeyModifiers::SHIFT),
            _ => false,
        }
    }

    /// Execute one of the context-menu actions. Called both when the
    /// user clicks an item and when they hit Enter on a focused item.
    /// `clicked_chat_row` is the chat-relative row that was
    /// right-clicked — used by "Copy as rich text" to find which
    /// agent message was under the click; ignored by the other
    /// actions.
    fn execute_context_menu_action(
        &mut self,
        action: crate::tui::context_menu::ContextMenuAction,
        clicked_chat_row: usize,
    ) {
        use crate::tui::context_menu::ContextMenuAction;
        let Some((title, text)) = self.agent_message_at_or_before(clicked_chat_row) else {
            self.show_toast("No agent message to copy yet.", ToastKind::Info);
            return;
        };
        let (msg, kind) = match action {
            ContextMenuAction::CopyAsRichText => {
                let html = crate::clipboard::markdown_to_html(&text);
                match crate::clipboard::copy_rich(&text, &html) {
                    Ok(()) => (format!("Copied {title} as rich text."), ToastKind::Success),
                    Err(crate::clipboard::CopyError::UnsupportedOverSsh) => {
                        // Shouldn't normally happen because the menu
                        // builder hides this option over SSH, but
                        // guard anyway so a stale menu doesn't error.
                        match crate::clipboard::copy_plain(&text) {
                            Ok(()) => (
                                format!(
                                    "SSH — copied {title} as plain text \
                                     (rich-text unavailable over SSH)."
                                ),
                                ToastKind::Success,
                            ),
                            Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
                        }
                    }
                    Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
                }
            }
            ContextMenuAction::CopyAsMarkdown => match crate::clipboard::copy_plain(&text) {
                Ok(()) => (format!("Copied {title} as markdown."), ToastKind::Success),
                Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
            },
            ContextMenuAction::CopyAsPlainText => {
                let plain = crate::clipboard::markdown_to_plain(&text);
                match crate::clipboard::copy_plain(&plain) {
                    Ok(()) => (format!("Copied {title} as plain text."), ToastKind::Success),
                    Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
                }
            }
        };
        self.show_toast(msg, kind);
    }

    /// Find the agent message whose chat row is at or before
    /// `clicked_chat_row` (so a right-click in the middle of a
    /// multi-line message still resolves to that message). Returns
    /// `(title, full message text)` or `None` if no agent message
    /// precedes the click.
    fn agent_message_at_or_before(&self, clicked_chat_row: usize) -> Option<(String, String)> {
        let Some(area) = self.chat_area else {
            return None;
        };
        // Map the chat-relative click row back to an entry index via
        // the cell-grid's owning entry. We don't have an explicit
        // row→entry map (only `clickable_rows` for chips), so walk
        // history with the same row-budget logic `render_history`
        // uses. For "agent message under click", the simpler
        // heuristic is: pick the most-recent agent message at or
        // before the click row's absolute terminal y. We approximate
        // by walking history bottom-up and returning the first agent
        // entry, since multi-line agent blocks are the common case
        // and pinpointing per-row is overkill for this UX.
        // `clicked_chat_row` is used only to honor "at or before"
        // — if it's past the last rendered row, return the last
        // agent. We don't currently have an entry-precise hit map.
        let _ = area;
        let _ = clicked_chat_row;
        self.history.iter().rev().find_map(|e| match e {
            HistoryEntry::Agent { name, text, .. } if !text.trim().is_empty() => {
                Some((format!("{name} message"), text.clone()))
            }
            _ => None,
        })
    }

    /// Build the plaintext of the active drag-selection from the
    /// cached chat grid and push it to the system clipboard via
    /// `clipboard::copy_plain` (OSC52 + arboard locally). No-op when
    /// the selection is empty or stale (chat_area moved between
    /// selection and copy).
    fn copy_selection_plaintext(&mut self) {
        let Some(sel) = self.selection else {
            return;
        };
        let Some(area) = self.chat_area else {
            return;
        };
        let (start, end) = sel.ordered();
        // Stale guard: if either selection endpoint is outside the
        // current chat area, the snapshot we have no longer
        // corresponds. Clear the selection and bail.
        if start.1 < area.y
            || end.1 >= area.y + area.height
            || start.0 < area.x
            || end.0 >= area.x + area.width
        {
            self.selection = None;
            return;
        }
        let plain =
            extract_selection_plaintext(&self.chat_text_grid, &self.chat_cont_rows, area, sel);
        if plain.is_empty() {
            return;
        }
        let (msg, kind) = match crate::clipboard::copy_plain(&plain) {
            Ok(()) => (
                format!("Copied {} chars to clipboard.", plain.chars().count()),
                ToastKind::Success,
            ),
            Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
        };
        self.show_toast(msg, kind);
        // Clear selection after a successful copy — the user got
        // what they wanted; leaving it highlighted just gets in the
        // way of the next interaction.
        self.selection = None;
    }

    /// Copy the most recent agent message to the system clipboard as
    /// rich text (HTML + plain alt). Surfaces feedback via a toast
    /// (TUI-design-philosophy §7). No-op when `tui.rich_text_copy`
    /// is off or no agent messages exist.
    fn copy_last_agent_message_as_rich_text(&mut self) {
        if !self.rich_text_copy {
            self.show_toast(
                "Rich-text copy is disabled (toggle in /settings → ui).",
                ToastKind::Info,
            );
            return;
        }
        let last_agent_text = self.history.iter().rev().find_map(|e| match e {
            HistoryEntry::Agent { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        });
        let Some(text) = last_agent_text else {
            self.show_toast("No agent message to copy yet.", ToastKind::Info);
            return;
        };
        let html = crate::clipboard::markdown_to_html(&text);
        let (msg, kind) = match crate::clipboard::copy_rich(&text, &html) {
            Ok(()) => (
                "Copied last agent message as rich text.".to_string(),
                ToastKind::Success,
            ),
            Err(crate::clipboard::CopyError::UnsupportedOverSsh) => {
                // SSH session — fall back to plain text via OSC52 so
                // the user gets at least something on the local
                // clipboard.
                match crate::clipboard::copy_plain(&text) {
                    Ok(()) => (
                        "SSH — copied as plain text (rich-text unavailable over SSH).".to_string(),
                        ToastKind::Success,
                    ),
                    Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
                }
            }
            Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
        };
        self.show_toast(msg, kind);
    }

    /// Toggle every agent message's `expanded` flag. Bound to `Ctrl+J`
    /// for keyboard-only use. If any entry is currently collapsed we
    /// expand them all; otherwise we collapse them all.
    fn toggle_recent_reasoning(&mut self) {
        let any_collapsed = self.history.iter().any(|e| {
            matches!(e,
                HistoryEntry::Agent { reasoning, expanded, .. }
                    if !reasoning.trim().is_empty() && !*expanded)
        });
        for entry in self.history.iter_mut() {
            if let HistoryEntry::Agent {
                expanded,
                reasoning,
                ..
            } = entry
                && !reasoning.trim().is_empty()
            {
                *expanded = any_collapsed;
            }
        }
    }

    /// Handle a mouse event. Routing:
    /// - context menu open → route into the menu (click to select,
    ///   click outside to dismiss);
    /// - text popup open → any click dismisses;
    /// - right-down in chat area → open the context menu (T8.f menu);
    /// - wheel up/down inside the chat area → scroll chat history;
    /// - left-down in composer input area → position the cursor (T8.d);
    /// - left-down on a chat thinking-chip → toggle reasoning expansion;
    /// - left-down on a non-chip chat row → start drag-select (T8.f);
    /// - left-drag → extend the active drag-select;
    /// - left-up → finalize drag-select (selection persists for copy).
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        // Toast dismissal on "meaningful" mouse events — clicks and
        // wheels count, motion-only / drag-continuation / release
        // don't (those are part of an in-flight gesture and the
        // first event already dismissed).
        if self.toast.is_some()
            && matches!(
                mouse.kind,
                MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        {
            self.toast = None;
        }
        // Context menu is modal too — clicks either hit an item or
        // dismiss. Wheel events while it's open are eaten so we don't
        // accidentally scroll chat underneath.
        if let Some(menu) = self.context_menu.clone() {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let full = ratatui::layout::Rect::new(0, 0, u16::MAX, u16::MAX);
                    if let Some(action) = menu.hit_test(mouse.column, mouse.row, full) {
                        self.context_menu = None;
                        self.execute_context_menu_action(action, menu.clicked_chat_row);
                    } else {
                        // Click outside the menu dismisses it without
                        // executing anything.
                        self.context_menu = None;
                    }
                }
                MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    self.context_menu = None;
                }
                _ => {}
            }
            return;
        }

        // Right-click in chat area opens the context menu.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right))
            && self.mouse_in_chat_area(&mouse)
        {
            let chat_row = self
                .chat_area
                .map(|a| (mouse.row.saturating_sub(a.y)) as usize)
                .unwrap_or(0);
            let items =
                crate::tui::context_menu::ContextMenu::build_items(crate::clipboard::is_ssh());
            self.context_menu = Some(crate::tui::context_menu::ContextMenu {
                preferred_origin: (mouse.column, mouse.row),
                clicked_chat_row: chat_row,
                cursor: 0,
                items,
            });
            return;
        }

        // Wheel: scroll the chat history. Wheel also clears any
        // active selection because the selection coords refer to
        // specific terminal rows, and a scroll changes what's at
        // each row.
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.mouse_in_chat_area(&mouse) {
                    self.selection = None;
                    self.scroll_chat_up(3);
                }
                return;
            }
            MouseEventKind::ScrollDown => {
                if self.mouse_in_chat_area(&mouse) {
                    self.selection = None;
                    self.scroll_chat_down(3);
                }
                return;
            }
            _ => {}
        }

        // Drag extends an in-flight selection. We only follow Left
        // drags; other button drags are ignored.
        if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
            let clamped = self.clamp_to_chat_area(mouse.column, mouse.row);
            if let Some(sel) = self.selection.as_mut()
                && sel.active
            {
                sel.focus = clamped;
            }
            return;
        }

        // Release finalizes the selection. It persists in
        // `self.selection` until cleared (Esc, new click outside chat,
        // wheel scroll).
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
            if let Some(sel) = self.selection.as_mut() {
                sel.active = false;
            }
            return;
        }

        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return;
        }

        // Composer first: clicks here position the cursor in the
        // input buffer (T8.d). The input rect is the *outer* rect
        // including the block border; we re-derive the inner rect
        // (1-cell border on each side, top border absent when the
        // queue is above) for hit-testing.
        if let Some(area) = self.input_area
            && let Some((line, col)) = self.composer_cursor_target_for_click(area, &mouse)
        {
            // Clicking into the composer dismisses any chat
            // selection — the user has switched contexts.
            self.selection = None;
            self.composer.set_cursor_from_line_col(line, col);
            // If the user is in Normal mode, drop into Insert — clicking
            // to place the cursor implies they're about to type there.
            if self.composer.vim_enabled() && matches!(self.composer.vim_mode(), VimMode::Normal) {
                self.composer.set_vim_mode(VimMode::Insert);
            }
            return;
        }

        let Some(area) = self.chat_area else {
            self.selection = None;
            return;
        };
        // crossterm reports row/column as 0-indexed absolute terminal
        // coordinates. Translate to chat-area relative.
        if mouse.row < area.y || mouse.row >= area.y + area.height {
            self.selection = None;
            return;
        }
        if mouse.column < area.x || mouse.column >= area.x + area.width {
            self.selection = None;
            return;
        }
        let rel = (mouse.row - area.y) as usize;
        // Chip click wins over drag-select start: chip rows have a
        // single owning entry whose `expanded` flag we toggle.
        if let Some(Some(entry_idx)) = self.clickable_rows.get(rel).copied() {
            self.selection = None;
            if let Some(HistoryEntry::Agent { expanded, .. }) = self.history.get_mut(entry_idx) {
                *expanded = !*expanded;
            }
            return;
        }
        // Non-chip chat row + left-down: start a fresh drag-select.
        // Anchor = focus = click point; mouse-drag will extend the
        // focus from here.
        self.selection = Some(Selection {
            anchor: (mouse.column, mouse.row),
            focus: (mouse.column, mouse.row),
            active: true,
        });
    }

    /// Clamp `(col, row)` into the current chat area. Used while
    /// dragging — if the user drags past the edge of the pane we
    /// pin the focus to the nearest edge cell instead of dropping
    /// the event.
    fn clamp_to_chat_area(&self, col: u16, row: u16) -> (u16, u16) {
        let Some(area) = self.chat_area else {
            return (col, row);
        };
        let clamped_col = col.max(area.x).min(area.x + area.width.saturating_sub(1));
        let clamped_row = row.max(area.y).min(area.y + area.height.saturating_sub(1));
        (clamped_col, clamped_row)
    }

    /// True when the mouse position is inside the chat area's last-
    /// rendered rect. Returns false when the chat area hasn't been
    /// rendered yet (e.g. a dialog is open).
    fn mouse_in_chat_area(&self, mouse: &MouseEvent) -> bool {
        let Some(area) = self.chat_area else {
            return false;
        };
        mouse.row >= area.y
            && mouse.row < area.y + area.height
            && mouse.column >= area.x
            && mouse.column < area.x + area.width
    }

    /// Scroll the chat history up (further back in time) by `n`
    /// logical lines. Clamped to `chat_total_lines - chat_visible_lines`
    /// so the top of the buffer can sit at the top of the pane but
    /// no further.
    fn scroll_chat_up(&mut self, n: usize) {
        let max_offset = self
            .chat_total_lines
            .saturating_sub(self.chat_visible_lines);
        self.chat_scroll_offset = (self.chat_scroll_offset + n).min(max_offset);
    }

    /// Scroll the chat history down (toward the live tail) by `n`
    /// logical lines. Saturates at 0 (pinned to bottom = live).
    fn scroll_chat_down(&mut self, n: usize) {
        self.chat_scroll_offset = self.chat_scroll_offset.saturating_sub(n);
    }

    /// Translate an absolute mouse position into a `(line, col)` in
    /// the composer's text buffer, or `None` if the click landed
    /// outside the input area. The inner-rect calculation mirrors
    /// the render path: a 1-cell border on left/right, and a 1-cell
    /// border on top *unless* the queue strip is above, in which
    /// case its bottom row is our top border (no top border of our
    /// own). Continuation lines render with `prefix_width` spaces
    /// of indent so the click-to-col math is uniform across lines.
    fn composer_cursor_target_for_click(
        &self,
        outer: Rect,
        mouse: &MouseEvent,
    ) -> Option<(usize, usize)> {
        if mouse.row < outer.y || mouse.row >= outer.y + outer.height {
            return None;
        }
        if mouse.column < outer.x || mouse.column >= outer.x + outer.width {
            return None;
        }
        let queue_above = !self.queue.is_empty();
        let top_border: u16 = if queue_above { 0 } else { 1 };
        let bottom_border: u16 = 1;
        let inner_top = outer.y.saturating_add(top_border);
        let inner_bottom = outer.y + outer.height.saturating_sub(bottom_border);
        let inner_left = outer.x.saturating_add(1);
        let inner_right = outer.x + outer.width.saturating_sub(1);
        if mouse.row < inner_top || mouse.row >= inner_bottom {
            return None;
        }
        if mouse.column < inner_left || mouse.column >= inner_right {
            return None;
        }
        let row_rel = (mouse.row - inner_top) as usize;
        let prefix_w = input_prefix_width();
        // Every visible row (first or continuation) has the prefix /
        // indent at the left edge of the inner rect.
        let col_rel = (mouse.column - inner_left) as usize;
        let buffer_col = col_rel.saturating_sub(prefix_w);
        Some((row_rel, buffer_col))
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
            "mouse" => {
                self.toggle_mouse_capture_inline();
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
        self.exit_tail_lines = tui_cfg.exit_tail_lines;
        self.rich_text_copy = tui_cfg.rich_text_copy;
        // Note: mouse_capture is *not* synced here. The live terminal
        // state is reconciled via the dialog's pending-flag drain
        // (see sync_mouse_capture_from_dialog) so we don't reapply
        // EnableMouseCapture on every reload — only when the user
        // actually toggled the setting.
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
    fn render_queue(&self, frame: &mut ratatui::Frame, area: Rect) {
        if area.height < 2 || area.width < 5 || self.queue.is_empty() {
            return;
        }
        // Border tracks the input box: dark grey while an agent turn
        // is in flight (matches the "agent is working, hold off" cue
        // on the input border), white when idle. Indexed(238) — same
        // shade the input uses.
        let border_color = if self.pending.is_some() {
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

    fn render_history(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.chat_area = Some(area);
        let area_h = area.height as usize;

        let mut all: Vec<Line<'static>> = Vec::new();
        // `targets[i]` carries the history-entry index whose thinking
        // chip occupies row `i` of `all`, or `None` otherwise. Only
        // the chip row toggles on click — body rows stay open for
        // drag-select.
        let mut targets: Vec<Option<usize>> = Vec::new();
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
            );
            let chip_abs = chip_row.map(|cr| all.len() + cr);
            for i in 0..lines.len() {
                targets.push(if Some(all.len() + i) == chip_abs {
                    Some(idx)
                } else {
                    None
                });
            }
            // Each entry's renderer returns one bool per emitted line;
            // pad if there's any mismatch (defensive — shouldn't
            // happen but keeps the parallel arrays in lockstep).
            let mut entry_conts = continuations;
            entry_conts.resize(lines.len(), false);
            conts.extend(entry_conts);
            all.extend(lines);
            // Insert a one-line gap after agent entries to separate from
            // the next user message or pending turn. Skip consecutive
            // agents so multi-turn blocks read as a single block.
            if matches!(entry, HistoryEntry::User { .. }) {
                all.push(Line::default());
                targets.push(None);
                conts.push(false);
            } else if matches!(entry, HistoryEntry::Agent { .. }) {
                let prev_is_agent = idx
                    .checked_sub(1)
                    .map(|i| matches!(self.history[i], HistoryEntry::Agent { .. }))
                    .unwrap_or(false);
                if !prev_is_agent {
                    all.push(Line::default());
                    targets.push(None);
                    conts.push(false);
                }
            }
        }
        if let Some(pending) = &self.pending {
            let dots = thinking_dots(self.started_at.elapsed().as_millis());
            let pending_lines = render_pending(pending, dots, area.width);
            for _ in 0..pending_lines.len() {
                targets.push(None);
                conts.push(false);
            }
            all.extend(pending_lines);
        }

        // Track totals so the mouse-wheel handler can clamp scrollback
        // to a valid range.
        self.chat_total_lines = all.len();
        self.chat_visible_lines = area_h;
        // Clamp the user's scroll offset to "can't scroll past the
        // top of the buffer". Max offset = total - visible (so the
        // top of the buffer can sit at the top of the pane).
        let max_offset = all.len().saturating_sub(area_h);
        if self.chat_scroll_offset > max_offset {
            self.chat_scroll_offset = max_offset;
        }

        // Bottom-align the visible window over `all`, then walk back by
        // `chat_scroll_offset` logical lines so the user can scroll up
        // into history with the wheel (T8.f wheel-scroll path).
        let (visible, visible_targets, visible_conts): (
            Vec<Line<'static>>,
            Vec<Option<usize>>,
            Vec<bool>,
        ) = if all.len() < area_h {
            let pad = area_h - all.len();
            let mut v: Vec<Line<'static>> = (0..pad).map(|_| Line::default()).collect();
            let mut t: Vec<Option<usize>> = (0..pad).map(|_| None).collect();
            let mut c: Vec<bool> = (0..pad).map(|_| false).collect();
            v.extend(all);
            t.extend(targets);
            c.extend(conts);
            (v, t, c)
        } else {
            let drop = all.len() - area_h - self.chat_scroll_offset;
            let v: Vec<Line<'static>> = all.into_iter().skip(drop).take(area_h).collect();
            let t: Vec<Option<usize>> = targets.into_iter().skip(drop).take(area_h).collect();
            let c: Vec<bool> = conts.into_iter().skip(drop).take(area_h).collect();
            (v, t, c)
        };
        self.clickable_rows = visible_targets;
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

    fn render_input(
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
        // Dark grey border while the agent loop is in flight; white
        // when we're idle. The same `pending` slot the renderer uses
        // to drive the "Thinking…" placeholder gates this, so the
        // border stays dim across reasoning, streaming, and tool
        // dispatch and flips back to white the moment the turn
        // finalizes. We use a darker grey than MUTED_COLOR_INDEX so
        // the "agent is working, hold off typing" signal reads at a
        // glance against the surrounding chrome.
        let border_color = if self.pending.is_some() {
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

/// True for keys that are pure-modifier (Shift, Ctrl, Alt, etc. being
/// pressed in isolation). Used to skip selection-clear so that
/// holding Shift to extend a future selection-by-keyboard motion
/// doesn't drop the in-flight selection on the press alone.
fn is_modifier_only(key: &KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Modifier(_) | KeyCode::CapsLock | KeyCode::NumLock | KeyCode::ScrollLock
    )
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
fn extract_selection_plaintext(
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
            let mut out: Vec<String> = vec![format!("[{ts}] you:")];
            for line in text.split('\n') {
                out.push(format!("  {line}"));
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
            let mut out: Vec<String> = vec![format!("[{ts}] {name}:")];
            if !reasoning.trim().is_empty() && *expanded {
                out.push("  thinking:".to_string());
                for raw in reasoning.lines() {
                    out.push(format!("    {raw}"));
                }
                out.push(String::new());
            }
            for line in text.split('\n') {
                out.push(format!("  {line}"));
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

/// True when `cursor` falls on the last line of `text` (no `\n` after
/// it). Used by history navigation: Down only steps history when the
/// cursor is at the bottom of the buffer; otherwise it moves the
/// composer cursor down a line.
fn cursor_on_last_line(text: &str, cursor: usize) -> bool {
    let after = &text[cursor.min(text.len())..];
    !after.contains('\n')
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
