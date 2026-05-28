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

mod input;
mod render;

use input::accepts_key;
use render::{extract_selection_plaintext, is_edit_tool};

use std::collections::HashMap;
use std::io::stdout;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;

use crate::config::dirs::discover_config_dirs;
use crate::config::extended::{DiffStyle, ExtendedConfig, ThinkingDisplay, VimModeSetting};
use crate::engine::TurnEvent;
use crate::git::{self, RepoStatus};
use crate::tui::agent_runner::{self, AgentRunner};
use crate::tui::composer::{Composer, VimMode, input_prefix_width};
use crate::tui::geometry::PaneGeometry;
use crate::tui::history::{
    HistoryEntry, MarkdownOpts, PendingMsg, ToolCall, ToolCallState, route_text_delta,
};
use crate::tui::settings::{self, Dialog};
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

/// Recompute a scroll-window top offset so `selected` stays visible with
/// a one-row margin (scrolloff=1) above and below — i.e. the next and
/// previous items are always shown — except at the true ends of the
/// list. Hard stops, no wrap. Shared by the `@`-popup and the model
/// picker so their scrolling feels identical.
pub(super) fn windowed_scroll(
    selected: usize,
    mut offset: usize,
    len: usize,
    window: usize,
) -> usize {
    if len <= window {
        return 0;
    }
    const SCROLLOFF: usize = 1;
    // Keep a margin above the selection.
    if selected < offset + SCROLLOFF {
        offset = selected.saturating_sub(SCROLLOFF);
    }
    // Keep a margin below the selection.
    if selected + SCROLLOFF + 1 > offset + window {
        offset = (selected + SCROLLOFF + 1).saturating_sub(window);
    }
    offset.min(len - window)
}

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

#[allow(private_interfaces)]
pub struct App {
    pub(super) launch: LaunchInfo,
    pub(super) composer: Composer,
    /// User's vim_mode setting (hint/enabled/disabled). Drives whether
    /// the Normal-mode hint chip is shown.
    pub(super) vim_setting: VimModeSetting,
    /// User's thinking-display setting. Drives whether the chip is shown
    /// and whether reasoning is rendered inline.
    pub(super) thinking_setting: ThinkingDisplay,
    /// User's markdown-rendering preferences. Threaded into each
    /// `render_entry` call so the renderer can pick the markdown path
    /// per kind of entry.
    pub(super) markdown_opts: MarkdownOpts,
    /// How `edit` / `editunlock` tool calls render in history
    /// (`tui.diff_style`). The narrow-terminal degradation from
    /// side-by-side → inline is per-render, computed from the
    /// rendered pane width.
    pub(super) diff_style: DiffStyle,
    /// `tui.use_emojis`. Threaded into the history renderers so tool-call
    /// boxes (and other glyphs) pick emoji vs. text-only labels.
    pub(super) use_emojis: bool,
    /// Cached args from `ToolStart` for edit tools that need them at
    /// `ToolEnd` time (to build the `Diff` history entry). Keyed by
    /// `call_id`; entries are popped at `ToolEnd`. Anything left
    /// behind (e.g. a tool that errored before emitting `ToolEnd`)
    /// gets cleaned up on the next `finalize_pending`.
    pub(super) pending_edit_args: HashMap<String, PendingEditArgs>,
    /// Messages typed and submitted while an agent turn is in flight.
    /// Mirrors the daemon's queue (GOALS §1c) for display; the daemon
    /// is the source of truth — these get cleared on `ThinkingStarted`
    /// because that event implies the daemon just drained the queue
    /// into the next inference round.
    pub(super) queue: Vec<String>,
    /// Submitted user messages (excluding queued ones). Used for Up/Down
    /// shell-style history navigation in the composer.
    pub(super) prompt_history: Vec<String>,
    /// Index into `prompt_history` for history navigation. `0` means
    /// "at the live buffer" (no history offset); `1` = most recent, etc.
    pub(super) prompt_history_cursor: usize,
    /// In-progress composer text saved when the user first pressed
    /// Up to enter history mode. Restored when they walk back past
    /// the newest entry (cursor going `1 → 0`). `None` when not in
    /// history mode or when entry happened from an empty composer.
    pub(super) staged_draft: Option<String>,
    pub(super) history: Vec<HistoryEntry>,
    /// In-flight assistant turn (between `ThinkingStarted` and the
    /// matching `AssistantText`/tool boundary). When `Some`, the
    /// renderer appends a live entry to the bottom of the history
    /// pane.
    pub(super) pending: Option<PendingMsg>,
    /// Reference point for the animated `Thinking…` dots. Set once at
    /// `App::new` time; the renderer derives the dot count from the
    /// elapsed time so the animation advances each tick.
    pub(super) started_at: Instant,
    /// Live git status; updated by a background tokio task spawned in
    /// `run`. The event loop syncs this into `launch.repo_status` once
    /// per tick.
    pub(super) repo_status: Arc<Mutex<Option<RepoStatus>>>,
    pub(super) dialog: Dialog,
    /// `/model` picker. Mutually exclusive with `dialog` (we never show
    /// both); kept separate so the picker doesn't clutter the settings
    /// state machine.
    pub(super) model_picker: Option<crate::tui::model_picker::ModelPickerDialog>,
    /// "Daemon not running" prompt shown at startup. Once the user picks,
    /// this is taken and the prompt closes.
    pub(super) daemon_prompt: Option<crate::tui::daemon_prompt::DaemonPromptDialog>,
    /// True after we've successfully connected to (or started) the daemon.
    pub(super) daemon_connected: bool,
    /// Lines emitted by an in-flight `/fetch-models` task. The event
    /// loop drains this each tick and appends to history.
    pub(super) fetch_models_progress: Arc<Mutex<Vec<String>>>,
    /// Lazily-initialized agent runner. None until the first user
    /// submit; populated by [`Self::ensure_agent_runner`]. Stored as
    /// `Result<AgentRunner, String>` so a failed init keeps the error
    /// around for next-time visibility.
    pub(super) agent_runner: Option<Result<AgentRunner, String>>,
    /// Last-rendered chat area `Rect`. Used to translate absolute
    /// terminal mouse coordinates into chat-relative coordinates so
    /// click-to-expand works on thinking blocks.
    pub(super) chat_area: Option<Rect>,
    /// Last-rendered composer-input `Rect` (the outer rect — block
    /// border included). Used by `handle_mouse` to route clicks into
    /// click-to-position-cursor (plan.md T8.d).
    pub(super) input_area: Option<Rect>,
    /// Logical-line scroll offset for the chat history pane. `0` =
    /// pinned to the bottom (live). Higher = scrolled further back in
    /// time. Bumped by mouse wheel when capture is on; clamped by
    /// `render_history` so we never scroll past the top.
    pub(super) chat_scroll_offset: usize,
    /// How tall (logical lines) the full chat content was at the last
    /// render. Updated each `render_history` and consulted by the
    /// mouse-wheel handler to clamp scroll-back to a valid maximum.
    pub(super) chat_total_lines: usize,
    /// How many logical lines fit in the chat pane at the last render.
    /// Same purpose — clamp scrollback so the bottom of the visible
    /// window can't go below the top of the content.
    pub(super) chat_visible_lines: usize,
    /// In-app drag-select state for chat content (plan.md T8.f). Set
    /// when the user mouse-downs in the chat area; updated on drag;
    /// committed on release. `Ctrl+Shift+C` copies the underlying
    /// plaintext via `clipboard::copy_plain` (OSC52 → SSH-safe).
    pub(super) selection: Option<Selection>,
    /// Snapshot of the chat area's rendered cells, one row per outer
    /// element, one cell per inner element. Each cell's `String` is
    /// the cell's `symbol()` — typically one char, but multi-byte for
    /// non-ASCII and an empty marker for the continuation cell of a
    /// wide glyph. Populated by `render_history` after the paragraph
    /// widget writes to the buffer. Used by the copy path so we don't
    /// have to redo ratatui's wrap math to extract the selected
    /// plaintext.
    pub(super) chat_text_grid: Vec<Vec<String>>,
    /// Parallel to `chat_text_grid`: `chat_cont_rows[i]` is `true`
    /// when visible row `i` is a soft-wrap continuation of the
    /// previous logical line. The copy path joins continuations with
    /// a space, real line boundaries with a newline — so pasted
    /// agent text reconstructs the original paragraphs rather than
    /// preserving the screen-level wraps.
    pub(super) chat_cont_rows: Vec<bool>,
    /// Click hit map: for each *visible* row in `chat_area`, the index
    /// (within `self.history`) of the agent entry whose thinking chip
    /// lives there — or `None` for non-clickable rows. Refreshed every
    /// render.
    pub(super) clickable_rows: Vec<Option<usize>>,
    /// Click/wheel hit map: for each *visible* chat row, the index
    /// (within `self.history`) of the `ToolBox` rendered there, or
    /// `None`. A wheel over a collapsed box scrolls the box; a click on
    /// any box row toggles its expansion. Refreshed every render.
    pub(super) box_rows: Vec<Option<usize>>,
    /// Last cursor-shape we asked the terminal to use. Tracked so we
    /// only re-issue the escape when the desired shape changes (most
    /// terminals tolerate redundant `SetCursorStyle` writes but a few
    /// blink visibly).
    pub(super) last_cursor_shape: Option<CursorShape>,
    /// Highlighted index in the `@`-popup. Reset to 0 whenever the
    /// composer's at-query changes; bumped by Up/Down while the popup
    /// is open.
    pub(super) at_selected: usize,
    /// Top visible index of the `@`-popup scroll window. Maintained with
    /// a 1-row scrolloff so the next/prev candidate is always visible
    /// except at the true ends of the list (see [`super::windowed_scroll`]).
    pub(super) at_scroll: usize,
    /// Per-query memo of the suggestion walk so the filesystem isn't
    /// re-walked on every render / arrow keypress. Keyed by the exact
    /// `@`-query string; recomputed when the query changes. `RefCell`
    /// because `at_suggestions` is called from `&self` render paths.
    pub(super) at_cache:
        std::cell::RefCell<Option<(String, Vec<crate::tui::file_tag::Suggestion>)>>,
    /// Accepted `@`-tag paths that contain a space / shell-special char.
    /// Tracked so the submit-time pass can wrap them in quotes (the
    /// composer shows them unquoted; the wire payload needs the quotes
    /// to disambiguate the path boundary). Content-matched at submit, so
    /// editing elsewhere in the buffer can't desync it; cleared on
    /// submit and on `/new`.
    pub(super) accepted_tags: Vec<String>,
    /// `@`-tag expansions from messages submitted while the agent was
    /// busy. Flushed into history as tool-call entries right after the
    /// folded user message appears (on the next `ThinkingStarted`), so
    /// they render in order with their message.
    pub(super) queued_tag_calls: Vec<crate::tui::file_tag::TagExpansion>,
    /// True once the user dismissed the `@`-popup with `Esc`. Stays
    /// suppressed until the active `@partial` token is dropped (e.g.
    /// whitespace appears after `@` or the `@` is deleted).
    pub(super) at_dismissed: bool,
    /// `/new` was invoked; the event loop services it on the next tick
    /// (needs the terminal handle for `insert_before` so the existing
    /// history spills to scrollback before the welcome header is
    /// reprinted above the viewport).
    pub(super) pending_new_session: bool,
    /// Provider-reported usage from the most recent round-trip.
    /// Preferred over the local tiktoken estimate in the context
    /// indicator; `None` until the first call returns.
    pub(super) last_usage: Option<crate::tokens::TokenUsage>,
    /// Ctrl+G was pressed — the event loop suspends ratatui, runs
    /// `$EDITOR` against the composer text, then reloads the file back
    /// into the composer.
    pub(super) pending_external_edit: bool,
    /// Whether crossterm mouse capture is currently enabled. Tracks the
    /// real terminal state so the settings toggle can push/pop the
    /// escape sequence without double-enabling. Sourced from
    /// `tui.mouse_capture` at startup; mutated when the user toggles
    /// the setting mid-session.
    pub(super) mouse_capture: bool,
    /// User's `tui.exit_tail_lines` setting (GOALS §1d). Cached at
    /// startup so the exit-tail dump survives the dialog being closed.
    pub(super) exit_tail_lines: i32,
    /// User's `tui.rich_text_copy` setting. Gates the `Ctrl+Shift+Y`
    /// keybind that copies the last agent message as HTML to the
    /// system clipboard (plan.md T8.g).
    pub(super) rich_text_copy: bool,
    /// Active right-click context menu in the chat area. Modal while
    /// `Some` — intercepts every key + mouse event.
    pub(super) context_menu: Option<crate::tui::context_menu::ContextMenu>,
    /// Transient FYI message overlaid on the status line
    /// (TUI-design-philosophy §7). 3-second TTL; dismissed early by
    /// any user interaction (keystroke or mouse click/wheel).
    pub(super) toast: Option<Toast>,
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

#[allow(private_interfaces)]
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
        let use_emojis = tui_cfg.use_emojis;
        let mut app = Self {
            launch,
            composer,
            vim_setting,
            thinking_setting,
            markdown_opts,
            diff_style,
            use_emojis,
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
            box_rows: Vec::new(),
            last_cursor_shape: None,
            at_selected: 0,
            at_scroll: 0,
            at_cache: std::cell::RefCell::new(None),
            accepted_tags: Vec::new(),
            queued_tag_calls: Vec::new(),
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
    pub(super) fn maybe_open_add_provider_wizard(&mut self) {
        if self.dialog.is_active() {
            return;
        }
        if !crate::tui::settings::Dialog::has_no_providers(&self.launch.cwd) {
            return;
        }
        self.dialog = crate::tui::settings::Dialog::open_providers_add(&self.launch.cwd);
    }

    pub(super) fn geometry(&self) -> PaneGeometry {
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
    pub(super) fn build_exit_tail_lines(&mut self) -> Vec<String> {
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

    pub(super) fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
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
    pub(super) fn show_toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast {
            text: text.into(),
            kind,
            expires_at: Instant::now() + TOAST_TTL,
        });
    }

    /// Drop the toast if it has expired. Called once per event-loop
    /// tick so a toast left untouched for 3 seconds cleans itself
    /// up without needing a new event to fire.
    pub(super) fn tick_toast(&mut self) {
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
    pub(super) fn toggle_mouse_capture_inline(&mut self) {
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
    pub(super) fn sync_mouse_capture_from_dialog(&mut self) {
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

    pub(super) fn drain_fetch_progress(&mut self) {
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

    pub(super) fn sync_repo_status(&mut self) {
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
    pub(super) fn maybe_service_new_session(
        &mut self,
        terminal: &mut DefaultTerminal,
    ) -> Result<()> {
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
        self.box_rows.clear();
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
    pub(super) fn maybe_service_external_edit(
        &mut self,
        terminal: &mut DefaultTerminal,
    ) -> Result<()> {
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

    pub(super) fn ensure_agent_runner(&mut self) {
        if self.agent_runner.is_some() {
            return;
        }
        self.agent_runner = Some(agent_runner::try_spawn(&self.launch.cwd));
    }

    /// Drain any [`TurnEvent`]s the engine has produced into the
    /// pending+history state machine. Runs each tick.
    pub(super) fn drain_agent_events(&mut self) {
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

    pub(super) fn apply_event(&mut self, event: TurnEvent) {
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
                    // Flush the @-tag tool-call entries for those queued
                    // messages so they render right under the folded turn.
                    if !self.queued_tag_calls.is_empty() {
                        let calls = std::mem::take(&mut self.queued_tag_calls);
                        self.push_tag_call_entries(&calls);
                    }
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
                // Edit tools render as a diff, which breaks the box. We
                // wait for ToolEnd to push the `Diff` entry once we have
                // the result.
                if is_edit_tool(&tool) {
                    if let Some(captured) = extract_edit_args(&args) {
                        self.pending_edit_args.insert(call_id, captured);
                        return;
                    }
                }
                let (summary, full_input) = tool_invocation(&tool, &args);
                // Write tools are conceptually diffs too — render them as
                // a standalone line that breaks the box (no diff body
                // until the engine surfaces pre-write content).
                if is_write_tool(&tool) {
                    self.history.push(HistoryEntry::ToolLine {
                        call_id,
                        tool,
                        summary,
                        state: ToolCallState::Processing,
                    });
                    return;
                }
                let call = ToolCall {
                    call_id,
                    tool,
                    summary,
                    full_input,
                    output: String::new(),
                    state: ToolCallState::Processing,
                };
                // Append to the open box (a run of consecutive boxable
                // calls), or start a new one. Anything non-boxable
                // pushed since the last box (agent text, a diff, a write,
                // a subagent) means `last` isn't a ToolBox, so the run
                // restarts here.
                if let Some(HistoryEntry::ToolBox {
                    calls,
                    view_offset,
                    follow,
                    ..
                }) = self.history.last_mut()
                {
                    calls.push(call);
                    *view_offset =
                        crate::tui::history::toolbox_top(calls.len(), *view_offset, *follow);
                } else {
                    self.history.push(HistoryEntry::ToolBox {
                        calls: vec![call],
                        view_offset: 0,
                        follow: true,
                        expanded: false,
                    });
                }
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
                self.update_tool_state(&call_id, ToolCallState::Success, Some((output, truncated)));
            }
            TurnEvent::ToolError {
                tool,
                error,
                call_id,
                kind,
                ..
            } => {
                // Drop any cached args from a paired ToolStart that never
                // produced a ToolEnd — the diff would be misleading on a
                // hard failure.
                self.pending_edit_args.remove(&call_id);
                // Bold red when the model built the call badly; plain red
                // when the tool failed for another reason.
                let state = match kind {
                    crate::engine::tool::ToolFailKind::Invocation => ToolCallState::BadCall,
                    crate::engine::tool::ToolFailKind::Execution => ToolCallState::Failed,
                };
                if !self.update_tool_state(&call_id, state, Some((error.clone(), false))) {
                    // No pending call to update (e.g. an edit/write tool
                    // whose entry we never created) — leave a standalone
                    // failed line so the error is still visible.
                    self.history.push(HistoryEntry::ToolLine {
                        call_id,
                        tool,
                        summary: agent_runner::first_line(&error, 200),
                        state,
                    });
                }
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
    /// Find the most-recent tool call with `call_id` — in a `ToolBox` or
    /// a standalone `ToolLine` — and update its state. For output-bearing
    /// box tools the output is stored as the expandable detail; input-
    /// only tools (read/readlock/unlock) drop it so a big file read
    /// doesn't sit in history. Returns whether a call was found.
    pub(super) fn update_tool_state(
        &mut self,
        call_id: &str,
        state: ToolCallState,
        output: Option<(String, bool)>,
    ) -> bool {
        for entry in self.history.iter_mut().rev() {
            match entry {
                HistoryEntry::ToolBox { calls, .. } => {
                    if let Some(call) = calls.iter_mut().rev().find(|c| c.call_id == call_id) {
                        call.state = state;
                        if let Some((out, truncated)) = output.as_ref()
                            && crate::tui::history::tool_shows_output(&call.tool)
                        {
                            call.output = if *truncated {
                                format!("{out}\n… (output truncated)")
                            } else {
                                out.clone()
                            };
                        }
                        return true;
                    }
                }
                HistoryEntry::ToolLine {
                    call_id: cid,
                    state: st,
                    ..
                } => {
                    if cid == call_id {
                        *st = state;
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    pub(super) fn finalize_pending(&mut self) {
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

    /// Execute one of the context-menu actions. Called both when the
    /// user clicks an item and when they hit Enter on a focused item.
    /// `clicked_chat_row` is the chat-relative row that was
    /// right-clicked — used by "Copy as rich text" to find which
    /// agent message was under the click; ignored by the other
    /// actions.
    pub(super) fn execute_context_menu_action(
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
    pub(super) fn agent_message_at_or_before(
        &self,
        clicked_chat_row: usize,
    ) -> Option<(String, String)> {
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
    pub(super) fn copy_selection_plaintext(&mut self) {
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
    pub(super) fn copy_last_agent_message_as_rich_text(&mut self) {
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
    pub(super) fn toggle_recent_reasoning(&mut self) {
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
    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
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
                if let Some(area) = self.chat_area
                    && self.mouse_in_chat_area(&mouse)
                {
                    self.selection = None;
                    // A collapsed tool box under the cursor captures the
                    // wheel until it hits its top; then the transcript
                    // scrolls.
                    let rel = (mouse.row - area.y) as usize;
                    if !self.scroll_box_at_row(rel, true) {
                        self.scroll_chat_up(3);
                    }
                }
                return;
            }
            MouseEventKind::ScrollDown => {
                if let Some(area) = self.chat_area
                    && self.mouse_in_chat_area(&mouse)
                {
                    self.selection = None;
                    let rel = (mouse.row - area.y) as usize;
                    if !self.scroll_box_at_row(rel, false) {
                        self.scroll_chat_down(3);
                    }
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
        // Click anywhere on a tool box toggles its expansion (per-block):
        // expanded shows every call in full (and disables the internal
        // scroll); collapsed returns to the windowed view.
        if self.box_rows.get(rel).copied().flatten().is_some() {
            self.selection = None;
            self.toggle_box_at_row(rel);
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
    pub(super) fn clamp_to_chat_area(&self, col: u16, row: u16) -> (u16, u16) {
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
    pub(super) fn mouse_in_chat_area(&self, mouse: &MouseEvent) -> bool {
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
    pub(super) fn scroll_chat_up(&mut self, n: usize) {
        let max_offset = self
            .chat_total_lines
            .saturating_sub(self.chat_visible_lines);
        self.chat_scroll_offset = (self.chat_scroll_offset + n).min(max_offset);
    }

    /// Scroll the chat history down (toward the live tail) by `n`
    /// logical lines. Saturates at 0 (pinned to bottom = live).
    pub(super) fn scroll_chat_down(&mut self, n: usize) {
        self.chat_scroll_offset = self.chat_scroll_offset.saturating_sub(n);
    }

    /// If a *collapsed* `ToolBox` sits under chat-relative row `rel`,
    /// advance its internal viewport by one call in `up`'s direction.
    /// Returns `true` if it consumed the wheel (the box moved); `false`
    /// to let the transcript scroll instead — so the box captures the
    /// wheel only between its top and its newest call. Scrolling up
    /// drops `follow`; scrolling back to the end restores it.
    pub(super) fn scroll_box_at_row(&mut self, rel: usize, up: bool) -> bool {
        let Some(Some(idx)) = self.box_rows.get(rel).copied() else {
            return false;
        };
        let Some(HistoryEntry::ToolBox {
            calls,
            view_offset,
            follow,
            expanded,
        }) = self.history.get_mut(idx)
        else {
            return false;
        };
        if *expanded {
            return false;
        }
        let n = calls.len();
        if n <= crate::tui::history::TOOLBOX_VISIBLE {
            return false;
        }
        let max_offset = n - crate::tui::history::TOOLBOX_VISIBLE;
        let cur = if *follow {
            max_offset
        } else {
            (*view_offset).min(max_offset)
        };
        if up {
            if cur == 0 {
                return false;
            }
            *follow = false;
            *view_offset = cur - 1;
            true
        } else {
            if *follow {
                return false;
            }
            let next = cur + 1;
            if next >= max_offset {
                *view_offset = max_offset;
                *follow = true;
            } else {
                *view_offset = next;
            }
            true
        }
    }

    /// Toggle the expansion of the `ToolBox` under chat-relative row
    /// `rel`. Collapsing resumes `follow` so the newest calls show.
    /// Returns whether a box was toggled.
    pub(super) fn toggle_box_at_row(&mut self, rel: usize) -> bool {
        let Some(Some(idx)) = self.box_rows.get(rel).copied() else {
            return false;
        };
        if let Some(HistoryEntry::ToolBox {
            expanded, follow, ..
        }) = self.history.get_mut(idx)
        {
            *expanded = !*expanded;
            if !*expanded {
                *follow = true;
            }
            return true;
        }
        false
    }

    /// Translate an absolute mouse position into a `(line, col)` in
    /// the composer's text buffer, or `None` if the click landed
    /// outside the input area. The inner-rect calculation mirrors
    /// the render path: a 1-cell border on left/right, and a 1-cell
    /// border on top *unless* the queue strip is above, in which
    /// case its bottom row is our top border (no top border of our
    /// own). Continuation lines render with `prefix_width` spaces
    /// of indent so the click-to-col math is uniform across lines.
    pub(super) fn composer_cursor_target_for_click(
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
    pub(super) fn sync_cursor_shape(&mut self) {
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

    pub(super) fn sync_active_agent(&mut self) {
        let Some(Ok(runner)) = self.agent_runner.as_ref() else {
            return;
        };
        let name = runner.active_agent.lock().unwrap().clone();
        if name != self.launch.agent_name {
            self.launch.agent_name = name;
        }
    }

    pub(super) fn execute_slash(&mut self, cmd: SlashCommand) -> bool {
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
    pub(super) fn reload_launch_info(&mut self) {
        let mut fresh = welcome::load(Some(&self.launch.cwd));
        // Don't clobber the live repo status — it's maintained by the
        // background poller and is fresher than a re-read here.
        fresh.repo_status = self.launch.repo_status.clone();
        self.launch = fresh;
    }

    /// Re-read the TUI-side config (vim mode, thinking display,
    /// markdown rendering) so changes made via `/settings` take effect
    /// immediately on dialog close.
    pub(super) fn reload_tui_config(&mut self) {
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
        self.use_emojis = tui_cfg.use_emojis;
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
    pub(super) fn spawn_fetch_models(&mut self) {
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
}

/// Pull `(path, old, new)` out of an edit tool's args. Returns
/// `None` when any field is missing; the caller falls back to the
/// generic Plain rendering in that case.
/// True for write tools rendered as a standalone line (they'd be diffs,
/// but the engine doesn't surface pre-write content yet — see
/// [`crate::tui::diff`]).
fn is_write_tool(tool: &str) -> bool {
    matches!(tool, "write" | "writeunlock")
}

/// `(collapsed_summary, full_input)` for a tool call. The summary is a
/// single line (path, first line of a command, URL); `full_input` is the
/// complete invocation text shown when a box is expanded.
fn tool_invocation(tool: &str, args: &serde_json::Value) -> (String, String) {
    let field = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
    match tool {
        "bash" => {
            let cmd = field("command").unwrap_or_default();
            let first = cmd.lines().next().unwrap_or("").to_string();
            let summary = if cmd.contains('\n') {
                format!("{first} …")
            } else {
                first
            };
            (summary, cmd)
        }
        "read" | "readlock" | "unlock" | "write" | "writeunlock" | "edit" | "editunlock" => {
            let p = field("path").unwrap_or_else(|| agent_runner::short_args(args));
            (p.clone(), p)
        }
        "webfetch" => {
            let u = field("url").unwrap_or_else(|| agent_runner::short_args(args));
            (u.clone(), u)
        }
        _ => {
            let s = agent_runner::short_args(args);
            (s.clone(), s)
        }
    }
}

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
        HistoryEntry::ToolLine { tool, summary, .. } => {
            let (_, label) = crate::tui::history::tool_glyph_label(tool, false);
            vec![format!("  {label}: {summary}")]
        }
        HistoryEntry::ToolBox { calls, .. } => calls
            .iter()
            .map(|c| {
                let (_, label) = crate::tui::history::tool_glyph_label(&c.tool, false);
                format!("  {label}: {}", c.summary)
            })
            .collect(),
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

#[allow(private_interfaces)]
pub(super) fn slash_matches(query: &str) -> Vec<&'static SlashCommand> {
    SLASH_COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(query))
        .collect()
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

#[cfg(test)]
mod windowed_scroll_tests {
    use super::windowed_scroll;

    const W: usize = 6;

    #[test]
    fn no_scroll_when_list_fits() {
        assert_eq!(windowed_scroll(0, 0, 5, W), 0);
        assert_eq!(windowed_scroll(4, 0, 5, W), 0);
    }

    #[test]
    fn top_has_no_margin_at_index_zero() {
        // n=10: selecting 0 keeps offset 0 (nothing above to show).
        assert_eq!(windowed_scroll(0, 0, 10, W), 0);
        // selecting 1 still shows index 0 above it.
        assert_eq!(windowed_scroll(1, 0, 10, W), 0);
    }

    #[test]
    fn scrolls_when_reaching_last_visible_row() {
        // From offset 0 (rows 0..5 visible), moving to index 5 must
        // scroll one so index 6 (the next item) is visible.
        assert_eq!(windowed_scroll(5, 0, 10, W), 1);
    }

    #[test]
    fn end_of_list_fills_last_window_without_bottom_margin() {
        // Last index of a 10-item list with window 6 → offset 4 so the
        // final six (4..10) show, selection on the bottom row.
        assert_eq!(windowed_scroll(9, 4, 10, W), 4);
    }

    #[test]
    fn moving_up_keeps_previous_item_visible() {
        // Coming back up to index 4 from a scrolled offset keeps a row
        // above visible.
        assert_eq!(windowed_scroll(4, 4, 10, W), 3);
    }
}
