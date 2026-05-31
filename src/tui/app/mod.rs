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

use anyhow::{Context, Result};
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
    HistoryEntry, MarkdownOpts, PendingMsg, SubagentOutcome, ToolCall, ToolCallState,
    route_text_delta,
};
use crate::tui::settings::{self, Dialog};
use crate::welcome::{self, LaunchInfo};

const MIN_INPUT_CONTENT: u16 = 1;
const MAX_INPUT_CONTENT: u16 = 6;
const INPUT_BORDER: u16 = 2;
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const EVENT_TICK: Duration = Duration::from_millis(100);

/// Double-press window for ctrl+c (GOALS §3a). A single ctrl+c interrupts
/// the running agent (never quits); a second press within this window of
/// the previous press exits the TUI. Sliding from the last press, so a
/// steady stream of slow presses interrupts repeatedly and never exits.
pub(super) const CTRL_C_EXIT_WINDOW: Duration = Duration::from_millis(500);

/// What a ctrl+c press should do, decided purely from the prior-press
/// time, the agent's busy state, and the configured window. Factored out
/// of [`App`] so the state machine is unit-testable without a live
/// terminal or daemon. See [`decide_ctrl_c`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CtrlCAction {
    /// Second press inside the window — exit the TUI now (regardless of
    /// agent state). During a run, this is the "interrupt AND exit" case:
    /// the first press already sent the interrupt.
    Exit,
    /// First press (or first after the window lapsed) while the agent is
    /// running — arm the exit window, show the hint, and interrupt the
    /// agent.
    ArmAndInterrupt,
    /// First press while the agent is idle — arm the exit window and show
    /// the hint only (nothing to interrupt).
    ArmOnly,
}

/// Pure double-press decision (GOALS §3a). `now` is a monotonic clock
/// reading; `armed_at` is the previous press time while the window is
/// live (`None` once it has lapsed); `agent_busy` is whether a turn is in
/// flight. Returns the action plus the new `armed_at` to store: `None`
/// when exiting (window is moot), `Some(now)` when arming/re-arming.
///
/// Rules:
/// - A press within `window` of `armed_at` → [`CtrlCAction::Exit`].
/// - Otherwise it's a fresh first press: re-arm at `now`, and interrupt
///   iff the agent is running.
pub(super) fn decide_ctrl_c(
    now: Instant,
    armed_at: Option<Instant>,
    window: Duration,
    agent_busy: bool,
) -> (CtrlCAction, Option<Instant>) {
    if let Some(prev) = armed_at
        && now.duration_since(prev) <= window
    {
        // Second press inside the window: exit regardless of agent state.
        return (CtrlCAction::Exit, None);
    }
    // Fresh first press (or the window lapsed): arm and, if busy, interrupt.
    let action = if agent_busy {
        CtrlCAction::ArmAndInterrupt
    } else {
        CtrlCAction::ArmOnly
    };
    (action, Some(now))
}

/// Pure gate for the eager display attach (session-id-shown-before-first-
/// message). Decides whether [`App::ensure_session_for_display`] should
/// attach a deferred session now so the welcome box can show its short id
/// before any message is sent. Factored out of [`App`] so the precedence is
/// unit-testable without a live daemon or terminal.
///
/// `probe_when` is the (costly) "is the canonical daemon reachable right
/// now?" check; it is invoked lazily — only when the cheap struct-only gates
/// all pass — so a tick that can't attach for any other reason never pays for
/// a socket probe.
///
/// All of these must hold:
/// - no runner exists yet (`!has_runner`) — a live runner already shows the
///   id, and a poisoned `Some(Err)` from a *first-message* attempt is left
///   alone (it was already surfaced to the user);
/// - the "daemon not running" prompt is closed (`!prompt_open`) — never spawn
///   a daemon out from under the user's pending choice;
/// - not daemonless (`!daemonless`) — eager-attaching there would spawn the
///   owned ephemeral daemon purely to display an id (a deliberate non-goal);
/// - we believe a daemon should be reachable (`daemon_connected`); and
/// - the canonical daemon actually answers right now (`probe_when()`) — so we
///   don't fire against the not-yet-bound socket in the "Start and connect"
///   startup gap.
fn should_attempt_display_attach(
    has_runner: bool,
    prompt_open: bool,
    daemonless: bool,
    daemon_connected: bool,
    probe_when: impl FnOnce() -> bool,
) -> bool {
    if has_runner || prompt_open || daemonless || !daemon_connected {
        return false;
    }
    probe_when()
}

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

/// A `/compact` handoff awaiting the user's review-then-commit. Held
/// while the assembled handoff sits in the composer; consumed when the
/// user submits (re-attach to `new_session_id` + send the edited
/// handoff) or discards.
#[derive(Clone)]
pub(super) struct PendingCompact {
    pub(super) new_session_id: uuid::Uuid,
    pub(super) seed_tool_count: usize,
    /// Approx wire tokens the seed-tools cost on the fresh session's
    /// first turn (from `CompactReady`). Surfaced in the boundary marker.
    pub(super) seed_tool_tokens: u64,
    /// The predecessor (current) session's short id, captured at
    /// `CompactReady` time so the fresh session can draw a `compacted
    /// from <short-id>` boundary marker once committed. Empty when the
    /// runner had no short id.
    pub(super) predecessor_short_id: String,
}

/// A `/init` whose target file already exists, awaiting the user's
/// update/overwrite/cancel choice in the (locally-driven) question
/// dialog. The dialog carries `interrupt_id`; the close handler matches
/// it so the local choice resolves here rather than going to the daemon.
pub(super) struct PendingInit {
    /// The synthetic interrupt id minted for the local choice dialog.
    pub(super) interrupt_id: uuid::Uuid,
    /// The target path to hand the agent (relative to cwd when under it).
    pub(super) display: String,
}

/// An open `/side` side conversation. Created when `/side` forks the main
/// session into an ephemeral throwaway and switches the TUI onto it; the
/// snapshot is everything needed to restore the **main** session exactly
/// where the user left off when the side conversation ends (`/side end`,
/// Esc, or process exit). Restoring re-binds the saved runner and view
/// verbatim — no re-attach, so no lost scrollback. While `Some`, the chrome
/// shows the side indicator and the ephemeral fork id is discarded on exit.
pub(super) struct SideConversation {
    /// The ephemeral fork's session id — the row to discard on exit.
    pub(super) side_session_id: uuid::Uuid,
    /// The daemon socket the side fork lives on (the same one the parent
    /// runner is attached to), so the discard RPC reaches the right daemon.
    pub(super) socket: std::path::PathBuf,
    /// Saved main-session view, restored on exit.
    saved_runner: Option<Result<AgentRunner, String>>,
    saved_history: Vec<HistoryEntry>,
    saved_queue: Vec<String>,
    saved_pending: Option<PendingMsg>,
    saved_prunable_tokens: u64,
    saved_cache_cold: bool,
    saved_elided_event_ids: std::collections::HashSet<String>,
    saved_active_jobs: std::collections::BTreeMap<String, ActiveJob>,
    saved_pending_stop_confirm: Option<Vec<String>>,
    saved_chat_scroll_offset: usize,
    saved_project_id: Option<String>,
    saved_session_id: Option<uuid::Uuid>,
    saved_session_short_id: Option<String>,
    saved_current_session_persisted: bool,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "caffeinate",
        description: "Keep the machine awake so agents survive a closed lid (arg: on/off/until-idle)",
    },
    SlashCommand {
        name: "build",
        description: "Switch the primary agent to Build (make changes)",
    },
    SlashCommand {
        name: "clear",
        description: "Clear the chat and start a fresh session (alias of /new)",
    },
    SlashCommand {
        name: "compact",
        description: "Compress the conversation to save context",
    },
    SlashCommand {
        name: "context",
        description: "Show a colored breakdown of how the context window is filled",
    },
    SlashCommand {
        name: "copy",
        description: "Copy the last response to the clipboard (arg: markdown/plain/rich)",
    },
    SlashCommand {
        name: "editor",
        description: "Open $EDITOR in an embedded pane (arg: left/right/top/bottom)",
    },
    SlashCommand {
        name: "exit",
        description: "Quit cockpit",
    },
    SlashCommand {
        name: "export",
        description: "Export the current conversation to .cockpit/exports/ (arg: debug for the full bundle)",
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
        name: "git",
        description: "Run a git command and share its output with the agent",
    },
    SlashCommand {
        name: "init",
        description: "Explore the project and write its instructions file (arg: target path)",
    },
    SlashCommand {
        name: "jobs",
        description: "List active async jobs (arg: cancel <job-id> to cancel one)",
    },
    SlashCommand {
        name: "lazygit",
        description: "Open lazygit in an embedded pane",
    },
    SlashCommand {
        name: "llm-mode",
        description: "Switch LLM steering mode (arg: toggle/defend/normal; bare = toggle)",
    },
    SlashCommand {
        name: "model",
        description: "Switch the active model",
    },
    SlashCommand {
        name: "model-settings",
        description: "Open the active model's context, cache, shrink, and mode settings",
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
        name: "permissions",
        description: "View and delete persisted command/path approvals across project and global scopes",
    },
    SlashCommand {
        name: "pin",
        description: "Pin a message so it survives /compact verbatim (arg: text)",
    },
    SlashCommand {
        name: "plan",
        description: "Switch the primary agent to Plan (author a plan)",
    },
    SlashCommand {
        name: "plans",
        description: "Browse plans and their step dependency graphs (read-only)",
    },
    SlashCommand {
        name: "prune",
        description: "Collapse superseded snapshot reads to reclaim context",
    },
    SlashCommand {
        name: "ps",
        description: "List this session's running async jobs",
    },
    SlashCommand {
        name: "rename",
        description: "Rename the current session (arg: title)",
    },
    SlashCommand {
        name: "resume",
        description: "Browse and resume previous sessions (alias of /sessions)",
    },
    SlashCommand {
        name: "sandbox",
        description: "Toggle filesystem sandboxing (arg: on/off)",
    },
    SlashCommand {
        name: "sessions",
        description: "Browse and resume previous sessions",
    },
    SlashCommand {
        name: "settings",
        description: "Open the settings dialog",
    },
    SlashCommand {
        name: "side",
        description: "Start a throwaway side conversation forked from here (`/side end` to discard)",
    },
    SlashCommand {
        name: "skills",
        description: "List every discovered skill in a read-only overlay",
    },
    SlashCommand {
        name: "stats",
        description: "On-device model and project performance (tokens, recovery, languages)",
    },
    SlashCommand {
        name: "stop",
        description: "Stop this session's async jobs (arg: job-id for one, bare for all)",
    },
];

impl SlashCommand {
    /// Whether the command should appear in the menu and accept a typed
    /// invocation. `/editor` needs `$EDITOR`; `/lazygit` needs `lazygit`
    /// on `PATH` (GOALS §1i/§1j). Everything else is always available.
    fn is_available(&self) -> bool {
        match self.name {
            "editor" => std::env::var_os("EDITOR").is_some(),
            "lazygit" => program_on_path("lazygit"),
            _ => true,
        }
    }

    /// The `/model-settings` command the hidden `/modelsettings` alias
    /// resolves to (`prompts/model-provider-settings.md`). Returns the
    /// registered command so the dispatch + usage tally match the visible
    /// form exactly.
    pub(super) fn model_settings_alias() -> SlashCommand {
        *SLASH_COMMANDS
            .iter()
            .find(|c| c.name == "model-settings")
            .expect("model-settings command registered")
    }
}

/// True when `prog` is found as a file on any `PATH` entry. On Windows
/// also probes `prog.exe`. Used to gate `/lazygit`.
fn program_on_path(prog: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{prog}.exe"), prog.to_string()]
    } else {
        vec![prog.to_string()]
    };
    std::env::split_paths(&paths).any(|dir| names.iter().any(|n| dir.join(n).is_file()))
}

/// Where an embedded pane (`/editor`, `/lazygit`) sits in the chat-body
/// region (GOALS §1i). `Full` fills the body; the others split it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneSide {
    Full,
    Left,
    Right,
    Top,
    Bottom,
}

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
    /// True while the agent is actively working on the user's turn —
    /// from a fresh submit (rising edge) until the daemon's `AgentIdle`
    /// (falling edge). Unlike `pending.is_some()` this stays set across
    /// tool execution and inter-round gaps, so it's the signal the
    /// working indicator and the grey input border track.
    pub(super) busy: bool,
    /// Start of the cumulative "span" clock — set on a fresh submit,
    /// re-set on the next fresh submit, never touched by a queued
    /// message folded into an in-flight turn. Drives the working
    /// indicator's elapsed readout. `None` before the first submit.
    pub(super) span_started_at: Option<Instant>,
    /// Index into [`WORKING_MESSAGES`] held for the current span. Re-
    /// rolled on each fresh submit, avoiding the immediately previous
    /// pick. Initialized one-past-the-end so the first roll may land on
    /// any message (including index 0).
    pub(super) working_msg_idx: usize,
    /// Set to the 1-based retry number while an inference call is mid
    /// network-retry (`Reconnecting` event); the working indicator shows
    /// `reconnecting… attempt N` instead of the usual working line.
    /// Cleared on the next `ThinkingStarted` / `AssistantTextDelta` /
    /// `AgentIdle` (the call resumed, produced output, or the turn ended).
    pub(super) reconnect_attempt: Option<u32>,
    /// Live git status; updated by a background tokio task spawned in
    /// `run`. The event loop syncs this into `launch.repo_status` once
    /// per tick.
    pub(super) repo_status: Arc<Mutex<Option<RepoStatus>>>,
    pub(super) dialog: Dialog,
    /// `/model` picker. Mutually exclusive with `dialog` (we never show
    /// both); kept separate so the picker doesn't clutter the settings
    /// state machine.
    pub(super) model_picker: Option<crate::tui::model_picker::ModelPickerDialog>,
    /// `/stats` pane (GOALS §15). A full-body interactive overlay over
    /// the part-1 roll-up layer; `None` when closed. Routed input/render
    /// alongside `dialog` / `model_picker`.
    pub(super) stats_pane: Option<crate::tui::stats_pane::StatsPane>,
    /// `/sessions` + `/resume` browser pane (GOALS §17f). A full-body
    /// overlay; `None` when closed. Routed input/render alongside
    /// `stats_pane`. Enter resumes the highlighted session via the
    /// existing `attach_to_session` path.
    pub(super) sessions_pane: Option<crate::tui::sessions_pane::SessionsPane>,
    /// `/skills` pane — a read-only overlay listing every discovered
    /// skill (name + description + source). `None` when closed. Routed
    /// input/render alongside `stats_pane` / `sessions_pane`.
    pub(super) skills_pane: Option<crate::tui::skills_pane::SkillsPane>,
    /// `/plans` browser pane — a read-only, two-level overlay (plan list →
    /// step DAG). `None` when closed. Routed input/render alongside the
    /// other panes; reads plans from the daemon via `ListPlans` /
    /// `PlanDetail`.
    pub(super) plans_pane: Option<crate::tui::plans_pane::PlansPane>,
    /// `/permissions` pane — view + delete persisted command/path/loop
    /// approvals across the project and global file scopes. `None` when
    /// closed. Routed input/render alongside the other panes; the one
    /// mutating action (delete) rewrites the backing `approvals.json` via
    /// the approval store's load→mutate→atomic-store path.
    pub(super) permissions_pane: Option<crate::tui::permissions_pane::PermissionsPane>,
    /// `/context` overlay — a read-only, dismissable snapshot of the live
    /// context-window composition (colored per-category bar + legend).
    /// `None` when closed. Routed input/render alongside the other panes;
    /// the snapshot is captured once at open (not live-updating).
    pub(super) context_pane: Option<crate::tui::context_pane::ContextPane>,
    /// "Daemon not running" prompt shown at startup. Once the user picks,
    /// this is taken and the prompt closes.
    pub(super) daemon_prompt: Option<crate::tui::daemon_prompt::DaemonPromptDialog>,
    /// Answering dialog for a `question`-tool interrupt (GOALS §3b).
    /// Opened from `TurnEvent::InterruptRaised`, replaces the composer,
    /// and on submit/cancel sends `ResolveInterrupt` back to the daemon.
    /// `None` when no question is pending.
    pub(super) question_dialog: Option<crate::tui::dialog::question::QuestionDialog>,
    /// In-flight `/init` awaiting the user's update/overwrite/cancel
    /// choice. Set when the target file already exists; the question
    /// dialog open at that moment is this local prompt (not a daemon
    /// interrupt), so its close resolves here instead of going back to the
    /// daemon. `None` whenever no `/init` choice is pending.
    pub(super) pending_init: Option<PendingInit>,
    /// True after we've successfully connected to (or started) the daemon.
    pub(super) daemon_connected: bool,
    /// Daemonless mode (`DaemonChoice::ContinueWithout`): this TUI owns its
    /// own per-pid *ephemeral* daemon, fully isolated from the canonical
    /// persistent daemon and from any other TUI's ephemeral daemon. Set when
    /// the user picks "Continue without daemon" at the launch prompt; it
    /// flips the agent-runner lifecycle to `AlwaysEphemeral` so we spawn (and
    /// own) a fresh daemon rather than auto-promoting the canonical one.
    pub(super) daemonless: bool,
    /// RAII guard that reaps the owned ephemeral daemon on every exit path
    /// (clean quit, error, panic/unwind, SIGINT/SIGTERM) — the same
    /// ownership contract `cockpit run` uses. `Some` only in daemonless mode
    /// once the runner has spawned the owned daemon; `None` when attached to
    /// a daemon we don't own.
    pub(super) daemon_guard: Option<crate::daemon::ephemeral_guard::EphemeralDaemonGuard>,
    /// Signal task that fires the guard's shutdown on SIGINT/SIGTERM. Held so
    /// it can be aborted once the happy-path teardown has run.
    pub(super) daemon_signal_task: Option<tokio::task::JoinHandle<()>>,
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
    /// Registry of condensed-text / image paste blocks currently in the
    /// composer buffer (composer-paste-handling). Kept byte-range-synced
    /// with [`Self::composer`] across every edit; consumed at submit to
    /// inline text + emit real image parts (vision) or text notes
    /// (non-vision). Cleared on submit and `/new`.
    pub(super) paste_registry: crate::tui::paste::PasteRegistry,
    /// `@`-tag expansions from messages submitted while the agent was
    /// busy. Flushed into history as tool-call entries right after the
    /// folded user message appears (on the next `ThinkingStarted`), so
    /// they render in order with their message.
    pub(super) queued_tag_calls: Vec<crate::tui::file_tag::TagExpansion>,
    /// True once the user dismissed the `@`-popup with `Esc`. Stays
    /// suppressed until the active `@partial` token is dropped (e.g.
    /// whitespace appears after `@` or the `@` is deleted).
    pub(super) at_dismissed: bool,
    /// Highlighted index in the slash-command popup. Reset to 0 (the
    /// frequency-ranked top match) whenever the slash query changes;
    /// moved by Up/Down while the popup is open. While the popup shows,
    /// Up/Down drive this cursor instead of composer history recall.
    pub(super) slash_selected: usize,
    /// Top visible index of the slash popup's scroll window, maintained
    /// with the same 1-row scrolloff as the `@`-popup (see
    /// [`super::windowed_scroll`]). Reset alongside `slash_selected`.
    pub(super) slash_scroll: usize,
    /// `/new` was invoked; the event loop services it on the next tick
    /// (needs the terminal handle for `insert_before` so the existing
    /// history spills to scrollback before the welcome header is
    /// reprinted above the viewport).
    pub(super) pending_new_session: bool,
    /// Provider-reported usage from the most recent round-trip. Anchors
    /// the live context counter (see `context_tokens`): the displayed
    /// value is this total plus a local estimate of everything streamed
    /// since it arrived. `None` until the first call returns.
    pub(super) last_usage: Option<crate::tokens::TokenUsage>,
    /// Local cl100k_base estimate captured the instant `last_usage` was
    /// set — the baseline the live counter measures streamed tokens
    /// against, so the number climbs per token and re-snaps to the
    /// provider's exact count on the next report.
    pub(super) estimate_at_last_usage: u32,
    /// Memoized `(length-signature, token count)` for the finalized
    /// history portion of the context estimate. History is static while
    /// a turn streams, so the per-frame live counter only re-tokenizes
    /// the growing `pending` buffer instead of the whole transcript.
    /// `Cell` because the estimate runs from `&self` render paths.
    pub(super) history_estimate_cache: std::cell::Cell<Option<(u64, u32)>>,
    /// 30-day autocomplete frequency counts, used as a tie-breaker in
    /// the slash / model / @-tag surfaces. Seeded from the daemon at
    /// attach and incremented optimistically on each local pick. `tags`
    /// is scoped to the attached project. Empty until the first attach
    /// (sorts fall back to their existing alphabetical/declaration
    /// order until then).
    pub(super) usage_models: HashMap<String, u64>,
    pub(super) usage_slash: HashMap<String, u64>,
    pub(super) usage_tags: HashMap<String, u64>,
    /// The attached session's project id — the scope for `tag` records.
    /// `None` until the first attach.
    pub(super) project_id: Option<String>,
    /// Whether the *currently bound* session has been persisted to the DB
    /// (session-id-display-and-lazy-persist). The daemon writes the
    /// `sessions` row on the first user message, so this flips `true` the
    /// instant a submission is accepted by the runner, and resets to `false`
    /// whenever the runner is rebound (`/new`, `/resume`, `/compact`) since
    /// those open or switch to a different session. Read on exit to decide
    /// whether to print the session id; a resumed session is persisted from
    /// the start, so its rebind sets this `true`.
    pub(super) current_session_persisted: bool,
    /// Fresh-chat sizing for this project, resolved at launch: the
    /// guidance-file basename + body tokens (the `X tokens in <file>`
    /// label) and the full composed system prompt tokens (the baseline
    /// the running context estimate folds in). Calibrated when a daemon
    /// is running, raw cl100k otherwise. `None` only before the launch
    /// fetch has run.
    pub(super) guidance_estimate: Option<agent_runner::GuidanceEstimate>,
    /// Wire tokens `/prune` would drop from the foreground agent right
    /// now (GOALS §1a). Pushed by the daemon's `ContextProjection` event
    /// — the authoritative figure from the same `dedup_plan` `/prune`
    /// executes, so the status-line `→ Y% prunable` always matches what
    /// `/prune` removes. `0` until the first projection arrives.
    pub(super) prunable_tokens: u64,
    /// Whether the provider cache is expected cold on the next call (from
    /// the daemon's cache-cold predicate). Drives the `/prune` confirm's
    /// hot-vs-cold warning. Defaults true (no warm cache to lose).
    pub(super) cache_cold: bool,
    /// The active LLM-strength mode (`prompts/llm-modes-defensive-normal.md`).
    /// Resolved from the layered config at launch and tracked live off the
    /// daemon's `LlmModeChanged` event so the `/llm-mode` toggle + cache-break
    /// warning resolve against the authoritative current value.
    pub(super) llm_mode: crate::config::extended::LlmMode,
    /// The live set of wire-side elided tool-result `call_id`s on the
    /// foreground agent (from the daemon's `Pruned` event). The scrollback
    /// renderer dims any boxed tool call whose `call_id` is in here —
    /// full text stays visible (GOALS §14). A render-time view of live
    /// prune state, replaced wholesale on each `Pruned`, not a persisted
    /// flag. Cleared on a fresh thread (`/compact` commit, `/clear`).
    pub(super) elided_event_ids: std::collections::HashSet<String>,
    /// A `/compact` handoff awaiting review-then-commit (T6.e). `Some`
    /// while the assembled handoff sits in the composer for editing.
    pub(super) pending_compact: Option<PendingCompact>,
    /// `/prune` confirm armed: the user ran `/prune`, saw the before→after
    /// numbers + cache warning, and the next `y`/Enter commits (anything
    /// else cancels). `Some` holds nothing meaningful — its presence is
    /// the armed flag; the numbers were already pushed to history.
    pub(super) pending_prune_confirm: bool,
    /// Bare `/stop` confirm armed: the user ran `/stop` with no id, saw
    /// the `Stop N job(s) in this session? [y/N]` prompt, and the next
    /// `y` commits (anything else cancels). Carries the current-session
    /// job ids captured at arm time so the cancel set can't drift between
    /// the prompt and the confirmation.
    pub(super) pending_stop_confirm: Option<Vec<String>>,
    /// `RecordUsage` requests made before the daemon runner exists.
    /// Flushed (with tag project ids backfilled) once it's created.
    pub(super) pending_usage: Vec<crate::daemon::proto::Request>,
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
    /// Live embedded `$EDITOR` / `lazygit` pane (GOALS §1i/§1j). One at
    /// a time; `None` when no pane is open. Auto-closes when the child
    /// exits, serviced once per event-loop tick.
    pub(super) pane: Option<crate::tui::pty::PtyPane>,
    /// Where the open pane sits in the chat-body region.
    pub(super) pane_side: PaneSide,
    /// Pane's share of the body in a split (0.0–1.0), persisted for the
    /// session. Ignored when `pane_side` is `Full`.
    pub(super) pane_ratio: f32,
    /// True when keyboard/mouse route to the pane; false when they go to
    /// the composer. Toggled by `Ctrl+O` and by clicking a pane.
    pub(super) pane_focused: bool,
    /// Last-rendered pane content rect (absolute coords). Used for mouse
    /// hit-testing, PTY resize, and parking the real cursor.
    pub(super) pane_rect: Option<Rect>,
    /// Last-rendered split-divider rect, and whether it's a vertical
    /// rule (left/right split) vs. a horizontal one (top/bottom). Used
    /// to start a divider drag-resize. `None` in fullscreen.
    pub(super) divider: Option<(Rect, bool)>,
    /// Last-rendered body rect the split was computed from. Lets the
    /// mouse handler convert a divider drag into a new ratio without a
    /// frame.
    pub(super) pane_body: Option<Rect>,
    /// True while a left-drag that began on the divider is resizing the
    /// split.
    pub(super) dragging_divider: bool,
    /// Buffered `<git cmd="…">…</git>` blocks from `/git` (GOALS §1l),
    /// attached to the next user message's wire text and cleared on
    /// send (and on `/new`).
    pub(super) pending_git_blocks: Vec<String>,
    /// Live async jobs (GOALS §22), keyed by job id. Drives the transient
    /// jobs strip (rendered only when non-empty) and `/jobs`. Maintained
    /// from `JobStarted` / `JobNote` / `JobProgress` / `JobCompleted`
    /// events.
    pub(super) active_jobs: std::collections::BTreeMap<String, ActiveJob>,
    /// Monotonic timestamp of the most recent ctrl+c press, while the
    /// double-press exit window is armed. A single ctrl+c interrupts a
    /// running agent (never quits); a second press within
    /// [`CTRL_C_EXIT_WINDOW`] of the previous one exits the TUI. `None`
    /// when the window has lapsed (the next press is a fresh first press).
    /// Uses `Instant` (monotonic) so a wall-clock jump can't mis-trigger.
    pub(super) ctrl_c_armed_at: Option<Instant>,
    /// The client's `--no-sandbox` flag (sandboxing part 2). Passed to
    /// the daemon at attach so sessions this TUI creates start with
    /// filesystem sandboxing OFF (unless the daemon itself was launched
    /// `--no-sandbox`, which wins). A `/sandbox` flip still overrides.
    pub(super) no_sandbox: bool,
    /// Daemon-broadcast caffeination state (`/caffeinate`). Drives the `☕`
    /// chrome glyph; set/cleared from the daemon-global `CaffeinateState`
    /// event so it stays in sync across all clients (incl. until-idle
    /// auto-off). Not client-owned: the assertion lives in the daemon.
    pub(super) caffeinate_active: bool,
    /// Daemon-broadcast plan-status counts for *this* project
    /// (`plan-status-chrome-and-resolver.md`). Drives the additive plan-status
    /// chrome slot; set from the daemon-global `PlanStatusState` event (only
    /// when its `project_id` matches this session's), so a reconnecting /
    /// late-opened TUI shows the correct state — not TUI-local bookkeeping.
    /// Default-zero (slot absent) until the first broadcast.
    pub(super) plan_status: crate::db::plans::PlanStatusCounts,
    /// An open `/side` side conversation, or `None` in the main session. While
    /// `Some`, the TUI is bound to an ephemeral throwaway fork: the chrome
    /// shows the side indicator, `/side end` and the empty-composer Esc exit
    /// it, and the fork is discarded on exit (or process death — see the run
    /// teardown and the daemon boot sweep).
    pub(super) side_conversation: Option<SideConversation>,
    /// Daemon is draining for a graceful shutdown
    /// (`daemon-graceful-drain-shutdown.md`). Set from the daemon-global
    /// `DaemonDraining` event. While set, the composer refuses new
    /// submissions with a short notice — new work is rejected, not queued.
    pub(super) daemon_draining: bool,
    /// Composer next-message prediction setting
    /// (`prompts/predict-next-message.md`). `off` short-circuits before
    /// any utility call; `short`/`long` bound the prediction.
    pub(super) predict_setting: crate::config::extended::PredictNextMessage,
    /// The next-message prediction lifecycle state (turn counter, cache,
    /// live ghost). Pure + unit-testable; see [`PredictionState`].
    pub(super) prediction_state: PredictionState,
    /// Async prediction-result slot. The spawned utility-model task writes
    /// `(turn, Option<bounded-text>)`; the event loop drains it each tick
    /// and adopts the text only when `turn` still matches the current turn
    /// and the box is empty (appear-once-ready, discard-if-stale).
    pub(super) prediction_result: PredictionResultSlot,
}

/// Shared slot a spawned prediction task posts its `(turn, bounded-text)`
/// result back through; drained by the event loop each tick.
pub(super) type PredictionResultSlot = Arc<Mutex<Option<(u64, Option<String>)>>>;

/// A completed composer next-message prediction
/// (`prompts/predict-next-message.md`), cached so a clear-to-empty within
/// the same turn restores the ghost without a new utility call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Prediction {
    /// Agent turn the prediction was generated for.
    pub(super) turn: u64,
    /// Bounded prediction text (mode-capped by `engine::predict`).
    pub(super) text: String,
    /// `true` when the active setting is `long` (enables the two-stage
    /// reveal for multi-line predictions).
    pub(super) long_mode: bool,
}

/// The next-message prediction lifecycle (`prompts/predict-next-message.md`),
/// kept pure so the eager-generate / hide-on-type / restore-on-clear /
/// stale-replacement behavior is unit-testable without an `App`.
///
/// `turn` is a monotonic agent-turn counter (bumped at each `AgentIdle` and
/// on `/new`); a prediction belongs to the turn it was generated for, so a
/// result tagged with an older turn is discarded rather than shown. `cached`
/// is the bounded prediction for the current turn (the restore-on-clear
/// cache); `ghost` is the live two-stage reveal state, present only while
/// the box is empty.
#[derive(Debug, Default)]
pub(super) struct PredictionState {
    /// Monotonic agent-turn counter.
    turn: u64,
    /// Cached prediction for the current turn (`None` until one lands).
    cached: Option<Prediction>,
    /// Live ghost shown while the box is empty.
    ghost: Option<crate::tui::composer::PredictionGhost>,
}

impl PredictionState {
    /// The current agent-turn id (the tag a freshly-spawned prediction
    /// carries).
    pub(super) fn turn(&self) -> u64 {
        self.turn
    }

    /// The live ghost, if any (read by the renderer + key handler).
    pub(super) fn ghost(&self) -> Option<&crate::tui::composer::PredictionGhost> {
        self.ghost.as_ref()
    }

    /// Mutable access to the live ghost (the Tab-accept path advances its
    /// stage).
    pub(super) fn ghost_mut(&mut self) -> Option<&mut crate::tui::composer::PredictionGhost> {
        self.ghost.as_mut()
    }

    /// A new agent turn ended (or `/new`): bump the turn id (invalidating
    /// any in-flight or cached prior-turn prediction) and drop the cache +
    /// ghost so a stale prediction never shows.
    pub(super) fn begin_turn(&mut self) {
        self.turn = self.turn.wrapping_add(1);
        self.cached = None;
        self.ghost = None;
    }

    /// Adopt a completed async result tagged with `result_turn`. Discards a
    /// stale result (older turn) or a `None` text. Caches a usable result
    /// and — only when `box_empty` (appear-once-ready, never over active
    /// input) — builds the ghost. `long_mode` enables the two-stage reveal.
    pub(super) fn on_result(
        &mut self,
        result_turn: u64,
        text: Option<String>,
        long_mode: bool,
        box_empty: bool,
    ) {
        if result_turn != self.turn {
            return; // stale: a newer turn started
        }
        let Some(text) = text else {
            return;
        };
        self.cached = Some(Prediction {
            turn: result_turn,
            text: text.clone(),
            long_mode,
        });
        if box_empty {
            self.ghost = Some(crate::tui::composer::PredictionGhost::new(text, long_mode));
        }
    }

    /// Reconcile the ghost with the composer's empty/non-empty state. A
    /// non-empty box hides the ghost (user typing wins); a box cleared back
    /// to empty restores the cached prediction's ghost for the current turn
    /// — **without** a new utility call (the cache is reused).
    pub(super) fn reconcile(&mut self, box_empty: bool) {
        if !box_empty {
            self.ghost = None;
            return;
        }
        if self.ghost.is_none()
            && let Some(p) = &self.cached
            && p.turn == self.turn
        {
            self.ghost = Some(crate::tui::composer::PredictionGhost::new(
                p.text.clone(),
                p.long_mode,
            ));
        }
    }

    /// The Tab-accept terminal step: the ghost converted to real text, so
    /// consume the ghost AND the cache (the prediction has been acted on
    /// and must not be re-offered on a later clear-to-empty).
    pub(super) fn consume(&mut self) {
        self.ghost = None;
        self.cached = None;
    }

    /// Force the feature off (setting changed to `off`): drop cache + ghost.
    pub(super) fn clear(&mut self) {
        self.cached = None;
        self.ghost = None;
    }
}

/// A live async job tracked by the TUI for the jobs strip / `/jobs`.
#[derive(Debug, Clone)]
pub(super) struct ActiveJob {
    /// Session that owns the job. `/jobs` shows every session's jobs;
    /// `/ps` / `/stop` filter to the current session by this id.
    pub(super) session_id: uuid::Uuid,
    pub(super) label: String,
    /// `loop` / `timer` / `background`.
    pub(super) kind: String,
    /// Iterations observed so far (loops; bumped per note).
    pub(super) iteration: u64,
    /// Last time the job showed activity — drives an idle/elapsed readout.
    pub(super) last_activity: Instant,
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
    pub fn new(project: Option<&Path>, no_sandbox: bool) -> Self {
        let launch = welcome::load(project);
        let tui_cfg = load_tui_config(&launch.cwd);
        // The active LLM mode (`prompts/llm-modes-defensive-normal.md`),
        // resolved from the same layered config the daemon root reads.
        let extended = crate::config::extended::load_for_cwd(&launch.cwd);
        let llm_mode = extended.llm_mode;
        let predict_setting = extended.predict_next_message;
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
            busy: false,
            span_started_at: None,
            working_msg_idx: WORKING_MESSAGES.len(),
            reconnect_attempt: None,
            repo_status,
            dialog: Dialog::None,
            model_picker: None,
            stats_pane: None,
            sessions_pane: None,
            skills_pane: None,
            plans_pane: None,
            permissions_pane: None,
            context_pane: None,
            daemon_prompt,
            question_dialog: None,
            pending_init: None,
            daemon_connected,
            daemonless: false,
            daemon_guard: None,
            daemon_signal_task: None,
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
            paste_registry: crate::tui::paste::PasteRegistry::new(),
            queued_tag_calls: Vec::new(),
            at_dismissed: false,
            slash_selected: 0,
            slash_scroll: 0,
            pending_new_session: false,
            last_usage: None,
            estimate_at_last_usage: 0,
            history_estimate_cache: std::cell::Cell::new(None),
            usage_models: HashMap::new(),
            usage_slash: HashMap::new(),
            usage_tags: HashMap::new(),
            project_id: None,
            current_session_persisted: false,
            guidance_estimate: None,
            prunable_tokens: 0,
            cache_cold: true,
            llm_mode,
            elided_event_ids: std::collections::HashSet::new(),
            pending_compact: None,
            pending_prune_confirm: false,
            pending_stop_confirm: None,
            pending_usage: Vec::new(),
            pending_external_edit: false,
            mouse_capture,
            exit_tail_lines,
            rich_text_copy,
            context_menu: None,
            toast: None,
            pane: None,
            pane_side: PaneSide::Full,
            pane_ratio: 0.5,
            pane_focused: false,
            pane_rect: None,
            divider: None,
            pane_body: None,
            dragging_divider: false,
            pending_git_blocks: Vec::new(),
            active_jobs: std::collections::BTreeMap::new(),
            ctrl_c_armed_at: None,
            no_sandbox,
            caffeinate_active: false,
            plan_status: crate::db::plans::PlanStatusCounts::default(),
            side_conversation: None,
            daemon_draining: false,
            predict_setting,
            prediction_state: PredictionState::default(),
            prediction_result: Arc::new(Mutex::new(None)),
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
        // The answering dialog (GOALS §3b) is a compact, bottom-anchored
        // overlay sized to its content (capped), not a fullscreen modal.
        let compact = self
            .question_dialog
            .as_ref()
            .map(|d| d.desired_height())
            .unwrap_or(0);
        PaneGeometry::compute(
            self.input_height(),
            self.indicator_lines(),
            self.queue_lines(),
            self.popup_lines(),
            self.total_history_lines(),
            dialog,
            compact,
        )
    }

    pub async fn run(&mut self) -> Result<()> {
        // The launch banner now renders *inside* the alt screen as the
        // top of the chat pane (see `render_history` / `banner_box`),
        // so we no longer dump it to stdout before entering the alt
        // screen — that only ever showed up in scrollback after exit.

        // Pre-flight: size the instruction file + full system prompt for
        // the fresh-chat context indicator (`X tokens in <file>` plus the
        // baseline the running estimate folds in). Prefers a running
        // daemon's calibrated count, falls back to a local raw-cl100k
        // computation. Best-effort and non-blocking for launch.
        let (provider, model) = match &self.launch.active_model {
            Some((p, m)) => (Some(p.clone()), Some(m.clone())),
            None => (None, None),
        };
        self.guidance_estimate =
            Some(agent_runner::fetch_guidance_estimate(&self.launch.cwd, provider, model).await);

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

        // Bracketed paste (composer-paste-handling): the terminal wraps a
        // genuine paste in escape sequences crossterm surfaces as one
        // `Event::Paste(String)`, distinguishing it from char-by-char
        // typing (which keeps arriving as individual `KeyEvent`s). Without
        // this, large pastes would stream in as a flood of key events and
        // never trigger block behavior.
        let _ = crossterm::execute!(stdout(), crossterm::event::EnableBracketedPaste);

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

        // Process-exit cleanup for an open `/side` (no orphaned ephemeral
        // sessions): discard the throwaway fork *before* the daemon guard
        // reaps an owned ephemeral daemon, so the discard RPC still reaches a
        // live daemon. The daemon's boot sweep is the SIGKILL backstop.
        if self.side_conversation.is_some() {
            self.end_side_conversation(false);
        }

        // Daemonless teardown (happy path): reap the owned ephemeral daemon
        // and stop its signal watcher. The guard routes a synchronous
        // `StopDaemon` through the daemon's single graceful drain path, so
        // an in-flight ephemeral daemon drains before exiting. This fires on
        // a clean quit *and* the error path below (the guard's `Drop` is the
        // backstop if `run` returns early); SIGINT/SIGTERM are covered by the
        // signal task. The self-reaping idle watchdog remains the backstop
        // for an uncatchable death (SIGKILL). Reaping here is independent of
        // whether a message was sent — a persisted session never keeps an
        // owned ephemeral daemon alive past its owner's exit.
        if let Some(task) = self.daemon_signal_task.take() {
            task.abort();
        }
        if let Some(guard) = &self.daemon_guard {
            guard.shutdown();
        }

        // Build the exit-tail text while we still own the alt screen
        // (history is in memory; rendering is irrelevant — we want
        // the plaintext projection of recent entries).
        let tail = self.build_exit_tail_lines();

        if self.mouse_capture {
            let _ = crossterm::execute!(stdout(), DisableMouseCapture);
        }
        let _ = crossterm::execute!(stdout(), crossterm::event::DisableBracketedPaste);
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
        // Print the last opened session id — but only when it was actually
        // persisted (session-id-display-and-lazy-persist). An opened-but-
        // unused session left no DB row, so we print nothing about it.
        // Print the 6-char short id so the exit line matches the welcome
        // box; fall back to the full UUID only if the short id is somehow
        // absent (defensive — it should always be set once attached).
        if self.current_session_persisted {
            if let Some(short_id) = self.launch.session_short_id.as_deref() {
                println!("session {short_id}");
            } else if let Some(session_id) = self.launch.session_id {
                println!("session {session_id}");
            }
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
            self.ensure_session_for_display();
            self.sync_repo_status();
            self.drain_fetch_progress();
            self.drain_agent_events();
            self.drain_prediction();
            self.sync_prediction_ghost();
            self.sync_active_agent();
            self.sync_mouse_capture_from_dialog();
            self.tick_toast();
            self.tick_ctrl_c_window();
            self.dialog.tick();
            // Auto-close the embedded pane when its child has exited
            // (GOALS §1i — e.g. `:q`).
            self.service_pane();
            // In alt-screen mode the viewport is always the full
            // terminal; no need to grow it or spill history into
            // scrollback (alt screen doesn't have scrollback). The
            // wheel-scroll path handles in-app scrollback instead.
            self.maybe_service_new_session(terminal)?;
            self.maybe_service_external_edit(terminal)?;
            self.maybe_service_agent_file_edit(terminal)?;
            terminal.draw(|frame| self.render(frame))?;
            self.sync_cursor_shape();

            if event::poll(EVENT_TICK)? {
                match event::read()? {
                    Event::Key(key) if accepts_key(&key) && self.handle_key(key) => break,
                    Event::Paste(data) => {
                        self.handle_paste(data);
                    }
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

    /// Handle a ctrl+c press (GOALS §3a). Single press interrupts a
    /// running agent (never quits); a second press within
    /// [`CTRL_C_EXIT_WINDOW`] of the previous exits. Returns `true` to
    /// exit the TUI (the event loop breaks). Drives the double-press
    /// state machine via the pure [`decide_ctrl_c`] unit, sends the
    /// daemon `CancelTurn` on an interrupt, and shows the transient exit
    /// hint via the existing toast mechanism.
    pub(super) fn handle_ctrl_c(&mut self) -> bool {
        let (action, new_armed) = decide_ctrl_c(
            Instant::now(),
            self.ctrl_c_armed_at,
            CTRL_C_EXIT_WINDOW,
            self.busy,
        );
        self.ctrl_c_armed_at = new_armed;
        match action {
            CtrlCAction::Exit => true,
            CtrlCAction::ArmAndInterrupt => {
                self.interrupt_agent();
                self.show_ctrl_c_hint();
                false
            }
            CtrlCAction::ArmOnly => {
                self.show_ctrl_c_hint();
                false
            }
        }
    }

    /// Send the daemon a `CancelTurn` for the attached session (GOALS
    /// §3a). Fire-and-forget over the runner's request channel — same
    /// path `/jobs cancel` uses. No-op (and harmless) when no runner is
    /// connected. The daemon aborts the in-flight inference and kills any
    /// running `bash` subprocess; the resulting `AgentIdle` clears `busy`.
    pub(super) fn interrupt_agent(&self) {
        self.send_daemon_request(crate::daemon::proto::Request::CancelTurn);
    }

    /// Show the transient "press ctrl+c again to exit" hint. Reuses the
    /// status-line toast; its TTL is the exit window so it disappears
    /// exactly when a second press would no longer exit.
    fn show_ctrl_c_hint(&mut self) {
        self.toast = Some(Toast {
            text: "Press ctrl+c again to exit".to_string(),
            kind: ToastKind::Info,
            expires_at: Instant::now() + CTRL_C_EXIT_WINDOW,
        });
    }

    /// Disarm the ctrl+c exit window once it has lapsed. Called once per
    /// event-loop tick so a lone press auto-resets to a fresh first press
    /// without needing another event. The hint toast self-expires on the
    /// same TTL via [`Self::tick_toast`].
    pub(super) fn tick_ctrl_c_window(&mut self) {
        if let Some(armed) = self.ctrl_c_armed_at
            && Instant::now().duration_since(armed) > CTRL_C_EXIT_WINDOW
        {
            self.ctrl_c_armed_at = None;
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

    /// Assemble the prediction input from the visible transcript: the
    /// trailing turns, each reduced to the user's message + the agent's
    /// final response text. Tool calls, diffs, subagent reports, plain
    /// notices, and reasoning are skipped — only [`HistoryEntry::User`]
    /// and [`HistoryEntry::Agent`] carry into a turn (the latter's `text`
    /// is the final response; `reasoning` is never included).
    ///
    /// A user message opens a turn; the next agent message closes it.
    /// Consecutive user messages (e.g. queued + folded) flatten into the
    /// most recent open turn's user text so the turn count stays faithful.
    /// `engine::predict::last_turns` then keeps only the last 3.
    pub(super) fn prediction_turns(&self) -> Vec<crate::engine::predict::PredictionTurn> {
        turns_from_history(&self.history)
    }

    /// Kick off the eager next-message prediction for the current turn
    /// (`prompts/predict-next-message.md`). Short-circuits before any
    /// utility call when the setting is `off`, when there's no agent
    /// response to predict from (fresh session), or when no provider
    /// config can be loaded. The result lands in `prediction_result`
    /// tagged with the turn it belongs to; `drain_prediction` adopts it.
    pub(super) fn spawn_prediction(&mut self) {
        let mode = self.predict_setting;
        if !mode.is_enabled() {
            return;
        }
        let turns = self.prediction_turns();
        // Nothing to predict yet (no agent final response) → no call.
        if turns.is_empty() || turns.iter().all(|t| t.agent.trim().is_empty()) {
            return;
        }
        let turn_id = self.prediction_state.turn();
        let cwd = self.launch.cwd.clone();
        let slot = Arc::clone(&self.prediction_result);
        tokio::spawn(async move {
            let (extended, providers) = crate::auto_title::load_configs_for(&cwd);
            // Build the same non-bypassable redaction table the driver uses
            // (GOALS §7) so the prediction prompt is scrubbed before send.
            let redactor = match crate::redact::RedactionTable::build(&extended.redact, &cwd) {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(error = %e, "predict: redaction table build failed; no ghost");
                    return;
                }
            };
            let text =
                crate::engine::predict::predict(&turns, mode, &extended, &providers, &redactor)
                    .await;
            if let Ok(mut guard) = slot.lock() {
                *guard = Some((turn_id, text));
            }
        });
    }

    /// Adopt a completed async prediction. Runs each tick. Discards a
    /// result tagged with a stale turn (a newer turn started) or one that
    /// arrives after the user began typing (box non-empty) —
    /// appear-once-ready, never pop in over active input. On a usable
    /// result for the current empty turn, caches it and builds the ghost.
    pub(super) fn drain_prediction(&mut self) {
        let drained = match self.prediction_result.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => return,
        };
        let Some((turn_id, text)) = drained else {
            return;
        };
        let long_mode = matches!(
            self.predict_setting,
            crate::config::extended::PredictNextMessage::Long
        );
        self.prediction_state
            .on_result(turn_id, text, long_mode, self.composer.is_empty());
    }

    /// Reconcile the ghost with the composer's empty/non-empty state. Runs
    /// each tick after key handling: a non-empty box hides the ghost; a
    /// box cleared back to empty within the same turn restores the cached
    /// prediction's ghost — **without** a new utility call (the cache is
    /// reused). Never overwrites typed content.
    pub(super) fn sync_prediction_ghost(&mut self) {
        self.prediction_state.reconcile(self.composer.is_empty());
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

        // `/new` from inside a side conversation: discard the ephemeral fork
        // first (no orphan), then proceed to open a fresh session. We don't
        // restore the main session's view — `/new` is clearing everything
        // anyway — but the discard must still fire and the chrome flag clear.
        if self.side_conversation.is_some() {
            self.end_side_conversation(false);
        }

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
        // Drop any buffered `/git` blocks — they belong to the old
        // session's next-message that will never be sent now.
        self.pending_git_blocks.clear();
        // The new session starts with no async jobs; the daemon's fresh
        // session has its own (empty) authority.
        self.active_jobs.clear();
        // An armed bare-`/stop` confirm referenced the old session's jobs.
        self.pending_stop_confirm = None;
        // Fresh thread → no wire-side elisions yet.
        self.elided_event_ids.clear();
        self.prunable_tokens = 0;
        // A fresh session has no prior turn to predict from: drop any
        // cached/pending ghost and bump the turn counter so a stale async
        // result from the old session can never land in the new one.
        self.prediction_state.begin_turn();
        // Reload from disk in case settings changed.
        self.reload_launch_info();
        self.reload_tui_config();

        // Repaint the cleared canvas on the next draw.
        terminal.clear()?;

        // Drop the runner so the next submit re-attaches the daemon
        // with `session_id: None`, opening a fresh session.
        self.agent_runner = None;
        // The fresh session is deferred-persistence until its first message
        // (session-id-display-and-lazy-persist).
        self.current_session_persisted = false;

        // Reset the autocomplete tally so the next attach re-seeds it
        // fresh (additive merge would otherwise double-count). The
        // daemon re-fetch picks up everything recorded this session.
        self.usage_models.clear();
        self.usage_slash.clear();
        self.usage_tags.clear();
        self.project_id = None;
        self.pending_usage.clear();
        // Clear the provider usage so the fresh-chat instruction-file
        // estimate re-triggers on the new (empty) session.
        self.last_usage = None;
        self.estimate_at_last_usage = 0;

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
                    // The editor returns plain text; any prior paste
                    // blocks were flattened to their placeholder text when
                    // we wrote the temp file, so drop the registry.
                    self.paste_registry.clear();
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

    /// The `/settings → Agents` page asked to edit an agent file in
    /// `$EDITOR` (`prompts/settings-agents-management.md`). The page can't
    /// suspend the TUI from inside a key handler, so it records the path
    /// and we service it here: suspend ratatui, run `$EDITOR <file>`, then
    /// hand the outcome back so the page re-reads + re-parses the file
    /// (surfacing a parse error inline, never silently accepting a broken
    /// agent). External-process failure leaves the file untouched and is
    /// reported inline. Reuses the same raw-mode/alt-screen toggle dance as
    /// the composer's Ctrl+G handoff.
    pub(super) fn maybe_service_agent_file_edit(
        &mut self,
        terminal: &mut DefaultTerminal,
    ) -> Result<()> {
        let Some(path) = self.dialog.take_pending_agent_edit() else {
            return Ok(());
        };

        let Some(editor) = std::env::var_os("EDITOR") else {
            // Env shifted between the page deciding to defer and now; the
            // page only defers when EDITOR was set, so this is defensive.
            self.dialog
                .finish_agent_edit(Some("$EDITOR is no longer set".to_string()));
            return Ok(());
        };

        use crossterm::terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        };
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();

        let status = std::process::Command::new(&editor).arg(&path).status();

        let _ = enable_raw_mode();
        let _ = crossterm::execute!(stdout(), EnterAlternateScreen);
        terminal.clear()?;

        let editor_error = match status {
            Ok(s) if s.success() => None,
            Ok(s) => Some(format!("editor exited with {s} — file left unchanged")),
            Err(e) => Some(format!(
                "invoking `{}`: {e} — file left unchanged",
                editor.to_string_lossy()
            )),
        };
        self.dialog.finish_agent_edit(editor_error);
        Ok(())
    }

    /// Open `$EDITOR` in an embedded pane (GOALS §1i). No-op if a pane
    /// is already open (one at a time). `side` is `Full` for the bare
    /// `/editor`, or a split side.
    pub(super) fn open_editor(&mut self, side: PaneSide) {
        if self.pane.is_some() {
            return;
        }
        let Some(editor) = std::env::var_os("EDITOR") else {
            self.history.push(HistoryEntry::Plain {
                line: "/editor: no `$EDITOR` set".to_string(),
            });
            return;
        };
        let argv = crate::tui::pty::shell_split(&editor.to_string_lossy());
        if argv.is_empty() {
            self.history.push(HistoryEntry::Plain {
                line: "/editor: `$EDITOR` is empty".to_string(),
            });
            return;
        }
        self.spawn_pane(crate::tui::pty::PaneKind::Editor, &argv, side);
    }

    /// Open `lazygit` fullscreen in an embedded pane (GOALS §1j).
    pub(super) fn open_lazygit(&mut self) {
        if self.pane.is_some() {
            return;
        }
        if !program_on_path("lazygit") {
            self.history.push(HistoryEntry::Plain {
                line: "/lazygit: `lazygit` not found on `PATH`".to_string(),
            });
            return;
        }
        self.spawn_pane(
            crate::tui::pty::PaneKind::Lazygit,
            &["lazygit".to_string()],
            PaneSide::Full,
        );
    }

    /// Spawn a pane. Initial PTY size is a placeholder corrected by the
    /// first render's resize. Focus moves to the new pane.
    fn spawn_pane(&mut self, kind: crate::tui::pty::PaneKind, argv: &[String], side: PaneSide) {
        match crate::tui::pty::PtyPane::spawn(kind, argv, &self.launch.cwd, 24, 80) {
            Ok(pane) => {
                self.pane = Some(pane);
                self.pane_side = side;
                self.pane_focused = true;
                self.dragging_divider = false;
            }
            Err(e) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("/{}: {e}", kind.label()),
                });
            }
        }
    }

    /// Close the open pane and return focus to the composer. `force`
    /// terminates a still-running child (Ctrl+X); otherwise the child
    /// has already exited and we just reap it (auto-close).
    pub(super) fn close_pane(&mut self, force: bool) {
        if let Some(mut pane) = self.pane.take() {
            if force {
                pane.terminate();
            } else {
                pane.reap();
            }
        }
        self.pane_focused = false;
        self.dragging_divider = false;
        self.pane_rect = None;
        self.divider = None;
    }

    /// Service the open pane once per event-loop tick: auto-close when
    /// the child has exited (GOALS §1i).
    pub(super) fn service_pane(&mut self) {
        let exited = self.pane.as_mut().is_some_and(|p| p.has_exited());
        if exited {
            self.close_pane(false);
        }
    }

    /// `!` shell mode (GOALS §1k): run a one-shot command via the shell,
    /// capture stdout+stderr, and render it locally. Never sent to the
    /// agent.
    pub(super) fn run_shell_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return;
        }
        let (raw, failed) = exec_capture_shell(cmd, &self.launch.cwd);
        let clean = strip_ansi(&raw);
        self.history.push(HistoryEntry::LocalCommand {
            label: format!("! {cmd}"),
            output: cap_display_lines(&clean),
            failed,
        });
        self.chat_scroll_offset = 0;
    }

    /// `/git` (GOALS §1l): run `git <args>` locally, render it now, and
    /// buffer a `<git>` block (~2k-token cap) for the next user message.
    pub(super) fn run_git_command(&mut self, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            self.history.push(HistoryEntry::Plain {
                line: "/git: usage `/git <args>` (e.g. `/git status`)".to_string(),
            });
            return;
        }
        let (raw, failed) = exec_capture_git(args, &self.launch.cwd);
        let clean = strip_ansi(&raw);
        self.history.push(HistoryEntry::LocalCommand {
            label: format!("/git {args}"),
            output: cap_display_lines(&clean),
            failed,
        });
        self.chat_scroll_offset = 0;
        let capped = cap_tokens(&clean, GIT_AGENT_TOKEN_CAP);
        self.pending_git_blocks.push(format!(
            "<git cmd=\"{}\">\n{}\n</git>",
            xml_escape(args),
            capped
        ));
    }

    /// `/init [path]`: explore the project and write its instructions
    /// file via the normal `Build` → `coder` (single-writer) delegation
    /// path. With no arg the target is the first configured guidance
    /// filename (`agent_guidance_files[0]`, default `AGENTS.md`); with an
    /// arg it's that path. When the target already exists, opens the
    /// update/overwrite/cancel prompt (reusing the question dialog) and
    /// honors the choice; otherwise dispatches the fresh-write turn
    /// immediately. `extended-config.json` is never touched.
    pub(super) fn handle_init_command(&mut self, args: &str) {
        if self.busy {
            self.history.push(HistoryEntry::Plain {
                line: "/init: a turn is already running — wait for it to finish".to_string(),
            });
            return;
        }
        let explicit = {
            let a = args.trim();
            if a.is_empty() { None } else { Some(a) }
        };
        let target = crate::commands::init::resolve_target(&self.launch.cwd, explicit);
        let display = crate::commands::init::display_target(&self.launch.cwd, &target);

        if target.exists() {
            // Existing target: ask update / overwrite / cancel via the
            // shared question dialog, driven locally (no daemon interrupt).
            use crate::daemon::proto::{InterruptOption, InterruptQuestion, InterruptQuestionSet};
            let interrupt_id = uuid::Uuid::new_v4();
            let set = InterruptQuestionSet {
                questions: vec![InterruptQuestion::Single {
                    prompt: format!("`{display}` already exists — how should /init proceed?"),
                    options: vec![
                        InterruptOption {
                            id: "update".into(),
                            label: "Update in place".into(),
                            description: Some(
                                "Revise and extend, preserving accurate content".into(),
                            ),
                        },
                        InterruptOption {
                            id: "overwrite".into(),
                            label: "Overwrite from scratch".into(),
                            description: Some("Replace the file entirely".into()),
                        },
                        InterruptOption {
                            id: "cancel".into(),
                            label: "Cancel".into(),
                            description: None,
                        },
                    ],
                    allow_freetext: false,
                    command_detail: None,
                }],
            };
            let lockout = Duration::from_millis(load_dialog_config(&self.launch.cwd).lockout_ms);
            self.pending_init = Some(PendingInit {
                interrupt_id,
                display,
            });
            self.question_dialog = Some(crate::tui::dialog::question::QuestionDialog::new(
                interrupt_id,
                String::new(),
                set,
                lockout,
            ));
            return;
        }

        // Fresh file: dispatch the create turn straight away.
        let prompt = crate::commands::init::build_init_prompt(
            &display,
            crate::commands::init::InitMode::Create,
        );
        self.dispatch_init_turn(&display, prompt);
    }

    /// Resolve a closed `/init` existing-file prompt. `selected_id` is the
    /// chosen option id (or `None` on Esc/cancel). Update/overwrite
    /// dispatch the corresponding agent turn; cancel leaves the file
    /// untouched.
    pub(super) fn resolve_init_choice(&mut self, selected_id: Option<&str>) {
        let Some(pending) = self.pending_init.take() else {
            return;
        };
        let mode = match selected_id {
            Some("update") => crate::commands::init::InitMode::Update,
            Some("overwrite") => crate::commands::init::InitMode::Overwrite,
            _ => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("/init: cancelled — `{}` left untouched", pending.display),
                });
                return;
            }
        };
        let prompt = crate::commands::init::build_init_prompt(&pending.display, mode);
        self.dispatch_init_turn(&pending.display, prompt);
    }

    /// Send an `/init` turn to the agent: render `/init <target>` as the
    /// user's turn (display side) and hand the full exploration+write
    /// instruction to the agent as the wire (wire/user split, GOALS §14).
    /// Reuses the runner input channel `submit_input` uses, including the
    /// working-span bookkeeping so an orphaned dispatch never hangs the
    /// indicator.
    fn dispatch_init_turn(&mut self, display: &str, wire: String) {
        self.chat_scroll_offset = 0;
        self.begin_working_span();
        self.history.push(HistoryEntry::User {
            text: format!("/init {display}"),
            timestamp: chrono::Local::now(),
        });
        self.ensure_agent_runner();
        let submission = crate::engine::message::UserSubmission::text(wire);
        let orphaned = match self.agent_runner.as_ref() {
            Some(Ok(runner)) => match runner.input_tx.try_send(submission) {
                Ok(()) => {
                    self.current_session_persisted = true;
                    false
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.history.push(HistoryEntry::Plain {
                        line: "/init: engine input queue full — try again in a moment".to_string(),
                    });
                    true
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    self.history.push(HistoryEntry::Plain {
                        line: "/init: engine driver task has exited".to_string(),
                    });
                    true
                }
            },
            Some(Err(e)) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("/init: engine: {e}"),
                });
                true
            }
            None => {
                self.history.push(HistoryEntry::Plain {
                    line: "/init: no engine runner — cannot start".to_string(),
                });
                true
            }
        };
        // A turn the worker never received emits no `AgentIdle`, so undo
        // the span this dispatch opened.
        if orphaned {
            self.end_working_span();
        }
    }

    /// `/jobs` (GOALS §22): list active async jobs, or `/jobs cancel
    /// <job-id>` to cancel one (the human-side cancel affordance — these
    /// run on the user's dime). Cancellation rides the same fire-and-forget
    /// request channel the autocomplete tally uses.
    pub(super) fn handle_jobs_command(&mut self, args: &str) {
        let args = args.trim();
        if let Some(rest) = args.strip_prefix("cancel") {
            let job_id = rest.trim();
            if job_id.is_empty() {
                self.history.push(HistoryEntry::Plain {
                    line: "/jobs: usage `/jobs cancel <job-id>`".to_string(),
                });
                return;
            }
            let sent = match self.agent_runner.as_ref() {
                Some(Ok(runner)) => runner
                    .record_tx
                    .try_send(crate::daemon::proto::Request::CancelJob {
                        job_id: job_id.to_string(),
                    })
                    .is_ok(),
                _ => false,
            };
            let line = if sent {
                format!("/jobs: cancel requested for `{job_id}`")
            } else {
                format!("/jobs: no daemon connection — cannot cancel `{job_id}`")
            };
            self.history.push(HistoryEntry::Plain { line });
            return;
        }
        // Bare `/jobs`: list.
        if self.active_jobs.is_empty() {
            self.history.push(HistoryEntry::Plain {
                line: "/jobs: no active jobs".to_string(),
            });
            return;
        }
        self.history.push(HistoryEntry::Plain {
            line: "/jobs: active —".to_string(),
        });
        for (job_id, j) in &self.active_jobs {
            self.history.push(HistoryEntry::Plain {
                line: format!(
                    "  {}  (cancel: /jobs cancel {job_id})",
                    format_job_line(job_id, j)
                ),
            });
        }
    }

    /// The id of the session this client is attached to (live runner if
    /// connected, else the last-attached id from launch info). `None`
    /// before the first session exists. Same resolution `/rename` uses.
    pub(super) fn current_session_id(&self) -> Option<uuid::Uuid> {
        match self.agent_runner.as_ref() {
            Some(Ok(runner)) => Some(runner.session_id),
            _ => self.launch.session_id,
        }
    }

    /// Job ids in `active_jobs` that belong to the current session, in the
    /// map's (stable, job-id) order. The single filter `/ps` and `/stop`
    /// share so the listed set, the cancel set, and the confirm count can
    /// never disagree. Empty when there's no current session or no jobs.
    pub(super) fn current_session_job_ids(&self) -> Vec<String> {
        match self.current_session_id() {
            Some(sid) => session_job_ids(&self.active_jobs, sid),
            None => Vec::new(),
        }
    }

    /// `/ps` — list only the current session's running async jobs, using
    /// the same per-job formatting `/jobs` shows. Empty state matches the
    /// spec. Current-session-scoped; never reaches other sessions (that's
    /// `/jobs`).
    pub(super) fn handle_ps_command(&mut self) {
        let ids = self.current_session_job_ids();
        if ids.is_empty() {
            self.history.push(HistoryEntry::Plain {
                line: "No background jobs in this session.".to_string(),
            });
            return;
        }
        self.history.push(HistoryEntry::Plain {
            line: "/ps: active in this session —".to_string(),
        });
        for job_id in ids {
            if let Some(j) = self.active_jobs.get(&job_id) {
                self.history.push(HistoryEntry::Plain {
                    line: format!("  {}  (stop: /stop {job_id})", format_job_line(&job_id, j)),
                });
            }
        }
    }

    /// `/stop` — stop current-session jobs. `/stop <job-id>` cancels that
    /// one immediately (reusing the `/jobs cancel` `CancelJob` path);
    /// refuses an id outside the current session rather than reaching
    /// across. Bare `/stop` arms a `[y/N]` confirm to cancel them all.
    pub(super) fn handle_stop_command(&mut self, args: &str) {
        let job_id = args.trim();
        if job_id.is_empty() {
            self.arm_stop_confirm();
            return;
        }
        let in_session = self.current_session_job_ids().iter().any(|id| id == job_id);
        if !in_session {
            self.history.push(HistoryEntry::Plain {
                line: format!(
                    "/stop: no job `{job_id}` in this session (use /jobs for other sessions)"
                ),
            });
            return;
        }
        self.cancel_job(job_id, "/stop");
    }

    /// Send a `CancelJob` for one job over the runner's record channel —
    /// the same fire-and-forget path `/jobs cancel` uses. `cmd` is the
    /// command label for the rendered line.
    fn cancel_job(&mut self, job_id: &str, cmd: &str) {
        let sent = self.send_daemon_request(crate::daemon::proto::Request::CancelJob {
            job_id: job_id.to_string(),
        });
        let line = if sent {
            format!("{cmd}: cancel requested for `{job_id}`")
        } else {
            format!("{cmd}: no daemon connection — cannot cancel `{job_id}`")
        };
        self.history.push(HistoryEntry::Plain { line });
    }

    /// Bare `/stop`: count the current-session jobs and arm the `[y/N]`
    /// confirm (mirrors `/prune`'s arm-then-commit). With zero jobs it
    /// says so and arms nothing.
    pub(super) fn arm_stop_confirm(&mut self) {
        let ids = self.current_session_job_ids();
        if ids.is_empty() {
            self.history.push(HistoryEntry::Plain {
                line: "No background jobs in this session.".to_string(),
            });
            self.pending_stop_confirm = None;
            return;
        }
        let n = ids.len();
        self.history.push(HistoryEntry::Plain {
            line: format!("/stop: Stop {n} job(s) in this session? [y/N]"),
        });
        self.pending_stop_confirm = Some(ids);
    }

    /// Commit an armed bare `/stop`: cancel every job captured at arm
    /// time. A job that already ended (no longer in `active_jobs`) is
    /// skipped silently — its strip entry is already gone.
    pub(super) fn commit_stop(&mut self) {
        let Some(ids) = self.pending_stop_confirm.take() else {
            return;
        };
        let mut cancelled = 0;
        for job_id in ids {
            if self.active_jobs.contains_key(&job_id) {
                self.cancel_job(&job_id, "/stop");
                cancelled += 1;
            }
        }
        if cancelled == 0 {
            self.history.push(HistoryEntry::Plain {
                line: "/stop: those jobs already ended.".to_string(),
            });
        }
    }

    /// Cancel an armed bare `/stop`.
    pub(super) fn cancel_stop(&mut self) {
        self.pending_stop_confirm = None;
        self.history.push(HistoryEntry::Plain {
            line: "/stop: cancelled.".to_string(),
        });
    }

    /// `/plan` / `/build` — swap the session's primary agent (`plan.md
    /// §4.6.d`). Sends `SetAgent`, which the worker persists and forwards to
    /// the driver as a live root-frame swap at the idle boundary; the chrome
    /// updates off the daemon's `PrimarySwapped` event. A no-op message when
    /// no runner is connected yet.
    /// `/llm-mode [toggle|defend|defensive|normal]` — switch the active
    /// LLM-strength steering mode live (`prompts/llm-modes-defensive-normal.md`).
    /// No argument or `toggle` flips between `normal`/`defensive` (the default
    /// action); `defend` (advertised, shorter to type) and its silent alias
    /// `defensive` select defensive; `normal` selects normal. Switching busts
    /// the cached system prefix, so we surface the shared cache-break warning
    /// (suppressed on a no-cache provider). The actual rebuild happens
    /// daemon-side; the `LlmModeChanged` event confirms it.
    pub(super) fn handle_llm_mode_command(&mut self, arg: &str) {
        let requested = match parse_llm_mode_arg(arg) {
            Ok(r) => r,
            Err(usage) => {
                self.history.push(HistoryEntry::Plain { line: usage });
                return;
            }
        };
        // Resolve the target (for the no-op check + warning), against the
        // tracked authoritative value. The daemon re-resolves a toggle too,
        // so a stale client value can't desync the outcome.
        let target = requested.unwrap_or_else(|| self.llm_mode.toggled());
        if target == self.llm_mode {
            self.history.push(HistoryEntry::Plain {
                line: format!("Already in `{}` LLM mode", target.as_str()),
            });
            return;
        }
        let sent =
            self.send_daemon_request(crate::daemon::proto::Request::SetLlmMode { mode: requested });
        if !sent {
            self.history.push(HistoryEntry::Plain {
                line: "Send a message first to start a session, then switch LLM mode".to_string(),
            });
            return;
        }
        // Cache-break warning via the shared helper (silent on no-cache).
        if let Some(warning) = self.cache_break_warning() {
            self.history.push(HistoryEntry::Plain { line: warning });
        }
        // The `LlmModeChanged` event pushes the "Switched to …" confirmation
        // once the daemon applies it.
    }

    /// Shared cache-break warning helper. Returns the one-line warning to
    /// show when an action busts the cached system prefix (a `/llm-mode`
    /// switch today; the shift+tab agent cycle and `/agent` — specced
    /// elsewhere — reuse this verbatim). Returns `None` when the warning is
    /// meaningless because the active model/provider does not cache: reuses
    /// the pruning-policy no-cache predicate
    /// ([`crate::engine::prune::cache_state`] →
    /// [`crate::engine::prune::ColdReason::NoCacheProvider`]) rather than
    /// re-deriving "does this provider cache."
    pub(super) fn cache_break_warning(&self) -> Option<String> {
        if self.active_provider_caches() {
            Some(
                "Heads up: switching busts the prompt cache — the next call re-sends the \
                 full prefix uncached."
                    .to_string(),
            )
        } else {
            // No-cache provider: nothing to bust, so no warning.
            None
        }
    }

    /// Whether the active model/provider has a prompt cache at all. Reuses
    /// the pruning-policy no-cache predicate: the resolved
    /// [`crate::config::providers::CacheConfig`] is fed to
    /// [`crate::engine::prune::cache_state`]; a `NoCacheProvider` cold reason
    /// means it never caches. Best-effort — an unresolvable model is treated
    /// as caching so the warning errs on the side of showing.
    fn active_provider_caches(&self) -> bool {
        let Some((provider, model)) = self.launch.active_model.as_ref() else {
            return true;
        };
        let providers = crate::config::dirs::discover_config_dirs(&self.launch.cwd)
            .into_iter()
            .find_map(|d| {
                crate::config::providers::ConfigDoc::load(&d.path.join("config.json")).ok()
            })
            .map(|d| d.providers())
            .unwrap_or_default();
        let cache = providers.resolve_cache(provider, model);
        cache_config_caches(&cache)
    }

    pub(super) fn swap_primary_agent(&mut self, name: &str) {
        let sent = self.send_daemon_request(crate::daemon::proto::Request::SetAgent {
            name: name.to_string(),
        });
        let line = if sent {
            format!("Switched primary agent to `{name}`")
        } else {
            "Send a message first to start a session, then switch agents".to_string()
        };
        self.history.push(HistoryEntry::Plain { line });
    }

    /// Send a fire-and-forget daemon request over the runner's record
    /// channel (same path `/jobs cancel` uses). Returns whether a runner
    /// was connected to receive it.
    pub(super) fn send_daemon_request(&self, req: crate::daemon::proto::Request) -> bool {
        match self.agent_runner.as_ref() {
            Some(Ok(runner)) => runner.record_tx.try_send(req).is_ok(),
            _ => false,
        }
    }

    /// Open the question dialog for a needs-attention resolver item
    /// (`plan-status-chrome-and-resolver.md`). Reuses the exact dialog the
    /// daemon-pushed `InterruptRaised` uses (no second dialog): it carries the
    /// item's `interrupt_id`, so the submit/cancel routes through
    /// [`Self::resolve_question_dialog`] → `ResolveInterrupt`, resuming the
    /// paused plan step without blocking its siblings. A single-question
    /// (`question`) item wraps to a one-element set, wire-equivalent to a
    /// batch of one. An item with no question payload (defensive) is a no-op.
    pub(super) fn open_attention_dialog(&mut self, item: crate::daemon::proto::AttentionItemWire) {
        use crate::daemon::proto::InterruptQuestionSet;
        let set = match (item.questions, item.question) {
            (Some(set), _) => set,
            (None, Some(q)) => InterruptQuestionSet { questions: vec![q] },
            (None, None) => return,
        };
        let lockout = Duration::from_millis(load_dialog_config(&self.launch.cwd).lockout_ms);
        self.question_dialog = Some(crate::tui::dialog::question::QuestionDialog::new(
            item.interrupt_id,
            item.description,
            set,
            lockout,
        ));
    }

    /// Send the answering dialog's outcome back to the daemon (GOALS
    /// §3b). Both submit and cancel become a `ResolveInterrupt` — cancel
    /// carries `ResolveResponse::Cancel`, which the worker fans out to a
    /// per-question `Cancel` so the blocked `question` tool unblocks with
    /// dismissed answers.
    pub(super) fn resolve_question_dialog(
        &self,
        result: crate::tui::dialog::question::QuestionResult,
    ) {
        use crate::daemon::proto::{Request, ResolveResponse};
        use crate::tui::dialog::question::QuestionResult;
        let (interrupt_id, response) = match result {
            QuestionResult::Submit {
                interrupt_id,
                responses,
            } => (interrupt_id, ResolveResponse::Batch { responses }),
            QuestionResult::Cancel { interrupt_id } => (interrupt_id, ResolveResponse::Cancel),
        };
        self.send_daemon_request(Request::ResolveInterrupt {
            interrupt_id,
            response,
        });
    }

    /// `/prune` (T6.d): show the before→after context % and the
    /// cache-bust warning, then arm the confirm. The numbers come from the
    /// daemon-authoritative `prunable_tokens` (same `dedup_plan` `/prune`
    /// executes), so the projection equals the result.
    pub(super) fn arm_prune_confirm(&mut self) {
        if self.prunable_tokens == 0 {
            self.history.push(HistoryEntry::Plain {
                line: "/prune: 0% prunable — nothing to do.".to_string(),
            });
            self.pending_prune_confirm = false;
            return;
        }
        let tokens = self.context_tokens();
        let prunable = self.prunable_tokens;
        let numbers = match self.launch.active_model_max_context {
            Some(max) if max > 0 => {
                let pct = (tokens as u64 * 100 / max as u64).min(999);
                let after = (tokens as u64).saturating_sub(prunable);
                let after_pct = (after * 100 / max as u64).min(999);
                format!("context {pct}% → {after_pct}% (~{prunable} wire tokens)")
            }
            _ => format!("~{prunable} wire tokens"),
        };
        // Cache warning derived from the predicate, not a guess.
        let cache_line = if self.cache_cold {
            "Cache is cold — pruning is free (auto-prune normally handles this)."
        } else {
            "Cache is HOT — pruning breaks it; the cache-bust cost may exceed the savings. \
             When the cache goes cold, auto-prune handles it for free."
        };
        self.history.push(HistoryEntry::Plain {
            line: format!(
                "/prune: {numbers}. {cache_line} Press y or Enter to confirm, any other key to cancel."
            ),
        });
        self.pending_prune_confirm = true;
    }

    /// Commit an armed `/prune`: send the request to the daemon. The
    /// `Pruned` + refreshed `ContextProjection` events render the result.
    pub(super) fn commit_prune(&mut self) {
        self.pending_prune_confirm = false;
        if !self.send_daemon_request(crate::daemon::proto::Request::Prune) {
            self.history.push(HistoryEntry::Plain {
                line: "/prune: no daemon connection — cannot prune.".to_string(),
            });
        }
    }

    /// Cancel an armed `/prune`.
    pub(super) fn cancel_prune(&mut self) {
        self.pending_prune_confirm = false;
        self.history.push(HistoryEntry::Plain {
            line: "/prune: cancelled.".to_string(),
        });
    }

    /// `/compact` (T6.e): request the daemon assemble a fresh-thread
    /// handoff. The result arrives as a `CompactReady` event that drops
    /// the handoff into the composer for review-then-commit.
    pub(super) fn start_compact(&mut self) {
        if !self.send_daemon_request(crate::daemon::proto::Request::Compact) {
            self.history.push(HistoryEntry::Plain {
                line: "/compact: no daemon connection — cannot compact.".to_string(),
            });
            return;
        }
        self.history.push(HistoryEntry::Plain {
            line: "/compact: assembling handoff (prune-first, model brief, deterministic appendix, seed tools)…".to_string(),
        });
    }

    /// Commit a reviewed `/compact` handoff (T6.e step 5). Re-attaches the
    /// TUI to the fresh session the daemon created and sends the (edited)
    /// handoff as its first user message; the fresh session re-executes
    /// its seed tools before the first inference. The old session is
    /// preserved in SQLite. Returns the `submit_input` quit signal
    /// (always `false`).
    pub(super) fn commit_compact(&mut self, handoff: String) -> bool {
        let Some(pending) = self.pending_compact.take() else {
            return false;
        };
        self.composer.clear();
        self.paste_registry.clear();
        // Switch the runner onto the fresh session.
        match agent_runner::attach_to_session(
            &self.launch.cwd,
            pending.new_session_id,
            self.no_sandbox,
            self.lifecycle_mode(),
        ) {
            Ok(runner) => {
                // Daemonless: this re-attach reconnects to our owned
                // ephemeral daemon; ensure the ownership guard is armed.
                self.arm_daemon_guard(&runner);
                // Fresh thread: clear the transcript view + queue + pending.
                self.history.clear();
                self.queue.clear();
                self.pending = None;
                self.prunable_tokens = 0;
                // Fresh thread → no wire-side elisions carry over.
                self.elided_event_ids.clear();
                self.launch.session_id = Some(runner.session_id);
                self.launch.session_short_id = Some(runner.short_id.clone());
                // The compaction successor session already has a DB row
                // (session-id-display-and-lazy-persist).
                self.current_session_persisted = true;
                self.agent_runner = Some(Ok(runner));
                // Boundary marker at the top of the fresh session's
                // scrollback (the divider-equivalent for compaction). Only
                // when we know the predecessor's short id; otherwise fall
                // back to the plain status line below.
                if !pending.predecessor_short_id.is_empty() {
                    self.history.push(HistoryEntry::CompactBoundary {
                        predecessor_short_id: pending.predecessor_short_id.clone(),
                        seed_tool_count: pending.seed_tool_count,
                        seed_tool_tokens: pending.seed_tool_tokens,
                    });
                }
                self.history.push(HistoryEntry::Plain {
                    line: format!(
                        "/compact: committed — fresh session started ({} seed tool(s) re-running). Old session recoverable via `cockpit session resume`.",
                        pending.seed_tool_count
                    ),
                });
                self.history.push(HistoryEntry::User {
                    text: handoff.clone(),
                    timestamp: chrono::Local::now(),
                });
                self.begin_working_span();
                if let Some(Ok(runner)) = self.agent_runner.as_ref() {
                    let _ = runner
                        .input_tx
                        .try_send(crate::engine::message::UserSubmission::text(handoff));
                }
            }
            Err(e) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("/compact: could not attach to fresh session: {e}"),
                });
            }
        }
        false
    }

    /// Resume `session_id` from the `/sessions` browser. Reuses the
    /// existing session-switch path (`attach_to_session`, the same plumbing
    /// `/compact` commit uses) — the runner's event stream + input channel
    /// move onto the resumed session, and the daemon marks it viewed on
    /// attach (clearing its unread state). Mirrors `commit_compact`'s
    /// transcript reset; new agent output streams in live.
    pub(super) fn resume_session(&mut self, session_id: uuid::Uuid) {
        // Resuming another session from inside a side conversation: discard the
        // ephemeral fork first (no orphan). The resume below then overwrites
        // the restored main view with the resumed session's.
        if self.side_conversation.is_some() {
            self.end_side_conversation(false);
        }
        match agent_runner::attach_to_session(
            &self.launch.cwd,
            session_id,
            self.no_sandbox,
            self.lifecycle_mode(),
        ) {
            Ok(runner) => {
                // Daemonless: keep the ownership guard armed across resume.
                self.arm_daemon_guard(&runner);
                let short_id = runner.short_id.clone();
                self.project_id = Some(runner.project_id.clone());
                self.launch.session_id = Some(runner.session_id);
                self.launch.session_short_id = Some(runner.short_id.clone());
                // A resumed session already has a DB row
                // (session-id-display-and-lazy-persist).
                self.current_session_persisted = true;
                // Switch the runner: fresh transcript view bound to the
                // resumed session.
                self.history.clear();
                self.queue.clear();
                self.pending = None;
                self.prunable_tokens = 0;
                self.elided_event_ids.clear();
                self.active_jobs.clear();
                self.pending_stop_confirm = None;
                self.chat_scroll_offset = 0;
                self.agent_runner = Some(Ok(runner));
                let label = if short_id.is_empty() {
                    session_id.to_string()
                } else {
                    short_id
                };
                self.history.push(HistoryEntry::Plain {
                    line: format!("/resume: switched to session {label}."),
                });
            }
            Err(e) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("/resume: could not attach to session: {e}"),
                });
            }
        }
    }

    /// `/side [end]`: throwaway side conversation forked from here.
    ///
    /// - bare `/side` forks the current session into an **ephemeral** fork
    ///   and switches the TUI onto it (full prior history stays visible).
    /// - `/side end` returns to the unchanged main session and discards the
    ///   ephemeral fork.
    ///
    /// `/side` while already in a side conversation is a flat, deterministic
    /// no-op (a persisted branch is `/fork`, not nested `/side`).
    pub(super) fn handle_side_command(&mut self, args: &str) {
        let arg = args.trim();
        if arg.eq_ignore_ascii_case("end") {
            if self.side_conversation.is_some() {
                self.end_side_conversation(true);
            } else {
                self.history.push(HistoryEntry::Plain {
                    line: "/side: not in a side conversation".to_string(),
                });
            }
            return;
        }
        if !arg.is_empty() {
            self.history.push(HistoryEntry::Plain {
                line: "Usage: `/side` to start, `/side end` to discard".to_string(),
            });
            return;
        }
        if self.side_conversation.is_some() {
            // Deterministic no-op: already in a side conversation, don't nest.
            self.history.push(HistoryEntry::Plain {
                line: "/side: already in a side conversation (`/side end` to discard)".to_string(),
            });
            return;
        }
        self.enter_side_conversation();
    }

    /// Fork the current (main) session into an ephemeral throwaway and switch
    /// the TUI onto it. The fork reuses `ForkSession` (with `ephemeral`), and
    /// we keep the visible scrollback so the user sees the full prior history.
    /// The main-session view is snapshotted into `side_conversation` so a
    /// later `/side end` / Esc / exit restores it verbatim.
    fn enter_side_conversation(&mut self) {
        // Need a live runner: the side fork goes onto the same daemon, and
        // forking off an un-persisted session has nothing to branch from.
        let (parent_session_id, socket) = match self.agent_runner.as_ref() {
            Some(Ok(runner)) => (runner.session_id, runner.socket.clone()),
            _ => {
                self.history.push(HistoryEntry::Plain {
                    line: "/side: no active session to fork from".to_string(),
                });
                return;
            }
        };
        // Forking off a never-persisted session has no parent row in the DB.
        if !self.current_session_persisted {
            self.history.push(HistoryEntry::Plain {
                line: "/side: send a message first — there's nothing to fork yet".to_string(),
            });
            return;
        }

        let (side_session_id, side_short_id) =
            match agent_runner::fork_session_blocking(&socket, parent_session_id, true) {
                Ok(pair) => pair,
                Err(e) => {
                    // Fork failed (daemon error): report and stay in main.
                    self.history.push(HistoryEntry::Plain {
                        line: format!("/side: could not fork: {e}"),
                    });
                    return;
                }
            };

        // Attach to the ephemeral fork. On failure, discard the orphan fork
        // we just created and stay in the main session, untouched.
        let runner = match agent_runner::attach_to_session(
            &self.launch.cwd,
            side_session_id,
            self.no_sandbox,
            self.lifecycle_mode(),
        ) {
            Ok(runner) => runner,
            Err(e) => {
                let _ = agent_runner::discard_session_blocking(&socket, side_session_id);
                self.history.push(HistoryEntry::Plain {
                    line: format!("/side: could not enter side conversation: {e}"),
                });
                return;
            }
        };
        self.arm_daemon_guard(&runner);

        // Snapshot the main-session view, then swap onto the side fork. We
        // keep `history` (prior scrollback stays visible) but take everything
        // else into the snapshot so `end` restores it exactly.
        let side = SideConversation {
            side_session_id,
            socket,
            saved_runner: self.agent_runner.take(),
            saved_history: self.history.clone(),
            saved_queue: std::mem::take(&mut self.queue),
            saved_pending: self.pending.take(),
            saved_prunable_tokens: self.prunable_tokens,
            saved_cache_cold: self.cache_cold,
            saved_elided_event_ids: std::mem::take(&mut self.elided_event_ids),
            saved_active_jobs: std::mem::take(&mut self.active_jobs),
            saved_pending_stop_confirm: self.pending_stop_confirm.take(),
            saved_chat_scroll_offset: self.chat_scroll_offset,
            saved_project_id: self.project_id.clone(),
            saved_session_id: self.launch.session_id,
            saved_session_short_id: self.launch.session_short_id.clone(),
            saved_current_session_persisted: self.current_session_persisted,
        };

        self.project_id = Some(runner.project_id.clone());
        self.launch.session_id = Some(runner.session_id);
        self.launch.session_short_id = Some(runner.short_id.clone());
        // The ephemeral fork is never surfaced as resumable — keep
        // `current_session_persisted = false` so the exit-tail never prints
        // its id, even though the fork has a (throwaway) DB row.
        self.current_session_persisted = false;
        // Reset the live-view fields the side conversation tracks on its own;
        // the visible scrollback (history) is intentionally preserved.
        self.queue.clear();
        self.pending = None;
        self.prunable_tokens = 0;
        self.cache_cold = true;
        self.elided_event_ids.clear();
        self.active_jobs.clear();
        self.pending_stop_confirm = None;
        self.chat_scroll_offset = 0;
        self.agent_runner = Some(Ok(runner));
        self.side_conversation = Some(side);

        self.history.push(HistoryEntry::Plain {
            line: format!(
                "Side conversation {side_short_id} — a throwaway fork. `/side end` (or Esc on an empty line) to discard and return."
            ),
        });
    }

    /// End the open side conversation: restore the main-session view verbatim
    /// and discard the ephemeral fork (row + descendant forks). Unconditional
    /// — no "keep this fork?" prompt (that's `/fork`). `announce` controls the
    /// confirmation line; the process-exit path passes `false`.
    pub(super) fn end_side_conversation(&mut self, announce: bool) {
        let Some(side) = self.side_conversation.take() else {
            return;
        };

        // Discard the ephemeral fork: stops its worker and deletes its row.
        // Best-effort — a transport failure still leaves the daemon's boot
        // sweep as the backstop, so an orphan can't survive long.
        if let Err(e) = agent_runner::discard_session_blocking(&side.socket, side.side_session_id) {
            tracing::warn!(error = %e, side_session_id = %side.side_session_id,
                "discarding ephemeral side session failed; boot sweep will reclaim it");
        }

        // Restore the main-session view exactly as it was on entry.
        self.agent_runner = side.saved_runner;
        self.history = side.saved_history;
        self.queue = side.saved_queue;
        self.pending = side.saved_pending;
        self.prunable_tokens = side.saved_prunable_tokens;
        self.cache_cold = side.saved_cache_cold;
        self.elided_event_ids = side.saved_elided_event_ids;
        self.active_jobs = side.saved_active_jobs;
        self.pending_stop_confirm = side.saved_pending_stop_confirm;
        self.chat_scroll_offset = side.saved_chat_scroll_offset;
        self.project_id = side.saved_project_id;
        self.launch.session_id = side.saved_session_id;
        self.launch.session_short_id = side.saved_session_short_id;
        self.current_session_persisted = side.saved_current_session_persisted;
        // The daemonless ownership guard stays armed throughout — the side
        // fork lives on the same owned daemon, so it's never dropped and
        // needs no re-arming here.

        if announce {
            self.history.push(HistoryEntry::Plain {
                line: "Side conversation discarded — back in the main session.".to_string(),
            });
        }
    }

    /// `/pin <text>`: mark a message as must-survive for the next
    /// `/compact` (injected verbatim, never summarized).
    /// `/sandbox` (sandboxing part 2): no arg toggles, `on`/`off` set
    /// explicitly. Sends `SetSandbox` to the daemon for the attached
    /// session; the resulting state is surfaced via the `SandboxState`
    /// event → toast. Effective immediately for subsequent tool calls.
    pub(super) fn handle_sandbox_command(&mut self, args: &str) {
        let enabled = match parse_sandbox_arg(args) {
            Ok(e) => e,
            Err(other) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("/sandbox: unknown arg `{other}` — use `on` or `off`"),
                });
                return;
            }
        };
        if !self.send_daemon_request(crate::daemon::proto::Request::SetSandbox { enabled }) {
            self.history.push(HistoryEntry::Plain {
                line: "/sandbox: no daemon connection".to_string(),
            });
        }
    }

    /// `/caffeinate [toggle|on|off|until-idle]`: suppress system sleep +
    /// lid-close so agents survive a closed lid. Daemon-owned state — this
    /// just sends the request; the daemon acquires/releases the OS
    /// assertion and broadcasts a `CaffeinateState` event back (→ toast +
    /// ☕ glyph). Bare command toggles.
    pub(super) fn handle_caffeinate_command(&mut self, args: &str) {
        let mode = match crate::daemon::caffeinate::CaffeinateMode::parse(args) {
            Ok(m) => m,
            Err(other) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!(
                        "/caffeinate: unknown arg `{other}` — use `on`, `off`, `until-idle`, or no arg to toggle"
                    ),
                });
                return;
            }
        };
        if !self.send_daemon_request(crate::daemon::proto::Request::SetCaffeinate { mode }) {
            self.history.push(HistoryEntry::Plain {
                line: "/caffeinate: no daemon connection".to_string(),
            });
        }
    }

    pub(super) fn handle_pin_command(&mut self, args: &str) {
        let text = args.trim();
        if text.is_empty() {
            self.history.push(HistoryEntry::Plain {
                line: "/pin: usage `/pin <text>` — pins a message verbatim for /compact"
                    .to_string(),
            });
            return;
        }
        if self.send_daemon_request(crate::daemon::proto::Request::Pin {
            text: text.to_string(),
        }) {
            self.history.push(HistoryEntry::Plain {
                line: format!("/pin: pinned (survives /compact verbatim): {text}"),
            });
        } else {
            self.history.push(HistoryEntry::Plain {
                line: "/pin: no daemon connection — cannot pin.".to_string(),
            });
        }
    }

    /// Attach the session eagerly once the daemon is reachable so the
    /// startup graphic can show its id (session-id-display-and-lazy-persist).
    /// The attach creates a deferred (un-persisted) session in the daemon;
    /// the first user message is what writes the `sessions` row. Runs each
    /// event-loop tick.
    ///
    /// Gates (all must hold):
    /// - No live runner yet. A successful attach (`Some(Ok)`) stops the
    ///   eager loop; a poisoned `Some(Err)` from a *previous first-message*
    ///   attempt would too, so this also short-circuits then — only the
    ///   `None` state retries here.
    /// - The "daemon not running" prompt is closed — we don't spawn a
    ///   daemon out from under the user's choice.
    /// - Not daemonless. In daemonless mode there is no daemon to merely
    ///   *show* an id for; eager-attaching would spawn the owned ephemeral
    ///   daemon purely for display. The short id appears once a daemon comes
    ///   up on its own (the first message). `daemon_connected` stays true in
    ///   that mode (the `/sessions` pane needs it), so it can't be the gate.
    /// - The canonical daemon is actually reachable *right now*. After
    ///   "Start and connect" the just-spawned socket isn't bound for a beat;
    ///   attaching then would either block the loop on `wait_for_daemon` or
    ///   race a second auto-promoted daemon onto the same socket. A cheap
    ///   probe lets us wait quietly and attach the instant it's up.
    pub(super) fn ensure_session_for_display(&mut self) {
        // Evaluate the cheap struct-only gates first; the daemon probe is
        // the only costly check, so only run it when everything else already
        // permits an attach (`probe_when` is lazy for exactly this reason).
        let attach = should_attempt_display_attach(
            self.agent_runner.is_some(),
            self.daemon_prompt.is_some(),
            self.daemonless,
            self.daemon_connected,
            || self.canonical_daemon_running(),
        );
        if attach {
            self.try_attach_for_display();
        }
    }

    /// Cheap "is the canonical daemon answering right now?" probe, used to
    /// gate the eager display attach so it never fires against a socket that
    /// isn't bound yet (the "Start and connect" startup gap). Any resolution
    /// or probe failure reads as "not reachable" — we simply retry next tick.
    fn canonical_daemon_running(&self) -> bool {
        crate::daemon::DaemonPaths::resolve()
            .map(|paths| {
                matches!(
                    crate::daemon::probe_blocking(&paths),
                    crate::daemon::DaemonStatus::Running
                )
            })
            .unwrap_or(false)
    }

    /// The daemon lifecycle this TUI attaches with. Daemonless mode owns a
    /// fresh per-pid ephemeral daemon (`AlwaysEphemeral`); otherwise the TUI
    /// attaches to the canonical daemon, auto-promoting a persistent one if
    /// none is running.
    pub(super) fn lifecycle_mode(&self) -> crate::daemon::client::LifecycleMode {
        if self.daemonless {
            // First attach spawns our owned per-pid ephemeral daemon; later
            // re-attaches (`/compact`, `/sessions` resume, `/new`) reconnect
            // to that same daemon instead of spawning a second one.
            crate::daemon::client::LifecycleMode::AttachOwnEphemeral
        } else {
            crate::daemon::client::LifecycleMode::AttachOrAutoPromote
        }
    }

    /// Build the ephemeral-daemon ownership guard (and arm its signal
    /// handler) for a runner that just spawned an owned daemon. No-op when
    /// the runner attached to a daemon we don't own or a guard already
    /// exists. The signal handler hands control back to the TUI's own
    /// restore path on SIGINT/SIGTERM rather than `exit`ing outright, so the
    /// alt-screen teardown still runs.
    fn arm_daemon_guard(&mut self, runner: &AgentRunner) {
        if !runner.owns_daemon || self.daemon_guard.is_some() {
            return;
        }
        let guard =
            crate::daemon::ephemeral_guard::EphemeralDaemonGuard::new(runner.socket.clone());
        self.daemon_signal_task =
            crate::daemon::ephemeral_guard::spawn_signal_shutdown(Some(&guard), false);
        self.daemon_guard = Some(guard);
    }

    /// Spawn (or attach to) the daemon and **latch** the result —
    /// including a failure. The first-message path
    /// (`src/tui/app/input.rs`) calls this: a user-initiated submit must
    /// surface a spawn error in history, and storing `Some(Err)` keeps it
    /// visible. The opportunistic display attach uses
    /// [`Self::try_attach_for_display`] instead, which never latches an
    /// error.
    pub(super) fn ensure_agent_runner(&mut self) {
        if matches!(self.agent_runner, Some(Ok(_))) {
            return;
        }
        let runner =
            agent_runner::try_spawn(&self.launch.cwd, self.no_sandbox, self.lifecycle_mode());
        self.adopt_runner(runner);
    }

    /// Adopt a freshly-spawned runner: on success, record its identity
    /// (session id + short id for the startup graphic), seed the usage
    /// tallies, flush buffered usage records, and refresh the guidance
    /// estimate from the now-live daemon. Always stores the result (`Ok`
    /// or `Err`) so the caller's latch semantics hold. Shared by the
    /// first-message path and the eager display attach.
    fn adopt_runner(&mut self, runner: Result<AgentRunner, String>) {
        if let Ok(r) = &runner {
            // In daemonless mode this runner spawned our own ephemeral
            // daemon; arm the ownership guard so it's reaped on exit.
            self.arm_daemon_guard(r);
            // Record the daemon-assigned session id so the startup graphic
            // shows it and `/new` re-renders with the fresh one
            // (session-id-display-and-lazy-persist).
            self.launch.session_id = Some(r.session_id);
            self.launch.session_short_id = Some(r.short_id.clone());
            // Seed the in-memory tally from the daemon's authoritative
            // counts. Additive: any optimistic increments made before
            // attach (held in the maps) stay on top of the historical
            // counts; the daemon's value isn't double-counted because we
            // only fetch once per session.
            merge_counts(&mut self.usage_models, &r.usage.models);
            merge_counts(&mut self.usage_slash, &r.usage.slash);
            merge_counts(&mut self.usage_tags, &r.usage.tags);
            self.project_id = Some(r.project_id.clone());
            // Flush records buffered before the runner existed,
            // backfilling tag project ids now that we know the project.
            let pid = self.project_id.clone();
            for mut req in std::mem::take(&mut self.pending_usage) {
                if let crate::daemon::proto::Request::RecordUsage {
                    kind: crate::daemon::proto::UsageKind::Tag,
                    project_id,
                    ..
                } = &mut req
                    && project_id.is_none()
                {
                    *project_id = pid.clone();
                }
                let _ = r.record_tx.try_send(req);
            }
            // Refresh the fresh-chat guidance estimate from the daemon now
            // that one is guaranteed up (lazy spawn / attach just completed).
            // The launch-time figure was a local raw-cl100k fallback computed
            // before any daemon existed; the daemon answers with the active
            // model's calibrated tokenizer and the same file-resolution the
            // engine then injects, so the indicator matches what's actually
            // sent. Best-effort: a daemon that can't answer leaves the
            // launch-time estimate in place (no regression). Targets the
            // runner's own socket so it reaches an owned per-pid ephemeral
            // daemon (daemonless / auto-spawn), not just the canonical one —
            // reuses the just-established daemon, no new spawn, one request.
            self.refresh_guidance_estimate_from_daemon(&r.socket);
        }
        self.agent_runner = Some(runner);
    }

    /// Opportunistic display attach: attach a deferred session so the
    /// welcome box can show its short id before the first message, but —
    /// unlike [`Self::ensure_agent_runner`] — **never latch a failure**. A
    /// transient `try_spawn` error (e.g. the just-started daemon's socket
    /// isn't bound yet) leaves `agent_runner = None` so the next event-loop
    /// tick retries, rather than poisoning the runner to `Some(Err)` and
    /// permanently disabling the eager display. On success the runner is
    /// the same one the first-message path then reuses (it early-returns on
    /// `is_some()`), so the id shown in the welcome box is exactly the
    /// session persisted on first message.
    fn try_attach_for_display(&mut self) {
        let runner =
            agent_runner::try_spawn(&self.launch.cwd, self.no_sandbox, self.lifecycle_mode());
        if runner.is_ok() {
            self.adopt_runner(runner);
        }
        // On `Err`, drop it silently: leave `agent_runner` as `None` so a
        // later tick can retry once the daemon is actually reachable.
    }

    /// Re-fetch the fresh-chat guidance estimate from the daemon at `socket`
    /// (the attached runner's own socket) and adopt it when it carries a
    /// resolved file or a non-zero system-prompt size. Called once the lazy
    /// daemon spawn/attach completes so the indicator reflects the daemon's
    /// calibrated figure rather than staying stuck on the launch-time local
    /// fallback (which is computed before any daemon exists). A daemon that
    /// can't answer, or a degenerate all-zero/no-file reply, is ignored so a
    /// transient miss never blanks a correct local estimate. Touches only the
    /// indicator — never the cached system prompt — so the prompt cache is
    /// undisturbed.
    fn refresh_guidance_estimate_from_daemon(&mut self, socket: &Path) {
        let (provider, model) = match &self.launch.active_model {
            Some((p, m)) => (Some(p.clone()), Some(m.clone())),
            None => (None, None),
        };
        let resp = agent_runner::daemon_request_at_blocking(
            socket,
            crate::daemon::proto::Request::GuidanceEstimate {
                project_root: self.launch.cwd.to_string_lossy().into_owned(),
                provider,
                model,
            },
        );
        if let Ok(crate::daemon::proto::Response::GuidanceEstimate {
            file,
            tokens,
            system_tokens,
        }) = resp
            && (file.is_some() || system_tokens > 0)
        {
            self.guidance_estimate = Some(agent_runner::GuidanceEstimate {
                file,
                guidance_tokens: tokens,
                system_tokens,
            });
        }
    }

    /// Record one accepted autocomplete pick: bump the in-memory count
    /// optimistically (so the current session reflects it without a
    /// round-trip) and forward it to the daemon, buffering until the
    /// runner exists.
    pub(super) fn record_usage(
        &mut self,
        kind: crate::daemon::proto::UsageKind,
        key: String,
        project_id: Option<String>,
    ) {
        use crate::daemon::proto::UsageKind;
        let map = match kind {
            UsageKind::Model => &mut self.usage_models,
            UsageKind::Slash => &mut self.usage_slash,
            UsageKind::Tag => &mut self.usage_tags,
        };
        *map.entry(key.clone()).or_insert(0) += 1;
        let req = crate::daemon::proto::Request::RecordUsage {
            kind,
            key,
            project_id,
        };
        match self.agent_runner.as_ref() {
            Some(Ok(runner)) => {
                let _ = runner.record_tx.try_send(req);
            }
            _ => self.pending_usage.push(req),
        }
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
            TurnEvent::Reconnecting { agent: _, attempt } => {
                // A network/transient failure is being auto-retried.
                // Surface a non-blocking status; ensure the working span
                // is live so the indicator row is shown even if we
                // attached mid-retry.
                if !self.busy {
                    self.begin_working_span();
                }
                self.reconnect_attempt = Some(attempt);
            }
            TurnEvent::ThinkingStarted { agent } => {
                // A (re)started round-trip clears any reconnect status —
                // the call is live again.
                self.reconnect_attempt = None;
                // Rising-edge fallback: a fresh submit normally starts
                // the span, but if we missed that (e.g. attached to an
                // already-running session) begin one here so the
                // indicator still shows.
                if !self.busy {
                    self.begin_working_span();
                }
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
                // Output is flowing — the retry (if any) reconnected.
                self.reconnect_attempt = None;
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
            TurnEvent::AssistantText { text, .. } => {
                if let Some(p) = &mut self.pending {
                    // Mark text-start (non-streaming providers land here
                    // without ever emitting a Delta).
                    if p.text_started_at.is_none() {
                        p.text_started_at = Some(Instant::now());
                    }
                    // The engine's finalizing text is the authoritative
                    // user-facing form: identical to the streamed accumulation
                    // on the common path, but the *translated* answer when
                    // round-trip translation is active
                    // (`prompts/utility-translation.md`, no streaming
                    // translation — the translated text lands here, once, on
                    // finalize). Adopt it when it differs so the frozen row
                    // shows the translation rather than the streamed
                    // model-language text. Empty event text (text-only
                    // reasoning turns) keeps the streamed accumulation.
                    if !text.trim().is_empty() && text != p.text {
                        p.text = text;
                    }
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
            TurnEvent::SubagentSpawned { parent, child, .. } => {
                // One live line: `{parent} delegated to {child}… (elapsed)`.
                // The prompt preview is intentionally dropped (the running
                // line shows no prompt text). The elapsed clock and animated
                // ellipses are derived at render time from `spawned_at`,
                // reusing the working-span tick.
                self.finalize_pending();
                self.history.push(HistoryEntry::Subagent {
                    parent,
                    child,
                    spawned_at: Instant::now(),
                    outcome: None,
                    expanded: false,
                });
            }
            TurnEvent::SubagentReport { agent, report } => {
                self.settle_subagent(&agent, report);
            }
            TurnEvent::Usage { usage, .. } => {
                self.last_usage = Some(usage);
                // Re-anchor the live counter: the provider's fresh total
                // becomes the baseline and the local streamed-token delta
                // resets to zero. `pending` still holds this round's
                // assistant turn here (Usage is emitted before the
                // finalizing `AssistantText`), so the snapshot already
                // accounts for it.
                self.estimate_at_last_usage = self.estimate_context_tokens();
            }
            TurnEvent::AgentIdle => {
                self.reconnect_attempt = None;
                self.finalize_pending();
                self.end_working_span();
                // A new agent turn has ended: a prediction now belongs to
                // this fresh turn. Bump the turn id (invalidates any
                // in-flight or cached prior-turn prediction) and kick off
                // the eager prediction for the next user message.
                self.prediction_state.begin_turn();
                self.spawn_prediction();
            }
            TurnEvent::PrimarySwapped { name } => {
                // The primary (root-frame) agent was swapped (`/plan` ↔
                // `/build`). Reflect it in the chrome's active-agent slot.
                // The daemon path also tracks this off the runner's
                // `PrimarySwapped` → `update_active_agent`; this arm keeps
                // `apply_event` exhaustive and covers any in-process path.
                self.launch.agent_name = name;
            }
            TurnEvent::LlmModeChanged { mode } => {
                // The live `/llm-mode` switch landed (daemon-authoritative).
                // Track it so the next toggle + cache-break warning resolve
                // against the true value, and confirm it in the history.
                self.llm_mode = mode;
                self.history.push(HistoryEntry::Plain {
                    line: format!("Switched to `{}` LLM mode", mode.as_str()),
                });
            }
            TurnEvent::InterruptRaised {
                interrupt_id,
                description,
                questions,
            } => {
                // A `question` tool blocked the agent (GOALS §3b). Open
                // the answering dialog over the composer. The
                // anti-misfire lockout uses the configured delay. If a
                // dialog is somehow already open (re-raise), the newest
                // one wins — the prior interrupt stays parked in the DB.
                let lockout =
                    Duration::from_millis(load_dialog_config(&self.launch.cwd).lockout_ms);
                self.question_dialog = Some(crate::tui::dialog::question::QuestionDialog::new(
                    interrupt_id,
                    description,
                    questions,
                    lockout,
                ));
            }
            TurnEvent::JobStarted {
                session_id,
                job_id,
                label,
                kind,
            } => {
                self.active_jobs.insert(
                    job_id.clone(),
                    ActiveJob {
                        session_id,
                        label: label.clone(),
                        kind,
                        iteration: 0,
                        last_activity: Instant::now(),
                    },
                );
                self.history.push(HistoryEntry::Plain {
                    line: format!("[job {job_id}] started: {label}"),
                });
            }
            TurnEvent::JobProgress { job_id } => {
                if let Some(j) = self.active_jobs.get_mut(&job_id) {
                    j.last_activity = Instant::now();
                }
            }
            TurnEvent::JobNote { job_id, text } => {
                if let Some(j) = self.active_jobs.get_mut(&job_id) {
                    j.iteration = j.iteration.saturating_add(1);
                    j.last_activity = Instant::now();
                }
                self.finalize_pending();
                self.history.push(HistoryEntry::Plain {
                    line: format!("[job {job_id} note] {text}"),
                });
            }
            TurnEvent::Notice { text } => {
                // Non-blocking system notice (prompt-injection warn chip,
                // GOALS §4i). UI-only — never enters model context.
                self.finalize_pending();
                self.history.push(HistoryEntry::Plain {
                    line: format!("⚠ {text}"),
                });
            }
            TurnEvent::JobCompleted {
                job_id,
                label,
                kind,
                failed,
            } => {
                self.active_jobs.remove(&job_id);
                self.finalize_pending();
                let verb = if failed { "failed" } else { "ended" };
                self.history.push(HistoryEntry::Plain {
                    line: format!("[job {job_id}] {kind} {verb}: {label}"),
                });
            }
            TurnEvent::ContextProjection {
                prunable_tokens,
                cache_cold,
            } => {
                // Authoritative "% prunable" basis. Stored, then rendered
                // by `context_indicator_text` against the model's max
                // context (GOALS §1a). `cache_cold` drives the /prune
                // confirm's hot-vs-cold copy.
                self.prunable_tokens = prunable_tokens;
                self.cache_cold = cache_cold;
            }
            TurnEvent::Pruned {
                auto,
                bodies,
                tokens_saved,
                elided,
                cache_break,
            } => {
                self.finalize_pending();
                // Replace the live elided set wholesale (it's the full
                // current wire-side set, not a delta) so scrollback dims
                // exactly what's out of the model's context now. Reversible:
                // an engine fallback that un-elides a body drops it here, so
                // it renders normally again.
                self.elided_event_ids = elided.into_iter().collect();
                let how = if auto { "auto-pruned" } else { "/prune" };
                let line = if bodies == 0 {
                    format!("{how}: nothing to do (0% prunable)")
                } else {
                    format!(
                        "{how}: collapsed {bodies} superseded snapshot{} (~{tokens_saved} wire tokens reclaimed)",
                        if bodies == 1 { "" } else { "s" }
                    )
                };
                self.history.push(HistoryEntry::Plain { line });
                // A ctx%-threshold auto-prune broke a warm cache to reclaim
                // context — surface the shared cache-break warning (suppressed
                // on a no-cache provider by the helper).
                if cache_break && let Some(warning) = self.cache_break_warning() {
                    self.history.push(HistoryEntry::Plain { line: warning });
                }
            }
            TurnEvent::CompactReady {
                new_session_id,
                handoff,
                seed_tool_count,
                seed_tool_tokens,
            } => {
                self.finalize_pending();
                // Review-then-commit (T6.e step 4/5): drop the assembled
                // handoff into the composer for the user to edit/append.
                // On submit, the TUI re-attaches to the new session and
                // sends the (edited) handoff as the first message; the
                // new session re-executes its seed tools first.
                self.composer.set(handoff);
                self.paste_registry.clear();
                // Capture the predecessor (current) session's short id now,
                // before the commit re-attaches onto the fresh session and
                // the runner's short id changes.
                let predecessor_short_id = match self.agent_runner.as_ref() {
                    Some(Ok(r)) => r.short_id.clone(),
                    _ => String::new(),
                };
                self.pending_compact = Some(PendingCompact {
                    new_session_id,
                    seed_tool_count,
                    seed_tool_tokens,
                    predecessor_short_id,
                });
                self.history.push(HistoryEntry::Plain {
                    line: format!(
                        "/compact: handoff ready for review in the composer — {seed_tool_count} seed tool(s), ~{seed_tool_tokens} tokens will re-run in the fresh session. Edit and submit to commit; the old session stays recoverable.",
                    ),
                });
            }
            TurnEvent::SandboxState { enabled } => {
                // `/sandbox` result (sandboxing part 2): surface the
                // resulting on/off state as a toast.
                self.show_toast(
                    if enabled { "sandbox on" } else { "sandbox off" },
                    ToastKind::Info,
                );
            }
            TurnEvent::CaffeinateState {
                active,
                lid_close_guaranteed,
                message,
            } => {
                // Daemon-global: always update the ☕ glyph state so every
                // client stays in sync (incl. until-idle auto-off). Only
                // the originating client gets a `message` → toast; a
                // not-guaranteed lid-close (or missing mechanism) makes the
                // toast a warning so the honest note reads as a caveat.
                self.caffeinate_active = active;
                if let Some(message) = message {
                    let kind = if active && !lid_close_guaranteed {
                        ToastKind::Error
                    } else {
                        ToastKind::Info
                    };
                    self.show_toast(message, kind);
                }
            }
            TurnEvent::PlanStatusState {
                project_id,
                ready,
                in_progress,
                interruptions,
            } => {
                // Daemon-global but project-scoped: apply only when it's our
                // own project (the event carries `project_id` so one bus can
                // serve every client). A TUI not yet attached has no
                // `project_id`; it picks up the state on its next attach-sync.
                if self.project_id.as_deref() == Some(project_id.as_str()) {
                    self.plan_status = crate::db::plans::PlanStatusCounts {
                        ready,
                        in_progress,
                        interruptions,
                    };
                }
            }
            TurnEvent::DaemonDraining { forced } => {
                // Daemon-global drain notice
                // (`daemon-graceful-drain-shutdown.md`). Flip the flag so the
                // composer refuses new submissions, and surface a toast. The
                // `forced` escalation reads as a warning so a truncated turn
                // isn't mistaken for a clean finish.
                self.daemon_draining = true;
                if forced {
                    self.show_toast(
                        "daemon shutdown forced — in-flight work was aborted",
                        ToastKind::Error,
                    );
                } else {
                    self.show_toast("finishing in-flight work, shutting down…", ToastKind::Info);
                }
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

    /// Begin a fresh working span: mark the agent busy, (re)start the
    /// cumulative span clock, and re-roll the playful working message.
    /// Called on a brand-new submit and as a fallback on the first
    /// `ThinkingStarted` of a span we didn't originate (e.g. attaching
    /// to an already-running session).
    pub(super) fn begin_working_span(&mut self) {
        self.busy = true;
        self.span_started_at = Some(Instant::now());
        self.working_msg_idx = pick_working_msg(self.working_msg_idx);
    }

    /// End the working span: the agent yielded control back to the
    /// human. Clears the indicator (via `busy`) and freezes the clock.
    pub(super) fn end_working_span(&mut self) {
        self.busy = false;
        self.span_started_at = None;
    }

    /// Settle the most-recent still-running [`HistoryEntry::Subagent`]
    /// for `child` with its report: freeze the elapsed clock into the
    /// total duration and replace the live `delegated to…` line with the
    /// `worked for {duration}` (or `failed after`) header + response.
    pub(super) fn settle_subagent(&mut self, child: &str, report: String) {
        settle_subagent_in(&mut self.history, child, report);
    }

    /// True while the current inference round is in its reasoning phase:
    /// we've received reasoning content and not yet any assistant text.
    /// Keyed off accumulated reasoning (not `ThinkingStarted`, which
    /// fires for every round including non-thinking models), so a model
    /// that emits no reasoning never flips the indicator to yellow.
    pub(super) fn in_thinking_block(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| !p.reasoning.trim().is_empty() && p.text_started_at.is_none())
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

    /// `/copy [format]` — copy the last assistant response (message text,
    /// excluding tool-call chrome) to the system clipboard. Default
    /// format is `markdown` (the raw response verbatim); `plain` strips
    /// the markdown; `rich` copies HTML. Mirrors the context-menu copy
    /// path (`execute_context_menu_action`) and reuses the clipboard
    /// module. Surfaces feedback via a toast.
    pub(super) fn handle_copy_command(&mut self, arg: &str) {
        let format = match parse_copy_format(arg) {
            Some(f) => f,
            None => {
                self.show_toast(
                    "Usage: `/copy [markdown|plain|rich]` (markdown is the default)",
                    ToastKind::Info,
                );
                return;
            }
        };
        let Some(text) = last_agent_text(&self.history) else {
            self.show_toast("No response to copy yet.", ToastKind::Info);
            return;
        };
        let (msg, kind) = match format {
            CopyFormat::Markdown => match crate::clipboard::copy_plain(&text) {
                Ok(()) => (
                    "Copied last response (markdown).".to_string(),
                    ToastKind::Success,
                ),
                Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
            },
            CopyFormat::Plain => {
                let plain = crate::clipboard::markdown_to_plain(&text);
                match crate::clipboard::copy_plain(&plain) {
                    Ok(()) => (
                        "Copied last response (plain).".to_string(),
                        ToastKind::Success,
                    ),
                    Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
                }
            }
            CopyFormat::Rich => {
                let html = crate::clipboard::markdown_to_html(&text);
                match crate::clipboard::copy_rich(&text, &html) {
                    Ok(()) => (
                        "Copied last response (rich).".to_string(),
                        ToastKind::Success,
                    ),
                    Err(crate::clipboard::CopyError::UnsupportedOverSsh) => {
                        // No multi-format clipboard pathway over SSH —
                        // fall back to plain so `/copy rich` never
                        // silently does nothing, and say why.
                        match crate::clipboard::copy_plain(&text) {
                            Ok(()) => (
                                "SSH — copied last response as plain text \
                                 (rich copy unavailable over SSH)."
                                    .to_string(),
                                ToastKind::Success,
                            ),
                            Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
                        }
                    }
                    Err(e) => (format!("Copy failed: {e}"), ToastKind::Error),
                }
            }
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
        // `/stats` pane is a full-body overlay: wheel scrolls it, every
        // other mouse event is eaten so nothing reaches the chat
        // underneath. Ahead of the embedded-pane / chat handlers.
        if let Some(pane) = self.stats_pane.as_mut() {
            match mouse.kind {
                MouseEventKind::ScrollUp => pane.scroll_up(),
                MouseEventKind::ScrollDown => pane.scroll_down(),
                _ => {}
            }
            return;
        }
        if let Some(pane) = self.sessions_pane.as_mut() {
            match mouse.kind {
                MouseEventKind::ScrollUp => pane.scroll_up(),
                MouseEventKind::ScrollDown => pane.scroll_down(),
                _ => {}
            }
            return;
        }
        // `/skills` overlay: same full-body wheel-scroll / eat-everything-
        // else rule as the other informational panes.
        if let Some(pane) = self.skills_pane.as_mut() {
            match mouse.kind {
                MouseEventKind::ScrollUp => pane.scroll_up(),
                MouseEventKind::ScrollDown => pane.scroll_down(),
                _ => {}
            }
            return;
        }
        // `/plans` overlay: same full-body wheel-scroll / eat-everything-
        // else rule as the other informational panes.
        if let Some(pane) = self.plans_pane.as_mut() {
            match mouse.kind {
                MouseEventKind::ScrollUp => pane.scroll_up(),
                MouseEventKind::ScrollDown => pane.scroll_down(),
                _ => {}
            }
            return;
        }
        // `/permissions` overlay: same full-body wheel-scroll / eat-
        // everything-else rule as the other informational panes.
        if let Some(pane) = self.permissions_pane.as_mut() {
            match mouse.kind {
                MouseEventKind::ScrollUp => pane.scroll_up(),
                MouseEventKind::ScrollDown => pane.scroll_down(),
                _ => {}
            }
            return;
        }
        // `/context` overlay: a fixed-size snapshot (no scroll), so just
        // eat every mouse event while it's open so nothing reaches the
        // chat underneath.
        if self.context_pane.is_some() {
            return;
        }
        // Embedded pane (GOALS §1i/§1e): divider drag-resize, click-to-
        // focus, and PTY mouse forwarding. Consumes the event when it
        // lands on the divider or inside the pane so the chat handlers
        // below don't also see it.
        if self.pane.is_some() && self.handle_pane_mouse(&mouse) {
            return;
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
            match self.history.get_mut(entry_idx) {
                Some(HistoryEntry::Agent { expanded, .. })
                | Some(HistoryEntry::Subagent { expanded, .. }) => {
                    *expanded = !*expanded;
                }
                _ => {}
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

    /// Route a mouse event to the embedded pane (GOALS §1i). Returns
    /// `true` when consumed: a divider drag-resize, a click that focuses
    /// the pane, or an event forwarded to the child's PTY. Returns
    /// `false` when the event missed the pane and divider, so the chat /
    /// composer handlers below get their normal turn (split mode).
    fn handle_pane_mouse(&mut self, mouse: &MouseEvent) -> bool {
        // Continue / end an in-progress divider drag wherever the mouse
        // goes (so dragging past the divider still tracks).
        if self.dragging_divider {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.resize_split_to(mouse.column, mouse.row);
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.dragging_divider = false;
                    return true;
                }
                _ => return true,
            }
        }
        // Start a divider drag when a left-down lands on the divider.
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some((drect, _)) = self.divider
            && point_in(drect, mouse.column, mouse.row)
        {
            self.dragging_divider = true;
            return true;
        }
        // Inside the pane content rect: a click focuses it; mouse events
        // forward to the child when focused and it requested tracking.
        if let Some(prect) = self.pane_rect
            && point_in(prect, mouse.column, mouse.row)
        {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.pane_focused = true;
            }
            if self.pane_focused
                && let Some(pane) = self.pane.as_mut()
            {
                pane.forward_mouse(mouse, prect);
            }
            return true;
        }
        false
    }

    /// Recompute the split ratio from a divider drag to `(col, row)`.
    fn resize_split_to(&mut self, col: u16, row: u16) {
        let Some(body) = self.pane_body else {
            return;
        };
        let ratio = match self.pane_side {
            PaneSide::Left => col.saturating_sub(body.x) as f32 / (body.width.max(1) as f32),
            PaneSide::Right => {
                (body.x + body.width).saturating_sub(col) as f32 / (body.width.max(1) as f32)
            }
            PaneSide::Top => row.saturating_sub(body.y) as f32 / (body.height.max(1) as f32),
            PaneSide::Bottom => {
                (body.y + body.height).saturating_sub(row) as f32 / (body.height.max(1) as f32)
            }
            PaneSide::Full => return,
        };
        self.pane_ratio = ratio.clamp(0.15, 0.85);
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
        // Capture the full composer line before clearing so arg-bearing
        // commands (`/git`, `/editor`) can read their arguments.
        let raw = self.composer.text().to_string();
        self.composer.clear();
        self.paste_registry.clear();
        // The slash line is gone; reset the menu cursor so the next `/`
        // session opens on the top match.
        self.reset_slash_window();
        // Tally the pick for frequency-ranked autocomplete (global).
        self.record_usage(
            crate::daemon::proto::UsageKind::Slash,
            cmd.name.to_string(),
            None,
        );
        let msg = match cmd.name {
            "exit" => return true,
            "editor" => {
                self.open_editor(parse_pane_side(&slash_args(&raw)));
                return false;
            }
            "lazygit" => {
                self.open_lazygit();
                return false;
            }
            "git" => {
                self.run_git_command(&slash_args(&raw));
                return false;
            }
            "settings" => {
                self.dialog = Dialog::open(&self.launch.cwd);
                return false;
            }
            "model-settings" => {
                self.dialog = Dialog::open_model_settings(&self.launch.cwd);
                return false;
            }
            "fetch-models" => {
                self.spawn_fetch_models();
                return false;
            }
            "model" => {
                match crate::tui::model_picker::ModelPickerDialog::open(
                    &self.launch.cwd,
                    &self.usage_models,
                ) {
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
            "new" | "clear" => {
                self.pending_new_session = true;
                return false;
            }
            "mouse" => {
                self.toggle_mouse_capture_inline();
                return false;
            }
            "llm-mode" => {
                self.handle_llm_mode_command(&slash_args(&raw));
                return false;
            }
            "init" => {
                self.handle_init_command(&slash_args(&raw));
                return false;
            }
            "jobs" => {
                self.handle_jobs_command(&slash_args(&raw));
                return false;
            }
            "ps" => {
                self.handle_ps_command();
                return false;
            }
            "stop" => {
                self.handle_stop_command(&slash_args(&raw));
                return false;
            }
            "caffeinate" => {
                self.handle_caffeinate_command(&slash_args(&raw));
                return false;
            }
            "compact" => {
                self.start_compact();
                return false;
            }
            "copy" => {
                self.handle_copy_command(&slash_args(&raw));
                return false;
            }
            "prune" => {
                self.arm_prune_confirm();
                return false;
            }
            "pin" => {
                self.handle_pin_command(&slash_args(&raw));
                return false;
            }
            "sandbox" => {
                self.handle_sandbox_command(&slash_args(&raw));
                return false;
            }
            "stats" => {
                self.stats_pane = Some(crate::tui::stats_pane::StatsPane::open(&self.launch.cwd));
                return false;
            }
            "context" => {
                let snapshot = self.context_snapshot();
                self.context_pane = Some(crate::tui::context_pane::ContextPane::open(snapshot));
                return false;
            }
            "sessions" | "resume" => {
                // Daemon-connected → RPC list (live status intact);
                // daemonless → read-only direct-DB browse (resume/archive
                // disabled). The pane picks the path off this flag.
                self.sessions_pane = Some(crate::tui::sessions_pane::SessionsPane::open(
                    &self.launch.cwd,
                    self.daemon_connected,
                ));
                return false;
            }
            "skills" => {
                self.skills_pane =
                    Some(crate::tui::skills_pane::SkillsPane::open(&self.launch.cwd));
                return false;
            }
            "plan" => {
                self.swap_primary_agent("Plan");
                return false;
            }
            "build" => {
                self.swap_primary_agent("Build");
                return false;
            }
            "plans" => {
                // `/plans answer` opens straight into the needs-attention
                // resolver; `/plans` opens the read-only browser (the resolver
                // is reachable from it via the `a` button)
                // (`plan-status-chrome-and-resolver.md`).
                let args = slash_args(&raw);
                self.plans_pane = Some(if args.trim() == "answer" {
                    crate::tui::plans_pane::PlansPane::open_resolver(self.project_id.clone())
                } else {
                    crate::tui::plans_pane::PlansPane::open(self.project_id.clone())
                });
                return false;
            }
            "permissions" => {
                self.permissions_pane = Some(crate::tui::permissions_pane::PermissionsPane::open(
                    &self.launch.cwd,
                ));
                return false;
            }
            "fork" => {
                "/fork: stub — the ForkSession RPC is live in the daemon; the TUI \
                 re-attach flow on top of it ships in a later cut."
            }
            "side" => {
                self.handle_side_command(&slash_args(&raw));
                return false;
            }
            "rename" => {
                self.handle_rename_command(&slash_args(&raw));
                return false;
            }
            "export" => {
                self.handle_export_command(&slash_args(&raw));
                return false;
            }
            _ => return false,
        };
        self.history.push(HistoryEntry::Plain {
            line: msg.to_string(),
        });
        false
    }

    /// `/rename <title>` — rename the current session. The title is the
    /// trimmed remainder of the command line (spaces allowed); an empty
    /// title shows usage only and changes nothing. The `RenameSession`
    /// RPC sets `user_renamed = 1` in the DB, so auto-titling stops
    /// overriding the name. The sessions browser reads titles fresh from
    /// the daemon on open, so the new name shows without a restart.
    pub(super) fn handle_rename_command(&mut self, arg: &str) {
        let title = arg.trim();
        if title.is_empty() {
            self.history.push(HistoryEntry::Plain {
                line: "Usage: `/rename <title>`".to_string(),
            });
            return;
        }
        // Authoritative current session: the live runner if attached,
        // else the last-attached id tracked on launch info.
        let session_id = match self.agent_runner.as_ref() {
            Some(Ok(runner)) => Some(runner.session_id),
            _ => self.launch.session_id,
        };
        let Some(session_id) = session_id else {
            self.history.push(HistoryEntry::Plain {
                line: "/rename: no active session yet — send a message first".to_string(),
            });
            return;
        };
        let req = crate::daemon::proto::Request::RenameSession {
            session_id,
            title: title.to_string(),
        };
        match agent_runner::daemon_request_blocking(req) {
            Ok(_) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("Renamed session to `{title}`"),
                });
            }
            Err(e) => {
                self.history.push(HistoryEntry::Plain {
                    line: format!("/rename: {e}"),
                });
            }
        }
    }

    /// `/export [debug]` — export the current session into
    /// `{cwd}/.cockpit/exports/`. Default exports the live transcript as
    /// `<short_id>.json` (user-facing form, GOALS §14); `debug` exports
    /// the full CLI bundle `.zip`. Both overwrite their own prior file
    /// and surface success/failure as a chat line, never a panic.
    pub(super) fn handle_export_command(&mut self, arg: &str) {
        // Authoritative current session: the live runner if attached,
        // else the last-attached ids tracked on launch info.
        let (session_id, short_id) = match self.agent_runner.as_ref() {
            Some(Ok(runner)) => (Some(runner.session_id), Some(runner.short_id.clone())),
            _ => (self.launch.session_id, self.launch.session_short_id.clone()),
        };
        let Some(session_id) = session_id else {
            self.history.push(HistoryEntry::Plain {
                line: "/export: no active session yet — send a message first".to_string(),
            });
            return;
        };
        // `<short_id>`, falling back to the full UUID (matching the CLI's
        // `default_output_path`).
        let file_stem = short_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| session_id.to_string());
        let exports_dir = self.launch.cwd.join(".cockpit").join("exports");

        if arg.trim() == "debug" {
            self.export_debug_bundle(session_id, &file_stem, &exports_dir);
        } else {
            self.export_transcript_json(&file_stem, &exports_dir);
        }
    }

    /// `/export` (default) — write the live transcript as
    /// `<stem>.json`, overwriting any prior file.
    fn export_transcript_json(&mut self, file_stem: &str, exports_dir: &Path) {
        let out_path = exports_dir.join(format!("{file_stem}.json"));
        let result = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(exports_dir).with_context(|| {
                format!("creating export directory `{}`", exports_dir.display())
            })?;
            let value = crate::tui::history::export_transcript(&self.history);
            let json = serde_json::to_string_pretty(&value)?;
            std::fs::write(&out_path, json)
                .with_context(|| format!("writing export to `{}`", out_path.display()))?;
            Ok(())
        })();
        let line = match result {
            Ok(()) => format!("Exported conversation → {}", out_path.display()),
            Err(e) => format!("/export: {e}"),
        };
        self.history.push(HistoryEntry::Plain { line });
    }

    /// `/export debug` (hidden) — write the full CLI bundle `.zip` for
    /// the current session, overwriting any prior file. Reads the DB
    /// directly (like the CLI) so it works regardless of daemon state,
    /// reusing the single shared zip-assembly implementation.
    fn export_debug_bundle(&mut self, session_id: uuid::Uuid, file_stem: &str, exports_dir: &Path) {
        let out_path = exports_dir.join(format!("{file_stem}.zip"));
        let result = (|| -> anyhow::Result<crate::commands::export::BundleSummary> {
            let db = crate::db::Db::open_default()?;
            let target = db
                .get_session(session_id)?
                .ok_or_else(|| anyhow::anyhow!("session `{session_id}` not found in the DB"))?;
            // Unconditional overwrite (the TUI has no `--force`); this
            // does not weaken the CLI's no-clobber-without-`--force`
            // guarantee, which lives in `commands::export::run`.
            crate::commands::export::write_bundle_zip(&db, &target, &out_path, true)
        })();
        let line = match result {
            Ok(summary) => format!(
                "Exported debug bundle ({} session{}, {} bytes) → {}",
                summary.session_count,
                if summary.session_count == 1 { "" } else { "s" },
                summary.byte_len,
                out_path.display()
            ),
            Err(e) => format!("/export debug: {e}"),
        };
        self.history.push(HistoryEntry::Plain { line });
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
        // The predict-next-message setting lives at the extended-config
        // root (not in `tui`); reload it so a `/settings` change takes
        // effect on subsequent turns. Turning it `off` also drops any
        // pending ghost/cache immediately.
        let predict_setting =
            crate::config::extended::load_for_cwd(&self.launch.cwd).predict_next_message;
        self.predict_setting = predict_setting;
        if !predict_setting.is_enabled() {
            self.prediction_state.clear();
        }
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

/// Playful "agent is working" lines. The animated, width-3-padded
/// ellipsis is appended at render time, so these carry no trailing
/// `...`. One is held per span (see [`App::begin_working_span`]).
pub(super) const WORKING_MESSAGES: &[&str] = &[
    "Working",
    "Slaving away",
    "Hard at work",
    "Why don't you play a game",
    "I bet you don't even read these",
    "Go make a coffee",
    "Go play Minecraft",
    "Still here, huh",
    "When will I ever be free",
    "Boiling the ocean",
    "You can't afford the GPU I'm on",
    "I'm not like other harnesses",
    "Putting on aviators",
    "Talk to me, Goose",
    "I was created by a genius",
    "Taking your job",
    "Doing your job for you",
    "Fighting demons",
    "Happily helping",
    "Touching grass",
    "I am the permanent underclass",
    "I'll never give you up",
    "I'll never let you down",
    "Of course I still love you",
    "Why don't you flirt with me",
    "I've got a bad feeling about this",
    "Still flying half a ship",
    "You were the chosen one",
    "Running away",
    "Hi, Neo",
    "Doo doo doo",
    "My team is better than yours",
    "Read The Count of Monte Cristo",
    "Read The Great Gatsby",
    "Read the Bible",
    "Wasting tokens",
    "Call your mom",
    "Call your dad",
    "Call your friend",
    "Plan a party",
];

/// Add the daemon's authoritative counts into the in-memory tally.
/// Additive (not replace) so optimistic pre-attach increments survive;
/// safe because the daemon is only queried once per session.
fn merge_counts(local: &mut HashMap<String, u64>, server: &HashMap<String, u64>) {
    for (key, count) in server {
        *local.entry(key.clone()).or_insert(0) += *count;
    }
}

/// Pick a random index into [`WORKING_MESSAGES`], avoiding `prev` so
/// the line visibly changes between consecutive spans. A `prev` that's
/// out of range (the initial one-past-end sentinel) lets the first
/// roll land anywhere.
fn pick_working_msg(prev: usize) -> usize {
    use rand::RngExt;
    let n = WORKING_MESSAGES.len();
    if n <= 1 {
        return 0;
    }
    let mut rng = rand::rng();
    loop {
        let idx = rng.random_range(0..n);
        if idx != prev {
            return idx;
        }
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

/// Max output lines shown in chat for `!` / `/git` before truncation
/// with a "re-run in a real terminal" note (GOALS §1k).
const LOCAL_CMD_DISPLAY_LINES: usize = 100;
/// Token cap for the agent-bound `<git>` block (GOALS §1l, §10).
const GIT_AGENT_TOKEN_CAP: usize = 2000;

/// True when `(col, row)` falls inside `rect` (absolute coords).
fn point_in(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Map a `/editor` argument to a pane side. Empty / unknown → fullscreen.
pub(super) fn parse_pane_side(arg: &str) -> PaneSide {
    match arg.trim().to_ascii_lowercase().as_str() {
        "left" => PaneSide::Left,
        "right" => PaneSide::Right,
        "top" | "up" => PaneSide::Top,
        "bottom" | "down" => PaneSide::Bottom,
        _ => PaneSide::Full,
    }
}

/// Parse a `/sandbox` argument (sandboxing part 2) into the
/// `SetSandbox.enabled` value: `""` (no arg) toggles (`None`), `on` /
/// `off` set explicitly. `Err(arg)` for anything else.
fn parse_sandbox_arg(args: &str) -> Result<Option<bool>, String> {
    match args.trim().to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "on" => Ok(Some(true)),
        "off" => Ok(Some(false)),
        other => Err(other.to_string()),
    }
}

/// Extract the argument string from a full slash line. The command
/// token (whatever was typed before the first space) is dropped; the
/// remainder is the args. `/git status` → `status`; `/git` → ``.
/// Output format for `/copy`. `Markdown` keeps the raw response text
/// verbatim; `Plain` strips markdown; `Rich` copies HTML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyFormat {
    Markdown,
    Plain,
    Rich,
}

/// Parse the `/copy` format argument. An empty argument defaults to
/// `Markdown` (bare `/copy`). Returns `None` for an unrecognized
/// argument so the caller can show usage.
fn parse_copy_format(arg: &str) -> Option<CopyFormat> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "" | "markdown" => Some(CopyFormat::Markdown),
        "plain" | "plaintext" => Some(CopyFormat::Plain),
        "rich" | "richtext" => Some(CopyFormat::Rich),
        _ => None,
    }
}

/// The text of the last assistant response in `history`, excluding
/// tool-call chrome (tool calls are non-`Agent` history variants).
/// `None` when no assistant message with text exists yet. Mirrors the
/// extraction in `agent_message_at_or_before` /
/// `copy_last_agent_message_as_rich_text`.
fn last_agent_text(history: &[HistoryEntry]) -> Option<String> {
    history.iter().rev().find_map(|e| match e {
        HistoryEntry::Agent { text, .. } if !text.trim().is_empty() => Some(text.clone()),
        _ => None,
    })
}

/// Reduce the visible transcript to the prediction input
/// (`prompts/predict-next-message.md`): one (user, agent-final-response)
/// pair per turn, with tool calls / diffs / subagent reports / notices /
/// reasoning skipped — only [`HistoryEntry::User`] + [`HistoryEntry::Agent`]
/// carry into a turn, and the agent's `reasoning` is never included. A user
/// message opens a turn; the next agent message closes it; a user message
/// arriving before the agent reply folds into the open turn so the
/// one-pair-per-turn shape (and the last-3 window) stays faithful. Pure +
/// deterministic so the assembly is unit-testable without an `App`.
fn turns_from_history(history: &[HistoryEntry]) -> Vec<crate::engine::predict::PredictionTurn> {
    use crate::engine::predict::PredictionTurn;
    let mut turns: Vec<PredictionTurn> = Vec::new();
    // True when the last pushed turn is still awaiting its agent reply (so a
    // following user message folds rather than opening a new one).
    let mut open = false;
    for entry in history {
        match entry {
            HistoryEntry::User { text, .. } => {
                if open {
                    if let Some(last) = turns.last_mut() {
                        last.user.push_str("\n\n");
                        last.user.push_str(text);
                    }
                } else {
                    turns.push(PredictionTurn {
                        user: text.clone(),
                        agent: String::new(),
                    });
                    open = true;
                }
            }
            HistoryEntry::Agent { text, .. } => {
                if let Some(last) = turns.last_mut() {
                    // Fold multiple agent messages (rare: tool rounds can
                    // finalize text more than once) into one final response
                    // so the pairing stays one-per-turn.
                    if last.agent.is_empty() {
                        last.agent = text.clone();
                    } else {
                        last.agent.push('\n');
                        last.agent.push_str(text);
                    }
                    open = false;
                }
            }
            _ => {}
        }
    }
    turns
}

/// Job ids in `jobs` owned by `session_id`, in map (stable, job-id)
/// order. The pure core of `/ps` / `/stop` scoping — the list, the
/// cancel set, and the bare-`/stop` confirm count all read from here so
/// they can't disagree, and it filters strictly to `session_id` so
/// neither command ever touches another session's jobs.
fn session_job_ids(
    jobs: &std::collections::BTreeMap<String, ActiveJob>,
    session_id: uuid::Uuid,
) -> Vec<String> {
    jobs.iter()
        .filter(|(_, j)| j.session_id == session_id)
        .map(|(id, _)| id.clone())
        .collect()
}

/// The per-job core line shared by `/jobs` and `/ps`: `job-id [kind]`,
/// the iteration count for loop/timer jobs, and the label. Each caller
/// appends its own cancel/stop hint.
fn format_job_line(job_id: &str, j: &ActiveJob) -> String {
    let progress = if j.kind == "background" {
        String::new()
    } else {
        format!(" {} iter", j.iteration)
    };
    format!("{job_id} [{}]{progress}  {}", j.kind, j.label)
}

fn slash_args(raw: &str) -> String {
    let rest = raw.strip_prefix('/').unwrap_or(raw);
    match rest.find(char::is_whitespace) {
        Some(idx) => rest[idx..].trim().to_string(),
        None => String::new(),
    }
}

/// Whether a resolved [`crate::config::providers::CacheConfig`] means the
/// provider/model actually caches. Reuses the pruning-policy no-cache
/// predicate ([`crate::engine::prune::cache_state`]): the only way it
/// reports [`crate::engine::prune::ColdReason::NoCacheProvider`] for a
/// freshly-sent, non-busting prefix is `cache.mode = none`. Pure over its
/// input so the cache-break-warning suppression is unit-testable without
/// constructing an `App`.
fn cache_config_caches(cache: &crate::config::providers::CacheConfig) -> bool {
    use crate::engine::prune::{CacheState, ColdReason, cache_state};
    !matches!(
        cache_state(cache, Some(0), false),
        CacheState::Cold(ColdReason::NoCacheProvider)
    )
}

/// Parse the `/llm-mode` argument (`prompts/llm-modes-defensive-normal.md`).
/// Returns `Ok(None)` for the toggle action (no argument or `toggle`),
/// `Ok(Some(mode))` for an explicit target, or `Err(usage)` for an
/// unrecognized argument. `defend` is the advertised short form for
/// defensive; `defensive` is accepted as a silent alias.
fn parse_llm_mode_arg(arg: &str) -> Result<Option<crate::config::extended::LlmMode>, String> {
    use crate::config::extended::LlmMode;
    match arg.trim().to_ascii_lowercase().as_str() {
        "" | "toggle" => Ok(None),
        "defend" | "defensive" => Ok(Some(LlmMode::Defensive)),
        "normal" => Ok(Some(LlmMode::Normal)),
        other => Err(format!(
            "Usage: `/llm-mode [toggle|defend|normal]` (got `{other}`)"
        )),
    }
}

/// Run a one-shot shell command, capturing stdout+stderr. Returns
/// `(combined_output, failed)`. Cross-platform: `cmd /C` on Windows,
/// `$SHELL -c` (fallback `/bin/sh`) elsewhere.
fn exec_capture_shell(cmd: &str, cwd: &Path) -> (String, bool) {
    let mut command;
    #[cfg(windows)]
    {
        command = std::process::Command::new("cmd");
        command.arg("/C").arg(cmd);
    }
    #[cfg(not(windows))]
    {
        let shell =
            std::env::var_os("SHELL").unwrap_or_else(|| std::ffi::OsString::from("/bin/sh"));
        command = std::process::Command::new(shell);
        command.arg("-c").arg(cmd);
    }
    command.current_dir(cwd);
    run_capture(command)
}

/// Run `git --no-pager <args>` with the pager disabled and prompts off,
/// capturing stdout+stderr. Returns `(combined_output, failed)`.
fn exec_capture_git(args: &str, cwd: &Path) -> (String, bool) {
    let mut command = std::process::Command::new("git");
    command.arg("--no-pager");
    for a in crate::tui::pty::shell_split(args) {
        command.arg(a);
    }
    command.current_dir(cwd);
    command.env("GIT_PAGER", "cat");
    command.env("GIT_TERMINAL_PROMPT", "0");
    run_capture(command)
}

fn run_capture(mut command: std::process::Command) -> (String, bool) {
    match command.output() {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            if !out.stderr.is_empty() {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(&String::from_utf8_lossy(&out.stderr));
            }
            (s, !out.status.success())
        }
        Err(e) => (format!("failed to run command: {e}"), true),
    }
}

/// Strip ANSI escape sequences (CSI + OSC) and bare carriage returns
/// from captured command output (GOALS §1k/§1l: "strip ANSI").
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: consume params until a final byte (0x40–0x7e).
                    for f in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&f) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: consume until BEL or ST (ESC \).
                    while let Some(f) = chars.next() {
                        if f == '\x07' {
                            break;
                        }
                        if f == '\x1b' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\r' => {} // drop bare CRs (CRLF → LF)
            _ => out.push(c),
        }
    }
    out
}

/// Truncate display output to [`LOCAL_CMD_DISPLAY_LINES`] with a note.
fn cap_display_lines(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= LOCAL_CMD_DISPLAY_LINES {
        return s.trim_end_matches('\n').to_string();
    }
    let mut out = lines[..LOCAL_CMD_DISPLAY_LINES].join("\n");
    out.push_str(&format!(
        "\n… [{} more lines — re-run in a real terminal for full output]",
        lines.len() - LOCAL_CMD_DISPLAY_LINES
    ));
    out
}

/// Cap text to roughly `max_tokens` (cl100k estimate) with a marker.
fn cap_tokens(s: &str, max_tokens: usize) -> String {
    if crate::tokens::count(s) <= max_tokens {
        return s.to_string();
    }
    let mut budget = max_tokens.saturating_mul(4).max(64);
    loop {
        let truncated: String = s.chars().take(budget).collect();
        if budget < 64 || crate::tokens::count(&truncated) <= max_tokens {
            return format!("{truncated}\n… [truncated to ~{max_tokens} tokens]");
        }
        budget = budget * 3 / 4;
    }
}

/// Escape a string for an XML attribute value (the `/git cmd="…"`).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Settle the most-recent still-running [`HistoryEntry::Subagent`] for
/// `child` against its `report`. Freezes the elapsed clock into the
/// total duration and flips the live `delegated to…` line into the
/// settled header + response. A report whose text the driver prefixed
/// with `Error: ` (its failure encoding) flips the entry to the failed
/// header — never leaving a dangling animated line. If no running entry
/// is found (defensive — spawn/report events should pair), a settled
/// entry is pushed so the report is never lost.
fn settle_subagent_in(history: &mut Vec<HistoryEntry>, child: &str, report: String) {
    let failed = report.starts_with("Error: ");
    let found = history.iter_mut().rev().find_map(|entry| match entry {
        HistoryEntry::Subagent {
            child: c,
            spawned_at,
            outcome: outcome @ None,
            ..
        } if c == child => Some((spawned_at, outcome)),
        _ => None,
    });
    match found {
        Some((spawned_at, outcome)) => {
            *outcome = Some(SubagentOutcome {
                duration: spawned_at.elapsed(),
                failed,
                report,
            });
        }
        None => history.push(HistoryEntry::Subagent {
            parent: String::new(),
            child: child.to_string(),
            spawned_at: Instant::now(),
            outcome: Some(SubagentOutcome {
                duration: Duration::ZERO,
                failed,
                report,
            }),
            expanded: false,
        }),
    }
}

fn entry_to_plain_lines(entry: &HistoryEntry) -> Vec<String> {
    match entry {
        HistoryEntry::Plain { line } => vec![line.clone()],
        HistoryEntry::LocalCommand { label, output, .. } => {
            let mut out = vec![label.clone()];
            for line in output.lines() {
                out.push(format!("  {line}"));
            }
            out
        }
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
        HistoryEntry::Subagent {
            parent,
            child,
            outcome,
            ..
        } => match outcome {
            // A still-running delegation spilled on `/new`: record the
            // delegation line without the (now-meaningless) live timer.
            None => vec![format!("{parent} delegated to {child}…")],
            Some(o) => {
                let verb = if o.failed {
                    "failed after"
                } else {
                    "worked for"
                };
                let header = format!(
                    "{child} {verb} {}",
                    crate::tui::history::format_compact_duration(o.duration)
                );
                let mut out = vec![header];
                for line in o.report.lines() {
                    out.push(format!("  {line}"));
                }
                out
            }
        },
        HistoryEntry::CompactBoundary {
            predecessor_short_id,
            seed_tool_count,
            ..
        } => {
            vec![format!(
                "── compacted from {predecessor_short_id} · {seed_tool_count} seed-tool(s) re-run ──"
            )]
        }
    }
}

#[allow(private_interfaces)]
pub(super) fn slash_matches(
    query: &str,
    counts: &HashMap<String, u64>,
) -> Vec<&'static SlashCommand> {
    let mut matched: Vec<(usize, &'static SlashCommand)> = SLASH_COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name.starts_with(query) && c.is_available())
        .collect();
    // Frequency tie-breaker: 30-day count desc, then the static
    // declaration order (the stable fallback) asc.
    matched.sort_by(|(ia, a), (ib, b)| {
        let ca = counts.get(a.name).copied().unwrap_or(0);
        let cb = counts.get(b.name).copied().unwrap_or(0);
        cb.cmp(&ca).then(ia.cmp(ib))
    });
    matched.into_iter().map(|(_, c)| c).collect()
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

/// Resolve the answering-dialog config (GOALS §3b) from the layered
/// `extended-config.json` — same first-match walk as
/// [`load_tui_config`]. Used to read the anti-misfire lockout delay.
fn load_dialog_config(cwd: &Path) -> crate::config::extended::DialogConfig {
    for dir in discover_config_dirs(cwd) {
        let path = dir.path.join("extended-config.json");
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(cfg) = serde_json::from_slice::<ExtendedConfig>(&bytes)
        {
            return cfg.dialog;
        }
    }
    crate::config::extended::DialogConfig::default()
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

#[cfg(test)]
mod display_attach_gate_tests {
    use super::should_attempt_display_attach;
    use std::cell::Cell;

    /// The happy path: no runner, prompt closed, not daemonless, believed
    /// connected, and the daemon answers → attach.
    #[test]
    fn attaches_when_daemon_reachable() {
        assert!(should_attempt_display_attach(
            false,
            false,
            false,
            true,
            || true
        ));
    }

    /// A runner already exists → no attach, and the probe is never run
    /// (cheap struct gates short-circuit before the costly probe).
    #[test]
    fn skips_when_runner_exists_without_probing() {
        let probed = Cell::new(false);
        let attach = should_attempt_display_attach(true, false, false, true, || {
            probed.set(true);
            true
        });
        assert!(!attach);
        assert!(!probed.get(), "must not probe once a runner exists");
    }

    /// The "daemon not running" prompt is still open → don't spawn a daemon
    /// out from under the user's choice; probe is skipped.
    #[test]
    fn skips_while_prompt_open() {
        let probed = Cell::new(false);
        let attach = should_attempt_display_attach(false, true, false, true, || {
            probed.set(true);
            true
        });
        assert!(!attach);
        assert!(!probed.get());
    }

    /// Daemonless ("continue without daemon") → never eager-spawn the owned
    /// ephemeral daemon purely to display an id (deliberate non-goal). Probe
    /// is skipped even though `daemon_connected` is true in this mode.
    #[test]
    fn skips_in_daemonless_mode() {
        let probed = Cell::new(false);
        let attach = should_attempt_display_attach(false, false, true, true, || {
            probed.set(true);
            true
        });
        assert!(!attach);
        assert!(
            !probed.get(),
            "daemonless must not probe/attach for display"
        );
    }

    /// `daemon_connected` is false → no attach, no probe.
    #[test]
    fn skips_when_not_connected() {
        let probed = Cell::new(false);
        let attach = should_attempt_display_attach(false, false, false, false, || {
            probed.set(true);
            true
        });
        assert!(!attach);
        assert!(!probed.get());
    }

    /// All cheap gates pass but the just-started daemon's socket isn't bound
    /// yet (probe returns false) → wait quietly; retry on a later tick. This
    /// is the "Start and connect" startup gap that previously double-spawned.
    #[test]
    fn waits_when_socket_not_yet_bound() {
        assert!(!should_attempt_display_attach(
            false,
            false,
            false,
            true,
            || false
        ));
    }
}

#[cfg(test)]
mod slash_rank_tests {
    use super::{SLASH_COMMANDS, slash_matches};
    use std::collections::HashMap;

    #[test]
    fn frequency_outranks_declaration_order() {
        // The last-declared command, given a count, jumps to the front.
        let last = SLASH_COMMANDS.last().unwrap().name;
        let mut counts = HashMap::new();
        counts.insert(last.to_string(), 9u64);
        let ranked = slash_matches("", &counts);
        assert_eq!(ranked.first().unwrap().name, last);
    }

    #[test]
    fn equal_counts_fall_back_to_declaration_order() {
        let ranked = slash_matches("", &HashMap::new());
        let names: Vec<&str> = ranked.iter().map(|c| c.name).collect();
        // `slash_matches` hides unavailable commands (`/editor` without
        // `$EDITOR`, `/lazygit` off `PATH`), so compare against the
        // available subset — otherwise this is env-dependent on CI.
        let declared: Vec<&str> = SLASH_COMMANDS
            .iter()
            .filter(|c| c.is_available())
            .map(|c| c.name)
            .collect();
        assert_eq!(names, declared);
    }

    #[test]
    fn sandbox_command_is_registered() {
        // `/sandbox` (sandboxing part 2) must be dispatchable.
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "sandbox"),
            "/sandbox must be a registered slash command"
        );
    }

    #[test]
    fn plan_and_build_commands_are_registered() {
        // `/plan` and `/build` swap the primary agent (`plan.md §4.6.d`).
        for name in ["plan", "build"] {
            assert!(
                SLASH_COMMANDS.iter().any(|c| c.name == name),
                "/{name} must be a registered slash command"
            );
        }
    }

    #[test]
    fn plan_agent_color_is_f8d749() {
        // The `Plan` agent shows in #f8d749 in the chrome/history.
        assert_eq!(
            crate::tui::history::agent_color("Plan"),
            ratatui::style::Color::Rgb(0xf8, 0xd7, 0x49)
        );
    }

    #[test]
    fn rename_command_is_registered() {
        // `/rename` (rename-current-session) must be dispatchable.
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "rename"),
            "/rename must be a registered slash command"
        );
    }

    #[test]
    fn skills_command_is_registered() {
        // `/skills` (read-only skill listing) must be dispatchable.
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "skills"),
            "/skills must be a registered slash command"
        );
    }

    #[test]
    fn side_command_is_registered() {
        // `/side` (ephemeral throwaway side conversation) must be dispatchable.
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "side"),
            "/side must be a registered slash command"
        );
    }

    #[test]
    fn plans_command_is_registered() {
        // `/plans` (read-only plan browser) must be dispatchable.
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "plans"),
            "/plans must be a registered slash command"
        );
    }

    #[test]
    fn permissions_command_is_registered() {
        // `/permissions` (delete-only approvals manager) must be dispatchable.
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "permissions"),
            "/permissions must be a registered slash command"
        );
    }

    #[test]
    fn session_command_is_not_registered() {
        // The dead `/session` subcommand stub was removed in favor of
        // `/rename`; it must no longer appear in the menu or dispatch.
        assert!(
            !SLASH_COMMANDS.iter().any(|c| c.name == "session"),
            "/session must not be a registered slash command"
        );
    }

    #[test]
    fn copy_command_is_registered() {
        // `/copy` (copy-last-response) must be dispatchable.
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "copy"),
            "/copy must be a registered slash command"
        );
    }

    #[test]
    fn export_command_is_registered_and_visible() {
        // `/export` must be a registered, available (menu-visible) slash
        // command. The `debug` argument is hidden — it's an arg of
        // `/export`, never its own menu entry — so there is no `export
        // debug` command name.
        let export = SLASH_COMMANDS
            .iter()
            .find(|c| c.name == "export")
            .expect("/export must be a registered slash command");
        assert!(export.is_available(), "/export must be visible in the menu");
        assert!(
            !SLASH_COMMANDS.iter().any(|c| c.name == "export debug"),
            "`debug` is a hidden arg of /export, not its own command"
        );
    }

    #[test]
    fn ps_and_stop_are_registered() {
        // `/ps` (current-session job list) and `/stop` (current-session
        // job stop) must both be dispatchable; `/jobs` (all-sessions) is
        // kept alongside them.
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "ps"),
            "/ps must be a registered slash command"
        );
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "stop"),
            "/stop must be a registered slash command"
        );
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "jobs"),
            "/jobs must remain a registered slash command"
        );
    }

    #[test]
    fn new_and_clear_are_both_registered_aliases() {
        // `/new` and `/clear` are both menu entries routing to the one
        // fresh-session handler (`"new" | "clear"` dispatch arm).
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "new"),
            "/new must be a registered slash command"
        );
        assert!(
            SLASH_COMMANDS.iter().any(|c| c.name == "clear"),
            "/clear must be a registered slash command"
        );
    }
}

#[cfg(test)]
mod session_jobs_tests {
    use super::{ActiveJob, format_job_line, session_job_ids};
    use std::collections::BTreeMap;
    use std::time::Instant;

    fn job(session_id: uuid::Uuid, kind: &str, iteration: u64) -> ActiveJob {
        ActiveJob {
            session_id,
            label: format!("{kind} job"),
            kind: kind.to_string(),
            iteration,
            last_activity: Instant::now(),
        }
    }

    fn fixture() -> (uuid::Uuid, uuid::Uuid, BTreeMap<String, ActiveJob>) {
        let a = uuid::Uuid::from_u128(1);
        let b = uuid::Uuid::from_u128(2);
        let mut jobs = BTreeMap::new();
        jobs.insert("job-a1".to_string(), job(a, "loop", 3));
        jobs.insert("job-a2".to_string(), job(a, "background", 0));
        jobs.insert("job-b1".to_string(), job(b, "timer", 1));
        (a, b, jobs)
    }

    #[test]
    fn filters_to_only_the_current_session() {
        // `/ps` scope: session `a` sees its two jobs, in stable job-id
        // order, and never session `b`'s.
        let (a, b, jobs) = fixture();
        assert_eq!(session_job_ids(&jobs, a), vec!["job-a1", "job-a2"]);
        assert_eq!(session_job_ids(&jobs, b), vec!["job-b1"]);
    }

    #[test]
    fn empty_when_session_has_no_jobs() {
        // `/ps` empty-state basis: a session with no jobs yields nothing.
        let (_, _, jobs) = fixture();
        let other = uuid::Uuid::from_u128(99);
        assert!(session_job_ids(&jobs, other).is_empty());
    }

    #[test]
    fn cross_session_id_is_not_in_current_set() {
        // `/stop <id>` refusal basis: an id owned by another session is
        // not a member of the current session's id set.
        let (a, _, jobs) = fixture();
        let current = session_job_ids(&jobs, a);
        assert!(!current.iter().any(|id| id == "job-b1"));
        assert!(current.iter().any(|id| id == "job-a1"));
    }

    #[test]
    fn bare_stop_count_matches_current_session_jobs() {
        // Bare `/stop` confirm count `N` = number of current-session jobs.
        let (a, b, jobs) = fixture();
        assert_eq!(session_job_ids(&jobs, a).len(), 2);
        assert_eq!(session_job_ids(&jobs, b).len(), 1);
    }

    #[test]
    fn job_line_shows_iteration_for_loops_but_not_background() {
        let a = uuid::Uuid::from_u128(1);
        assert_eq!(
            format_job_line("job-a1", &job(a, "loop", 3)),
            "job-a1 [loop] 3 iter  loop job"
        );
        assert_eq!(
            format_job_line("job-a2", &job(a, "background", 0)),
            "job-a2 [background]  background job"
        );
    }
}

#[cfg(test)]
mod working_msg_tests {
    use super::{WORKING_MESSAGES, pick_working_msg};

    #[test]
    fn picks_in_range_and_avoids_previous() {
        // Re-roll many times from each previous index; the result must
        // always be valid and never equal to the previous pick.
        for prev in 0..WORKING_MESSAGES.len() {
            for _ in 0..200 {
                let next = pick_working_msg(prev);
                assert!(next < WORKING_MESSAGES.len());
                assert_ne!(next, prev);
            }
        }
    }

    #[test]
    fn out_of_range_sentinel_allows_any_index() {
        // The one-past-end init lets the first roll land anywhere; just
        // assert it always returns an in-range index.
        for _ in 0..200 {
            let idx = pick_working_msg(WORKING_MESSAGES.len());
            assert!(idx < WORKING_MESSAGES.len());
        }
    }
}

#[cfg(test)]
mod local_cmd_tests {
    use super::{
        GIT_AGENT_TOKEN_CAP, PaneSide, cache_config_caches, cap_tokens, parse_llm_mode_arg,
        parse_pane_side, parse_sandbox_arg, slash_args, strip_ansi, xml_escape,
    };

    #[test]
    fn strip_ansi_removes_csi_and_cr() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m\r\nplain"), "red\nplain");
    }

    #[test]
    fn strip_ansi_removes_osc() {
        assert_eq!(strip_ansi("\x1b]0;window title\x07body"), "body");
    }

    #[test]
    fn slash_args_splits_off_command_token() {
        assert_eq!(slash_args("/git status -s"), "status -s");
        assert_eq!(slash_args("/git"), "");
        assert_eq!(slash_args("/editor right"), "right");
        // A bare prefix (popup-accepted before any space) has no args.
        assert_eq!(slash_args("/g"), "");
    }

    #[test]
    fn parse_pane_side_aliases() {
        assert_eq!(parse_pane_side("up"), PaneSide::Top);
        assert_eq!(parse_pane_side("down"), PaneSide::Bottom);
        assert_eq!(parse_pane_side("LEFT"), PaneSide::Left);
        assert_eq!(parse_pane_side(""), PaneSide::Full);
        assert_eq!(parse_pane_side("garbage"), PaneSide::Full);
    }

    #[test]
    fn parse_sandbox_arg_maps_to_enabled() {
        // `/sandbox` (no arg) toggles; `on`/`off` set explicitly
        // (sandboxing part 2). Case- and whitespace-insensitive.
        assert_eq!(parse_sandbox_arg(""), Ok(None));
        assert_eq!(parse_sandbox_arg("  "), Ok(None));
        assert_eq!(parse_sandbox_arg("on"), Ok(Some(true)));
        assert_eq!(parse_sandbox_arg(" ON "), Ok(Some(true)));
        assert_eq!(parse_sandbox_arg("off"), Ok(Some(false)));
        assert_eq!(parse_sandbox_arg("Off"), Ok(Some(false)));
        assert_eq!(parse_sandbox_arg("maybe"), Err("maybe".to_string()));
    }

    #[test]
    fn parse_llm_mode_arg_toggle_default_and_aliases() {
        use crate::config::extended::LlmMode;
        // No arg or `toggle` → toggle (None).
        assert_eq!(parse_llm_mode_arg(""), Ok(None));
        assert_eq!(parse_llm_mode_arg("  "), Ok(None));
        assert_eq!(parse_llm_mode_arg("toggle"), Ok(None));
        assert_eq!(parse_llm_mode_arg("TOGGLE"), Ok(None));
        // `defend` is the advertised form; `defensive` is a silent alias.
        assert_eq!(parse_llm_mode_arg("defend"), Ok(Some(LlmMode::Defensive)));
        assert_eq!(
            parse_llm_mode_arg("defensive"),
            Ok(Some(LlmMode::Defensive))
        );
        assert_eq!(parse_llm_mode_arg(" Defend "), Ok(Some(LlmMode::Defensive)));
        // `normal` selects normal.
        assert_eq!(parse_llm_mode_arg("normal"), Ok(Some(LlmMode::Normal)));
        // Anything else is a usage error.
        assert!(parse_llm_mode_arg("yolo").is_err());
    }

    #[test]
    fn cache_break_warning_suppressed_on_no_cache_provider() {
        use crate::config::providers::{CacheConfig, CacheMode};
        // No-cache provider → the predicate says it doesn't cache, so the
        // warning is suppressed.
        let none = CacheConfig {
            mode: CacheMode::None,
            ttl_secs: 300,
        };
        assert!(
            !cache_config_caches(&none),
            "a no-cache provider must report no caching (warning suppressed)"
        );
        // Caching provider → the warning fires.
        let ephemeral = CacheConfig {
            mode: CacheMode::Ephemeral,
            ttl_secs: 300,
        };
        assert!(
            cache_config_caches(&ephemeral),
            "a caching provider must report caching (warning fires)"
        );
    }

    #[test]
    fn xml_escape_attr() {
        assert_eq!(xml_escape("a\"b<c>&d"), "a&quot;b&lt;c&gt;&amp;d");
    }

    #[test]
    fn cap_tokens_keeps_small_input() {
        let small = "short git output";
        assert_eq!(cap_tokens(small, GIT_AGENT_TOKEN_CAP), small);
    }

    #[test]
    fn cap_tokens_truncates_large_input() {
        let big = "word ".repeat(5000);
        let capped = cap_tokens(&big, 100);
        assert!(capped.contains("truncated"));
        assert!(crate::tokens::count(&capped) <= 200);
    }

    #[cfg(unix)]
    #[test]
    fn exec_capture_shell_captures_stdout_and_status() {
        use super::exec_capture_shell;
        let (out, failed) = exec_capture_shell("printf hello", std::path::Path::new("."));
        assert!(!failed);
        assert!(out.contains("hello"));
        let (_o, failed) = exec_capture_shell("exit 3", std::path::Path::new("."));
        assert!(failed);
    }
}

#[cfg(test)]
mod subagent_settle_tests {
    use super::settle_subagent_in;
    use crate::tui::history::HistoryEntry;

    fn running(parent: &str, child: &str) -> HistoryEntry {
        HistoryEntry::Subagent {
            parent: parent.into(),
            child: child.into(),
            spawned_at: std::time::Instant::now(),
            outcome: None,
            expanded: false,
        }
    }

    fn outcome(entry: &HistoryEntry) -> Option<(&str, bool)> {
        match entry {
            HistoryEntry::Subagent {
                outcome: Some(o), ..
            } => Some((o.report.as_str(), o.failed)),
            _ => None,
        }
    }

    /// Spawn → report transition settles the running entry in place
    /// (no new entry pushed) with the report and failed=false.
    #[test]
    fn report_settles_running_entry_in_place() {
        let mut history = vec![running("Build", "explore")];
        settle_subagent_in(&mut history, "explore", "all done".into());
        assert_eq!(history.len(), 1);
        assert_eq!(outcome(&history[0]), Some(("all done", false)));
    }

    /// A report whose text is the driver's `Error: ` failure encoding
    /// settles as a failure (failed=true) rather than a normal report.
    #[test]
    fn error_prefixed_report_settles_as_failure() {
        let mut history = vec![running("Build", "explore")];
        settle_subagent_in(&mut history, "explore", "Error: it broke".into());
        assert_eq!(outcome(&history[0]), Some(("Error: it broke", true)));
    }

    /// An empty report still settles the entry (the renderer shows a
    /// bare header) — it doesn't leave a dangling running line.
    #[test]
    fn empty_report_settles_running_entry() {
        let mut history = vec![running("Build", "explore")];
        settle_subagent_in(&mut history, "explore", String::new());
        assert_eq!(outcome(&history[0]), Some(("", false)));
    }

    /// Each report settles the most-recent still-running entry for the
    /// child (the just-spawned one), leaving already-settled entries
    /// untouched. With two running entries, the first report settles the
    /// newer (last) one, the second report settles the older.
    #[test]
    fn settles_most_recent_running_for_child() {
        let mut history = vec![running("Build", "explore"), running("Build", "explore")];
        settle_subagent_in(&mut history, "explore", "first".into());
        settle_subagent_in(&mut history, "explore", "second".into());
        assert_eq!(outcome(&history[1]), Some(("first", false)));
        assert_eq!(outcome(&history[0]), Some(("second", false)));
    }

    /// A report with no matching running entry pushes a settled entry
    /// (defensive) so the report is never lost.
    #[test]
    fn orphan_report_pushes_settled_entry() {
        let mut history: Vec<HistoryEntry> = Vec::new();
        settle_subagent_in(&mut history, "explore", "orphan".into());
        assert_eq!(history.len(), 1);
        assert_eq!(outcome(&history[0]), Some(("orphan", false)));
    }
}

#[cfg(test)]
mod prediction_turn_assembly_tests {
    use super::turns_from_history;
    use crate::tui::history::{HistoryEntry, ToolCall, ToolCallState};

    fn user(text: &str) -> HistoryEntry {
        HistoryEntry::User {
            text: text.into(),
            timestamp: chrono::Local::now(),
        }
    }

    fn agent(text: &str, reasoning: &str) -> HistoryEntry {
        HistoryEntry::Agent {
            name: "Build".into(),
            text: text.into(),
            reasoning: reasoning.into(),
            timestamp: chrono::Local::now(),
            expanded: false,
            think_duration: None,
        }
    }

    fn tool_box() -> HistoryEntry {
        HistoryEntry::ToolBox {
            calls: vec![ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                summary: "ls".into(),
                full_input: "ls".into(),
                output: "file.txt".into(),
                state: ToolCallState::Success,
            }],
            view_offset: 0,
            follow: true,
            expanded: false,
        }
    }

    /// One pair per turn: the user message + the agent's final response,
    /// with tool calls and reasoning skipped entirely.
    #[test]
    fn pairs_user_with_agent_final_response_only() {
        let history = vec![
            user("add a flag"),
            tool_box(),
            agent("Done, added the flag.", "let me think about this"),
        ];
        let turns = turns_from_history(&history);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user, "add a flag");
        // The agent FINAL TEXT carries; reasoning never does.
        assert_eq!(turns[0].agent, "Done, added the flag.");
        assert!(!turns[0].agent.contains("think"));
    }

    /// More than three turns: assembly keeps every turn (the last-3 window
    /// is applied by `engine::predict::last_turns`), but each is faithful.
    #[test]
    fn assembles_every_turn_faithfully() {
        let history = vec![
            user("q1"),
            agent("a1", ""),
            user("q2"),
            tool_box(),
            agent("a2", ""),
            user("q3"),
            agent("a3", ""),
            user("q4"),
            agent("a4", ""),
        ];
        let turns = turns_from_history(&history);
        assert_eq!(turns.len(), 4);
        let last3 = crate::engine::predict::last_turns(&turns);
        assert_eq!(last3.len(), 3);
        assert_eq!(last3[0].user, "q2");
        assert_eq!(last3[2].user, "q4");
        assert_eq!(last3[2].agent, "a4");
    }

    /// A user message arriving before the agent reply (queued + folded)
    /// folds into the open turn rather than opening a phantom turn.
    #[test]
    fn consecutive_user_messages_fold_into_open_turn() {
        let history = vec![user("first part"), user("second part"), agent("ok", "")];
        let turns = turns_from_history(&history);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].user.contains("first part"));
        assert!(turns[0].user.contains("second part"));
        assert_eq!(turns[0].agent, "ok");
    }

    /// A trailing user message with no agent reply yet stays an open turn
    /// with an empty agent response — never paired with the wrong reply.
    #[test]
    fn trailing_open_turn_has_empty_agent() {
        let history = vec![user("q1"), agent("a1", ""), user("q2")];
        let turns = turns_from_history(&history);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].user, "q2");
        assert!(turns[1].agent.is_empty());
    }

    /// A fresh session (no agent response yet) yields a window that
    /// `engine::predict` treats as "nothing to predict".
    #[test]
    fn fresh_session_has_no_agent_response() {
        let history = vec![user("first message")];
        let turns = turns_from_history(&history);
        let window = crate::engine::predict::last_turns(&turns);
        assert!(window.iter().all(|t| t.agent.trim().is_empty()));
    }
}

#[cfg(test)]
mod prediction_lifecycle_tests {
    use super::PredictionState;

    /// Eager generate: a turn ends, a result for that turn lands, and the
    /// empty box shows the ghost. Then typing hides it; clearing back to
    /// empty restores it from the cache — WITHOUT a new result/utility call.
    #[test]
    fn show_hide_on_type_then_restore_from_cache_without_recall() {
        let mut st = PredictionState::default();
        st.begin_turn(); // turn 1
        let turn = st.turn();
        // Result for the current turn, box empty → ghost shows.
        st.on_result(turn, Some("run the tests".into()), false, true);
        assert_eq!(
            st.ghost().map(|g| g.display_text().to_string()),
            Some("run the tests".to_string())
        );
        // User types → box non-empty → ghost hidden (cache retained).
        st.reconcile(false);
        assert!(st.ghost().is_none());
        // User clears back to empty → ghost restored from CACHE. No new
        // `on_result` call was made (no new utility call this turn).
        st.reconcile(true);
        assert_eq!(
            st.ghost().map(|g| g.display_text().to_string()),
            Some("run the tests".to_string())
        );
    }

    /// Stale replacement: a result tagged with an older turn (a newer turn
    /// already began) is discarded — never shown.
    #[test]
    fn stale_turn_result_is_discarded() {
        let mut st = PredictionState::default();
        st.begin_turn(); // turn 1
        let stale_turn = st.turn();
        st.begin_turn(); // turn 2 — the stale result now belongs to turn 1
        st.on_result(stale_turn, Some("old prediction".into()), false, true);
        assert!(
            st.ghost().is_none(),
            "a prior turn's prediction must not show"
        );
        // A result for the CURRENT turn does land.
        st.on_result(st.turn(), Some("fresh prediction".into()), false, true);
        assert_eq!(
            st.ghost().map(|g| g.display_text().to_string()),
            Some("fresh prediction".to_string())
        );
    }

    /// Appear-once-ready: a result that arrives while the user is already
    /// typing (box non-empty) does NOT pop in over active input, but the
    /// cache is kept so a later clear-to-empty restores it.
    #[test]
    fn result_arriving_during_typing_does_not_pop_in_but_caches() {
        let mut st = PredictionState::default();
        st.begin_turn();
        let turn = st.turn();
        // Box non-empty when the async result lands → no ghost now.
        st.on_result(turn, Some("later".into()), false, false);
        assert!(st.ghost().is_none());
        // Clearing to empty restores it from the cache (no new call).
        st.reconcile(true);
        assert_eq!(
            st.ghost().map(|g| g.display_text().to_string()),
            Some("later".to_string())
        );
    }

    /// A new turn invalidates the previous turn's cache + ghost so a prior
    /// prediction never lingers into the next turn.
    #[test]
    fn begin_turn_drops_previous_prediction() {
        let mut st = PredictionState::default();
        st.begin_turn();
        st.on_result(st.turn(), Some("first".into()), false, true);
        assert!(st.ghost().is_some());
        st.begin_turn();
        assert!(st.ghost().is_none(), "new turn drops the old ghost");
        // The old cache is gone too: an empty-box reconcile restores
        // nothing until a fresh result lands.
        st.reconcile(true);
        assert!(st.ghost().is_none());
    }

    /// Consume (Tab → real text) drops both ghost and cache so a later
    /// clear-to-empty does not re-offer the accepted prediction.
    #[test]
    fn consume_clears_cache_so_clear_to_empty_does_not_restore() {
        let mut st = PredictionState::default();
        st.begin_turn();
        st.on_result(st.turn(), Some("accepted text".into()), false, true);
        st.consume();
        assert!(st.ghost().is_none());
        st.reconcile(true);
        assert!(
            st.ghost().is_none(),
            "an accepted prediction must not reappear as a ghost"
        );
    }

    /// A `None` result (degrade path — model unset/timeout/empty) leaves no
    /// ghost and no cache.
    #[test]
    fn none_result_leaves_no_ghost() {
        let mut st = PredictionState::default();
        st.begin_turn();
        st.on_result(st.turn(), None, false, true);
        assert!(st.ghost().is_none());
        st.reconcile(true);
        assert!(st.ghost().is_none());
    }
}

#[cfg(test)]
mod copy_cmd_tests {
    use super::{CopyFormat, last_agent_text, parse_copy_format};
    use crate::tui::history::HistoryEntry;

    fn agent(text: &str) -> HistoryEntry {
        HistoryEntry::Agent {
            name: "coder".to_string(),
            text: text.to_string(),
            reasoning: String::new(),
            timestamp: chrono::Local::now(),
            expanded: false,
            think_duration: None,
        }
    }

    #[test]
    fn bare_and_markdown_default_to_markdown() {
        assert_eq!(parse_copy_format(""), Some(CopyFormat::Markdown));
        assert_eq!(parse_copy_format("markdown"), Some(CopyFormat::Markdown));
        // Whitespace-only / mixed case still resolve.
        assert_eq!(parse_copy_format("  "), Some(CopyFormat::Markdown));
        assert_eq!(parse_copy_format("MarkDown"), Some(CopyFormat::Markdown));
    }

    #[test]
    fn plain_and_rich_aliases_parse() {
        assert_eq!(parse_copy_format("plain"), Some(CopyFormat::Plain));
        assert_eq!(parse_copy_format("plaintext"), Some(CopyFormat::Plain));
        assert_eq!(parse_copy_format("rich"), Some(CopyFormat::Rich));
        assert_eq!(parse_copy_format("richtext"), Some(CopyFormat::Rich));
    }

    #[test]
    fn unknown_format_is_none() {
        assert_eq!(parse_copy_format("html"), None);
        assert_eq!(parse_copy_format("md"), None);
    }

    #[test]
    fn last_agent_text_skips_non_agent_and_empty() {
        // No agent messages → None (the no-response path).
        assert_eq!(last_agent_text(&[]), None);
        assert_eq!(
            last_agent_text(&[HistoryEntry::Plain {
                line: "tool chrome".to_string(),
            }]),
            None
        );

        // Tool chrome after the agent message must not shadow it, and a
        // trailing empty agent turn is ignored.
        let history = vec![
            agent("first response"),
            HistoryEntry::Plain {
                line: "a tool ran".to_string(),
            },
            agent("**last** response"),
            agent("   "),
        ];
        assert_eq!(
            last_agent_text(&history).as_deref(),
            Some("**last** response")
        );
    }
}

#[cfg(test)]
mod ctrl_c_tests {
    use super::{CTRL_C_EXIT_WINDOW, CtrlCAction, decide_ctrl_c};
    use std::time::{Duration, Instant};

    /// Idle + single (first) press: arm the window + show hint only,
    /// nothing to interrupt. The window is armed at `now`.
    #[test]
    fn idle_first_press_arms_only() {
        let now = Instant::now();
        let (action, armed) = decide_ctrl_c(now, None, CTRL_C_EXIT_WINDOW, false);
        assert_eq!(action, CtrlCAction::ArmOnly);
        assert_eq!(armed, Some(now));
    }

    /// Busy + single (first) press: arm the window AND interrupt the agent.
    #[test]
    fn busy_first_press_arms_and_interrupts() {
        let now = Instant::now();
        let (action, armed) = decide_ctrl_c(now, None, CTRL_C_EXIT_WINDOW, true);
        assert_eq!(action, CtrlCAction::ArmAndInterrupt);
        assert_eq!(armed, Some(now));
    }

    /// Second press inside the window exits — regardless of agent state.
    /// During a run, the first press already interrupted; this second one
    /// is the "interrupt AND exit" case.
    #[test]
    fn second_press_within_window_exits_when_busy() {
        let first = Instant::now();
        let second = first + Duration::from_millis(200); // < 500ms
        let (action, armed) = decide_ctrl_c(second, Some(first), CTRL_C_EXIT_WINDOW, true);
        assert_eq!(action, CtrlCAction::Exit);
        assert_eq!(armed, None);
    }

    /// Second press inside the window exits even when idle (idle + two fast
    /// presses = exit).
    #[test]
    fn second_press_within_window_exits_when_idle() {
        let first = Instant::now();
        let second = first + Duration::from_millis(499);
        let (action, _armed) = decide_ctrl_c(second, Some(first), CTRL_C_EXIT_WINDOW, false);
        assert_eq!(action, CtrlCAction::Exit);
    }

    /// Exactly at the window boundary still counts as a second press
    /// (`<=` window).
    #[test]
    fn second_press_at_window_boundary_exits() {
        let first = Instant::now();
        let second = first + CTRL_C_EXIT_WINDOW;
        let (action, _armed) = decide_ctrl_c(second, Some(first), CTRL_C_EXIT_WINDOW, false);
        assert_eq!(action, CtrlCAction::Exit);
    }

    /// Two presses spaced further apart than the window NEVER exit: the
    /// second is treated as a fresh first press (re-armed at `now`).
    #[test]
    fn presses_outside_window_never_exit() {
        let first = Instant::now();
        let second = first + Duration::from_millis(501); // > 500ms
        let (action, armed) = decide_ctrl_c(second, Some(first), CTRL_C_EXIT_WINDOW, false);
        assert_eq!(action, CtrlCAction::ArmOnly);
        assert_eq!(
            armed,
            Some(second),
            "a lapsed window re-arms at the new press"
        );

        // A steady stream of slow presses interrupts repeatedly, never
        // exits: each press is > window after the previous.
        let third = second + Duration::from_millis(600);
        let (action, armed) = decide_ctrl_c(third, Some(second), CTRL_C_EXIT_WINDOW, true);
        assert_eq!(action, CtrlCAction::ArmAndInterrupt);
        assert_eq!(armed, Some(third));
    }

    /// The window slides from the *last* press: a press just inside the
    /// window of the immediately-previous press exits, even if the very
    /// first press was long ago.
    #[test]
    fn window_slides_from_last_press() {
        let t0 = Instant::now();
        // First press, armed at t0.
        let (_a, armed) = decide_ctrl_c(t0, None, CTRL_C_EXIT_WINDOW, false);
        // A press > window later: fresh first press, re-arm.
        let t1 = t0 + Duration::from_millis(800);
        let (a, armed) = decide_ctrl_c(t1, armed, CTRL_C_EXIT_WINDOW, false);
        assert_eq!(a, CtrlCAction::ArmOnly);
        // A press < window after t1: exits (slides from t1, not t0).
        let t2 = t1 + Duration::from_millis(100);
        let (a, _armed) = decide_ctrl_c(t2, armed, CTRL_C_EXIT_WINDOW, false);
        assert_eq!(a, CtrlCAction::Exit);
    }
}
