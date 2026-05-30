//! Wire protocol — NDJSON envelopes carried over any byte stream.
//!
//! One envelope per newline-terminated frame. Same shape on the
//! in-process channel (today), the Unix socket (P3), and the future
//! WebSocket relay for `cockpit connect` (GOALS §8c, §8d).
//!
//! Layout:
//!
//! ```text
//! { "v": 1, "kind": "req"|"res"|"evt"|"err", ... }
//! ```
//!
//! - **`req`** — client → daemon. Carries a uuid `id` the daemon
//!   echoes on the matching `res` / `err`.
//! - **`res`** — daemon → client. Pairs with `req` by `id`.
//! - **`evt`** — daemon → client. Unsolicited stream event (assistant
//!   text deltas, tool starts/ends, interrupt-raised, …). No id; the
//!   client routes events by `session_id` payload.
//! - **`err`** — daemon → client. Used both as a paired response to a
//!   failed `req` (carries the matching `id`) and as an
//!   out-of-band notification (`id = null`).
//!
//! The schema version (`v`) sits on every envelope so a future bump
//! can be detected on a per-line basis without buffering. Clients
//! refuse envelopes whose `v` is outside the supported range.

use std::collections::HashMap;
use std::io;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};
use uuid::Uuid;

/// Current wire schema version. Bumped only with a written migration
/// note in `GOALS.md`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Max length of a single NDJSON frame. Tool args + read payloads can
/// be large; keep this generous so a `read` of an 8 KB-capped file
/// plus the envelope wrapper has headroom.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

// ---- Envelope --------------------------------------------------------------

/// Top-level frame. Always carries the protocol version and one of four
/// body variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    #[serde(flatten)]
    pub body: Body,
}

impl Envelope {
    pub fn request(id: Uuid, request: Request) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body: Body::Request { id, request },
        }
    }

    pub fn response(id: Uuid, response: Response) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body: Body::Response { id, response },
        }
    }

    pub fn event(event: Event) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body: Body::Event { event },
        }
    }

    pub fn error(id: Option<Uuid>, error: ErrorPayload) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body: Body::Error { id, error },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Body {
    #[serde(rename = "req")]
    Request {
        id: Uuid,
        #[serde(flatten)]
        request: Request,
    },
    #[serde(rename = "res")]
    Response {
        id: Uuid,
        #[serde(flatten)]
        response: Response,
    },
    #[serde(rename = "evt")]
    Event {
        #[serde(flatten)]
        event: Event,
    },
    #[serde(rename = "err")]
    Error {
        /// `Some` when this `err` pairs with a `req`; `None` for
        /// out-of-band errors.
        #[serde(default)]
        id: Option<Uuid>,
        error: ErrorPayload,
    },
}

// ---- Requests --------------------------------------------------------------

/// Client → daemon RPCs. The daemon answers each with a matching
/// [`Response`] keyed by envelope id, or an [`ErrorPayload`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case", content = "params")]
pub enum Request {
    /// Attach to an existing session by id, or create a new one.
    /// Returns the session's identity + a snapshot of its existing
    /// history so the TUI can re-render the transcript after a
    /// reconnect.
    Attach {
        #[serde(default)]
        session_id: Option<Uuid>,
        /// Project root override; when None the daemon uses the cwd
        /// it knows for this client connection.
        #[serde(default)]
        project_root: Option<String>,
        /// The client's `--no-sandbox` flag (sandboxing part 2). When
        /// `true`, sessions this client *creates* start with filesystem
        /// sandboxing OFF — unless the daemon itself was launched
        /// `--no-sandbox` (which wins). Ignored on resume of an existing
        /// session (the session keeps its own state). Defaults to
        /// `false` so older clients attach sandboxed.
        #[serde(default)]
        no_sandbox: bool,
        /// Whether this client can *answer* interrupts (approval / loop-
        /// guard / `question` prompts). The TUI sets `true`; a `cockpit
        /// run` event pump sets `false` (it streams events but has no UI
        /// to answer with). The daemon tracks the interactive-client count
        /// per session so the loop guard knows when a run is headless and
        /// must auto-reject a repeat rather than block. Defaults to
        /// `false` so an older client (and any non-answering attach) is
        /// treated as headless — the safe, non-blocking default.
        #[serde(default)]
        interactive: bool,
    },

    /// Send a user message into the currently attached session. The
    /// daemon enqueues it on the driver and acks immediately —
    /// per-turn progress flows over the event stream. `images` carries
    /// PNG bytes for any pasted images sent as real image parts
    /// (vision models only; non-vision clients fold images into `text`
    /// and leave this empty — composer-paste-handling). The `text` may
    /// contain `IMAGE_PART_SENTINEL` markers, one per image, in order.
    SendUserMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Vec<u8>>,
    },

    /// Cancel the in-flight model call for the attached session. The
    /// daemon aborts the streaming completion and returns control to
    /// the agent stack so the user can redirect.
    CancelTurn,

    /// Resolve an outstanding interrupt (GOALS §3b) raised by a
    /// background coder.
    ResolveInterrupt {
        interrupt_id: Uuid,
        response: ResolveResponse,
    },

    /// List sessions, newest first. Both filters default to None:
    ///
    /// - `project_id = None, parent_session_id = None` — every session
    ///   (legacy behavior, used by `cockpit session list`).
    /// - `project_id = Some(p), parent_session_id = None` — root
    ///   sessions in project `p` (the top level of the `/sessions`
    ///   browser, GOALS §17f).
    /// - `project_id = _, parent_session_id = Some(s)` — direct forks
    ///   of session `s` (the right-arrow descent in `/sessions`).
    ListSessions {
        #[serde(default)]
        project_id: Option<String>,
        #[serde(default)]
        parent_session_id: Option<Uuid>,
    },

    /// Per-session live status for the `/sessions` browser's top two
    /// tiers (GOALS §17f): which of `session_ids` currently have active
    /// async jobs (loop/timer/background) and which are mid-turn
    /// (processing). Sourced from the in-daemon per-session `JobAuthority`
    /// plus worker turn-state — the TUI is a socket client and can't see
    /// in-memory daemon state otherwise. Sessions with no live worker are
    /// simply absent from the response (the browser treats them as
    /// not-processing, no-jobs and falls back to DB tiers).
    SessionLiveStatus { session_ids: Vec<Uuid> },

    /// Archive a session (recoverable soft-delete, GOALS §17h). With
    /// `cascade`, archives the whole descendant fork subtree. The browser
    /// hides archived sessions by default with a toggle to reveal them.
    ArchiveSession {
        session_id: Uuid,
        #[serde(default)]
        cascade: bool,
    },

    /// Clear a session's archive flag (recover it from the archived view).
    UnarchiveSession { session_id: Uuid },

    /// Branch a fork off `parent_session_id` at `fork_point_turn_id`
    /// (None = tail). GOALS §17e. `ephemeral` marks a throwaway `/side`
    /// side-conversation fork — excluded from lists, never auto-titled,
    /// discarded on end/exit.
    ForkSession {
        parent_session_id: Uuid,
        #[serde(default)]
        fork_point_turn_id: Option<String>,
        #[serde(default)]
        ephemeral: bool,
    },

    /// Stop an ephemeral side-conversation (`/side`) worker and discard its
    /// row + descendant forks. No-op for a non-ephemeral session (guarded).
    DiscardSession { session_id: Uuid },

    /// Manually set a session's title; locks out auto-titling.
    /// GOALS §17d.
    RenameSession { session_id: Uuid, title: String },

    /// Drop a session and (optionally) its descendant forks.
    /// FK cascades take care of tool_call_events / inference_calls /
    /// lock state. GOALS §17h.
    DeleteSession {
        session_id: Uuid,
        #[serde(default)]
        cascade: bool,
    },

    /// Return the resolved config plus the per-layer view, for the
    /// `/config` tabbed editor (GOALS §2c).
    GetConfig,

    /// List discovered skills, resolving the configured scan dirs from
    /// `project_root` (the client's cwd) so per-project config applies.
    ListSkills { project_root: String },

    /// List every plan (active first, newest within a group) for the
    /// read-only `/plans` browser. Plans are global, so no project scope.
    ListPlans,

    /// Full detail of one plan for the `/plans` drill-in: its steps with
    /// dependency edges, per-step status, and each step's tests.
    PlanDetail { plan_id: Uuid },

    /// List discovered agents (bundled + on-disk + agent_dirs).
    ListAgents,

    /// List models known for the active provider, or for a specific
    /// provider when set.
    ListModels {
        #[serde(default)]
        provider: Option<String>,
    },

    /// Switch the attached session to a different model.
    SetActiveModel { provider: String, model: String },

    /// Swap which built-in or user agent owns the conversation.
    SetAgent { name: String },

    /// Set (or toggle) filesystem sandboxing for the attached session at
    /// runtime (`/sandbox`, sandboxing part 2). `enabled = None` toggles
    /// the current state; `Some(true)`/`Some(false)` set it explicitly.
    /// Effective immediately for subsequent tool calls. Acked with the
    /// resulting state via [`Response::SandboxState`].
    SetSandbox {
        #[serde(default)]
        enabled: Option<bool>,
    },

    /// Set caffeination (`/caffeinate`): suppress system sleep + lid-close
    /// so agents survive a closed lid. Daemon-global state — the daemon
    /// holds the OS sleep assertion in its own (long-lived) process and
    /// broadcasts the resulting [`Event::CaffeinateState`] to **every**
    /// connected client (not just the attached session). `until_idle`
    /// auto-off is decided by the daemon once no agent is running. Acked
    /// with [`Response::CaffeinateState`].
    SetCaffeinate {
        mode: crate::daemon::caffeinate::CaffeinateMode,
    },

    /// Cancel a live async job (loop / timer / background, GOALS §22) by
    /// id, on behalf of the human (the `/jobs cancel <id>` affordance).
    CancelJob { job_id: String },

    /// Run `/prune` (snapshot dedup) on the attached session's foreground
    /// agent. Acked immediately; the `Pruned` + refreshed
    /// `ContextProjection` events flow over the stream. The confirm UX
    /// lives in the TUI — this request means the user already accepted.
    Prune,

    /// Run `/compact` (fresh-thread handoff) on the attached session's
    /// foreground agent. Acked immediately; the assembled handoff arrives
    /// as a `CompactReady` event for review-then-commit.
    Compact,

    /// Pin a user message verbatim for the next `/compact` (`/pin`).
    Pin { text: String },

    /// Cheap liveness probe. Replaces the legacy `"ok\n"` greeting.
    DaemonStatus,

    /// Refresh the daemon's view of selected environment variables.
    /// The TUI sends a curated snapshot of *its* env on every launch so
    /// API tokens / API-URL overrides the user just exported in their
    /// shell rc become visible to a long-running daemon without
    /// requiring `cockpit daemon restart`.
    RefreshEnv { vars: HashMap<String, String> },

    /// Record one accepted autocomplete pick into the 30-day frequency
    /// tally (GOALS §1; tie-breaker for the model / slash / @-tag
    /// surfaces). Fire-and-forget — acked immediately; no attached
    /// session is required since the tally is global. `project_id` is
    /// set only for `tag` picks.
    RecordUsage {
        kind: UsageKind,
        key: String,
        #[serde(default)]
        project_id: Option<String>,
    },

    /// Fetch the three 30-day autocomplete count maps. `project_id`
    /// scopes the `tag` map (model + slash are global); `None` yields an
    /// empty `tags` map.
    GetUsageCounts {
        #[serde(default)]
        project_id: Option<String>,
    },

    /// Pre-flight sizing of the project's instruction/guidance file and
    /// full system prompt, for the fresh-chat context indicator. The
    /// daemon resolves the guidance file for `project_root` and estimates
    /// both its body and the full composed system prompt with the
    /// tokenizer calibrated for `(provider, model)`. The daemon's count is
    /// calibrated; the TUI computes the same locally (raw cl100k) when no
    /// daemon is running.
    GuidanceEstimate {
        project_root: String,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        model: Option<String>,
    },

    /// Request orderly shutdown. The daemon flushes in-flight writes
    /// (session DB, lock state) before exiting.
    StopDaemon,
}

/// Which autocomplete surface a [`Request::RecordUsage`] belongs to.
/// Serializes to the `kind` column verbatim (`model` / `slash` / `tag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    Model,
    Slash,
    Tag,
}

impl UsageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Slash => "slash",
            Self::Tag => "tag",
        }
    }
}

// ---- Responses -------------------------------------------------------------

/// Daemon → client RPC responses. Each variant is the typed answer to
/// one [`Request`] kind. The envelope id pairs the two sides.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case", content = "data")]
pub enum Response {
    /// Generic "yes, accepted." Used by fire-and-forget requests
    /// whose effects flow back as events (`SendUserMessage`,
    /// `CancelTurn`, `ResolveInterrupt`, …).
    Ack,

    Attached {
        session_id: Uuid,
        /// 6-char display id (GOALS §17b). Used by the TUI as the
        /// predecessor short-id when this session later spawns a
        /// `/compact` handoff. Empty for pre-§17 rows not yet backfilled.
        #[serde(default)]
        short_id: String,
        project_root: String,
        project_id: String,
        active_agent: String,
        history: Vec<HistoryEntry>,
    },

    Sessions {
        sessions: Vec<SessionSummary>,
    },

    /// Per-session live status. Answer to [`Request::SessionLiveStatus`].
    /// Only sessions with a live worker appear; everything else is
    /// implicitly not-processing / no-jobs.
    SessionLiveStatus {
        statuses: Vec<LiveStatus>,
    },

    /// New session created by `ForkSession`.
    Forked {
        session_id: Uuid,
        short_id: String,
        parent_session_id: Uuid,
        #[serde(default)]
        fork_point_turn_id: Option<String>,
    },

    Config {
        layers: Vec<ConfigLayer>,
        merged: Value,
    },

    Skills {
        skills: Vec<SkillSummary>,
    },

    /// Answer to [`Request::ListPlans`]: every plan, active first.
    Plans {
        plans: Vec<PlanSummaryWire>,
    },

    /// Answer to [`Request::PlanDetail`]: the plan plus its full step DAG
    /// and per-step tests. `None`-equivalent (an error) when no such plan.
    PlanDetail {
        plan: PlanSummaryWire,
        steps: Vec<PlanStepWire>,
    },

    Agents {
        agents: Vec<AgentSummary>,
    },

    Models {
        models: Vec<ModelSummary>,
    },

    DaemonStatus {
        pid: u32,
        uptime_secs: u64,
        active_sessions: u32,
        socket_path: String,
    },

    /// The three 30-day autocomplete count maps. `models` and `slash`
    /// are global; `tags` is scoped to the requested project. Answer to
    /// [`Request::GetUsageCounts`].
    UsageCounts {
        models: HashMap<String, u64>,
        slash: HashMap<String, u64>,
        tags: HashMap<String, u64>,
    },

    /// Pre-flight sizing for the fresh-chat context indicator. `file` is
    /// the basename of the matched guidance file, or `None` when none was
    /// found. `tokens` is the guidance-file **body** size (the `… in
    /// <file>` label); `system_tokens` is the **full** composed system
    /// prompt (role prompt + OS + session + guidance body), the baseline
    /// the running context estimate folds in. Both are estimated with the
    /// tokenizer calibrated for the request's `(provider, model)`.
    /// Answer to [`Request::GuidanceEstimate`].
    GuidanceEstimate {
        #[serde(default)]
        file: Option<String>,
        tokens: u64,
        system_tokens: u64,
    },

    /// The resulting sandbox-enabled state after a [`Request::SetSandbox`]
    /// (sandboxing part 2). The TUI surfaces it via a toast.
    SandboxState {
        enabled: bool,
    },

    /// The resulting caffeination state after a [`Request::SetCaffeinate`].
    /// `message` is the honest confirmation text for the toast (names the
    /// lid-close limitation / missing mechanism where applicable);
    /// `lid_close_guaranteed` is `true` only when active *and* lid-close
    /// survival is assured on this platform/config. The matching
    /// broadcast for other clients is [`Event::CaffeinateState`].
    CaffeinateState {
        active: bool,
        lid_close_guaranteed: bool,
        message: String,
    },
}

// (The wire event variant for the same state change lives on `Event`
// below, carrying `session_id` so the client can route it.)

// ---- Events ----------------------------------------------------------------

/// Unsolicited daemon → client notifications. The event stream is
/// fire-and-forget — clients do not ack individual events. A client
/// that misses events (e.g. dropped connection) re-`Attach`es and
/// receives a fresh history snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", content = "data")]
pub enum Event {
    /// Model inference started. TUI shows `Thinking…` until the first
    /// `AssistantTextDelta` arrives.
    ThinkingStarted { session_id: Uuid, agent: String },

    /// An inference call hit a network/transient failure and is being
    /// auto-retried. TUI shows a non-blocking `reconnecting… attempt N`
    /// status (daemon owns inference state — this is forwarded, not
    /// computed client-side). `attempt` is the 1-based retry number.
    Reconnecting {
        session_id: Uuid,
        agent: String,
        attempt: u32,
    },

    /// One streaming chunk of assistant text.
    AssistantTextDelta {
        session_id: Uuid,
        agent: String,
        delta: String,
    },

    /// One streaming chunk of model reasoning (thinking-mode models).
    /// TUI hides this by default but persists it so the user can
    /// expand the chain of thought later.
    ReasoningDelta {
        session_id: Uuid,
        agent: String,
        delta: String,
    },

    /// Assistant turn complete — `text` is the full accumulated body.
    AssistantText {
        session_id: Uuid,
        agent: String,
        text: String,
    },

    /// Tool dispatch started; args are post-repair.
    ToolStart {
        session_id: Uuid,
        agent: String,
        call_id: String,
        tool: String,
        args: Value,
    },

    /// Tool finished cleanly. `output` is what the model sees on its
    /// next inference call.
    ToolEnd {
        session_id: Uuid,
        agent: String,
        call_id: String,
        tool: String,
        output: String,
        truncated: bool,
    },

    /// Tool errored. The model sees this string as the tool result.
    /// `kind` distinguishes a bad call (the model's fault) from a bad
    /// outcome (the tool's fault) for the TUI's color treatment.
    ToolError {
        session_id: Uuid,
        agent: String,
        call_id: String,
        tool: String,
        error: String,
        kind: crate::engine::tool::ToolFailKind,
    },

    /// `task` invoked an interactive subagent; primary handoff begins.
    SubagentSpawned {
        session_id: Uuid,
        parent: String,
        child: String,
        prompt: String,
    },

    /// A subagent finished and emitted its report back to the parent.
    SubagentReport {
        session_id: Uuid,
        agent: String,
        report: String,
    },

    /// Provider-reported token usage for the round-trip that just
    /// finished. Emitted once per `model.complete` call; absent when
    /// the provider didn't include a usage chunk.
    Usage {
        session_id: Uuid,
        agent: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
    },

    /// A background coder paused with a question (GOALS §3b). Wire
    /// shape lands now; the dispatch logic that pauses turns ships
    /// in a later milestone.
    InterruptRaised {
        session_id: Uuid,
        interrupt_id: Uuid,
        agent: String,
        description: String,
        /// Legacy single-question payload (the `jobs` needs-attention
        /// nudge raises with neither field set). Kept for wire
        /// back-compat; new question-tool interrupts use `questions`.
        #[serde(default)]
        question: Option<InterruptQuestion>,
        /// Multi-question batch (GOALS §3b). Present when an agent's
        /// `question` tool raised the interrupt; drives the answering
        /// dialog. Mutually exclusive with `question` in practice.
        #[serde(default)]
        questions: Option<InterruptQuestionSet>,
    },

    /// An outstanding interrupt was resolved — emitted to every client
    /// attached to the session (forward-compat for multi-client per
    /// GOALS §8e; v1 single-client receives it as a no-op echo).
    InterruptResolved {
        session_id: Uuid,
        interrupt_id: Uuid,
    },

    /// The agent yielded control back to the human: the driver loop
    /// finished the current user message (and any folded queue) and is
    /// now awaiting input. Distinct from the mid-turn gaps where no
    /// model call is in flight (between tools, between inference
    /// rounds) — this fires only when the stack unwinds to the root and
    /// the queue is empty. The TUI keys its span-long "agent is
    /// working" indicator off the user-submit (rising) / this (falling)
    /// edges. Forward-compat: it means "no longer actively working," so
    /// a future agent that is *waiting* (agent-invoked timers/loops)
    /// emits it too.
    AgentIdle { session_id: Uuid },

    /// The primary (root-frame) agent was swapped in place (`/plan` →
    /// `Plan`, `/build` → `Build`, `plan.md §4.6.d`). The client chrome's
    /// active-agent slot tracks `name`.
    PrimarySwapped { session_id: Uuid, name: String },

    /// The session ended (user requested, daemon shutting down,
    /// crash recovery couldn't restore it, …).
    SessionEnded { session_id: Uuid, reason: String },

    /// An async job (loop / timer / background, GOALS §22) started.
    /// Drives the transient jobs strip. `kind` is `loop` / `timer` /
    /// `background`.
    JobStarted {
        session_id: Uuid,
        job_id: String,
        label: String,
        kind: String,
    },
    /// A background job produced output (liveness tick for the strip).
    JobProgress { session_id: Uuid, job_id: String },
    /// A note from an ephemeral-fork loop iteration. Shown live in the
    /// transcript; the model sees it in main context only at loop end.
    JobNote {
        session_id: Uuid,
        job_id: String,
        text: String,
    },
    /// An async job reached a terminal state (completed / failed /
    /// cancelled). Clears the strip entry + posts an inline marker; the
    /// model-facing result arrives separately as a late-arriving turn.
    JobCompleted {
        session_id: Uuid,
        job_id: String,
        label: String,
        kind: String,
        failed: bool,
    },

    /// Live "% prunable" projection for the foreground agent (GOALS §1a).
    /// `prunable_tokens` is the wire-token drop `/prune` would achieve
    /// right now, computed by the same `dedup_plan` `/prune` executes.
    /// The TUI divides by the model's max context for the status line.
    ContextProjection {
        session_id: Uuid,
        prunable_tokens: u64,
        cache_cold: bool,
    },

    /// A `/prune` completed (manual or cache-aware auto). UI marker.
    /// `elided` is the **current** full set of `original_event_id`s whose
    /// tool-result body is now a wire-side elision marker; the TUI dims the
    /// matching scrollback tool-result bodies by `call_id`. Render-time
    /// view of live wire state, not a persisted transcript flag (§14).
    Pruned {
        session_id: Uuid,
        auto: bool,
        bodies: usize,
        tokens_saved: u64,
        #[serde(default)]
        elided: Vec<String>,
    },

    /// A `/compact` handoff is assembled and a fresh session created.
    /// The TUI drops `handoff` into the composer for review, then
    /// re-attaches to `new_session_id` to commit.
    CompactReady {
        session_id: Uuid,
        new_session_id: Uuid,
        handoff: String,
        seed_tool_count: usize,
        seed_tool_tokens: u64,
    },

    /// Filesystem sandboxing was set/toggled for the session (`/sandbox`,
    /// sandboxing part 2). Broadcast to every attached client so they
    /// surface the resulting state (TUI: a toast).
    SandboxState { session_id: Uuid, enabled: bool },

    /// Caffeination (`/caffeinate`) turned on or off — including the
    /// daemon-decided `until-idle` auto-off. **Daemon-global**: carries no
    /// `session_id` and is broadcast to *every* connected client so the
    /// `☕` chrome glyph appears (and clears) on all of them in lockstep.
    /// `message` is `Some` for the originating client's toast; other
    /// clients use `active` to drive the glyph. `lid_close_guaranteed`
    /// lets a client word the lid-close caveat if it shows one.
    CaffeinateState {
        active: bool,
        lid_close_guaranteed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },

    /// The daemon began (or escalated) a graceful shutdown
    /// (`daemon-graceful-drain-shutdown.md`). **Daemon-global**: carries no
    /// `session_id` and is broadcast to *every* connected client so each
    /// TUI shows the drain notice and stops offering new input. `forced` is
    /// `false` when the drain just began (in-flight work is finishing) and
    /// `true` once the grace deadline was hit with work still outstanding,
    /// so a truncated turn isn't mistaken for a clean finish.
    DaemonDraining { forced: bool },
}

// ---- Errors ----------------------------------------------------------------

/// Structured error response. The model and the TUI both render
/// `message` directly; `code` lets the client branch on
/// machine-readable kinds without parsing the message.
#[derive(Debug, Clone, Serialize, Deserialize, Error)]
#[error("{code}: {message}")]
pub struct ErrorPayload {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Request payload didn't parse / failed validation.
    BadRequest,
    /// Daemon doesn't speak this protocol version.
    ProtocolVersion,
    /// No active session — `Attach` first.
    NotAttached,
    /// Session id unknown.
    UnknownSession,
    /// Interrupt id unknown / already resolved.
    UnknownInterrupt,
    /// Daemon is shutting down.
    Shutdown,
    /// Anything else.
    Internal,
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::BadRequest => "bad_request",
            Self::ProtocolVersion => "protocol_version",
            Self::NotAttached => "not_attached",
            Self::UnknownSession => "unknown_session",
            Self::UnknownInterrupt => "unknown_interrupt",
            Self::Shutdown => "shutdown",
            Self::Internal => "internal",
        };
        f.write_str(s)
    }
}

// ---- Shared payload types --------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum HistoryEntry {
    User {
        text: String,
    },
    Assistant {
        agent: String,
        text: String,
    },
    /// Tool calls appear inline in history so the TUI re-renders the
    /// turn faithfully on reconnect. The shape mirrors the
    /// `tool_call_events` row (GOALS §15b): the user transcript sees
    /// `original_input` and the recovery chip; the model on its next
    /// inference call sees `wire_input` (which equals
    /// `original_input` unless §12 repair or §13c cascade rewrite
    /// fired).
    ToolCall {
        agent: String,
        call_id: String,
        tool: String,
        original_input: Value,
        wire_input: Value,
        recovery_kind: Option<String>,
        recovery_stage: Option<String>,
        output: String,
        hard_fail: bool,
        truncated: bool,
    },
}

/// One session's live in-daemon status, from the per-session
/// `JobAuthority` + worker turn-state. Drives the browser's tiers 1-2
/// (GOALS §17f). Only emitted for sessions with a live worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveStatus {
    pub session_id: Uuid,
    /// At least one loop/timer/background job is live.
    pub has_active_jobs: bool,
    /// A turn is in flight (between `ThinkingStarted` and `AgentIdle`).
    pub processing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: Uuid,
    /// 6-char display id (GOALS §17b). Optional for backwards-compat
    /// with pre-§17 rows that haven't been backfilled yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<String>,
    pub project_root: String,
    pub project_id: String,
    pub started_at: i64,
    pub last_active_at: i64,
    pub turns: u32,
    pub active_agent: String,
    /// Auto- or user-set title (GOALS §17d). `None` until generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Parent session in the fork tree (§17e). `None` = root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<Uuid>,
    /// Number of direct forks. The `/sessions` browser renders
    /// `[N forks]` from this.
    #[serde(default)]
    pub fork_count: u32,
    /// Total descendant forks (depth-unbounded, excluding this session).
    /// The archive/delete confirm states this as the cascade count.
    #[serde(default)]
    pub descendant_count: u32,
    /// Epoch seconds the user last opened/resumed this session (GOALS
    /// §17f). `None` = never viewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_viewed_at: Option<i64>,
    /// Epoch seconds of the most recent agent-produced event (max across
    /// tool calls + inference). `None` = no agent activity yet. The
    /// browser marks a session unread when this is newer than
    /// `last_viewed_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_activity_at: Option<i64>,
    /// Count of open (unresolved) interrupts/questions for this session
    /// (`needs_attention`). Drives the "read, pending question" tier.
    #[serde(default)]
    pub open_interrupts: u32,
    /// Epoch seconds the session was archived (GOALS §17h). `None` = live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigLayer {
    /// `"home_xdg" | "home_dot" | "project"`.
    pub kind: String,
    /// On-disk path of this layer's `config.json` (or where it would
    /// be created — see GOALS §2c).
    pub path: String,
    /// `true` if the layer exists on disk; `false` if the daemon
    /// surfaced it as a "create at this level" placeholder.
    pub exists: bool,
    /// Raw layer contents (when `exists`). The TUI merges with
    /// neighbour layers on demand.
    #[serde(default)]
    pub contents: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub source: String,
}

/// One plan as the `/plans` browser sees it: list-row fields plus the
/// step count. Mirrors [`crate::db::plans::PlanSummary`] flattened for the
/// wire (status / isolation rendered as their stored string form).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSummaryWire {
    pub plan_id: Uuid,
    pub slug: String,
    pub title: String,
    pub description: String,
    /// `"pending" | "in_progress" | "done"`.
    pub status: String,
    pub base_branch: Option<String>,
    pub target_branch: Option<String>,
    pub step_count: i64,
    pub created_at: i64,
}

/// One step in a plan's DAG for the `/plans` drill-in. `depends_on` lists
/// the *titles* of the steps this one must run after (resolved daemon-side
/// from the dependency edges) so the browser can render prerequisites
/// without a second lookup. `tests` carries each test's phase + concurrency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStepWire {
    pub step_id: Uuid,
    pub title: String,
    /// `"pending" | "in_progress" | "done"`.
    pub status: String,
    /// Titles of the prerequisite steps (this step depends on them).
    pub depends_on: Vec<String>,
    pub tests: Vec<PlanTestWire>,
}

/// One per-step test for the `/plans` drill-in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTestWire {
    pub command: String,
    /// `"post_step" | "branch_stable"`.
    pub phase: String,
    /// `"parallel"`, or `"exclusive: <resource-key>"`.
    pub concurrency: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub name: String,
    pub description: String,
    pub mode: String,
    pub source: String,
    /// `true` for the built-in cast (`Build`, `coder`,
    /// `explore`, …).
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSummary {
    pub provider: String,
    pub id: String,
    pub display_name: Option<String>,
    pub favorite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", content = "data")]
pub enum InterruptQuestion {
    Single {
        prompt: String,
        options: Vec<InterruptOption>,
        #[serde(default = "default_allow_freetext")]
        allow_freetext: bool,
        /// Optional structured command-detail block (bash approval, §sandbox
        /// part 1). When present the answering dialog renders the full
        /// verbatim command beneath the heading, with the current step's
        /// constituent highlighted and a `step N of M` indicator for
        /// compound commands. Absent for every non-approval `Single`
        /// question, so the field is wire-equivalent to the legacy shape
        /// (back-compat: an un-annotated `Single` carries `None`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_detail: Option<CommandDetail>,
    },
    Multi {
        prompt: String,
        options: Vec<InterruptOption>,
        #[serde(default = "default_allow_freetext")]
        allow_freetext: bool,
    },
    Freetext {
        prompt: String,
    },
}

fn default_allow_freetext() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptOption {
    pub id: String,
    pub label: String,
    /// Optional one-line description rendered dimmed beneath the label.
    /// Absent for options the agent didn't annotate (back-compat: an
    /// un-annotated option is wire-equivalent to the legacy shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Structured detail for a bash-command approval prompt. Rides on a
/// [`InterruptQuestion::Single`] so the answering dialog can show the full
/// verbatim command beneath the (terse) heading and, for compound
/// commands, point at the constituent this prompt is deciding. Purely
/// presentational — the grant still keys on the heading's approval key,
/// never on this text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDetail {
    /// The full command string the agent proposed, verbatim.
    pub full_command: String,
    /// Char range `[start, end)` (0-based, end-exclusive) of the
    /// constituent this prompt decides, within `full_command`. `None` for
    /// a single-constituent command (no highlight) or when the parser
    /// could not place the constituent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight: Option<CharSpan>,
    /// 1-based position of this prompt among the constituents that
    /// actually prompt, and the total count of such constituents. `(1, 1)`
    /// for a single-prompt command, which the dialog renders with no `step`
    /// indicator.
    pub step: u32,
    pub step_count: u32,
}

/// A 0-based, end-exclusive char range into a source string. Char-indexed
/// (not byte-indexed) so multi-byte input slices correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CharSpan {
    pub start: u32,
    pub end: u32,
}

/// A batch of one or more questions raised in a single interrupt. The
/// `question` tool (GOALS §3b) carries an array of questions in one
/// call because tool dispatch is sequential and structural tools drop
/// the rest of the turn — so everything the agent needs has to ride in
/// one interrupt. Each entry reuses [`InterruptQuestion`], so a
/// single-question batch is wire-equivalent to the legacy shape (the
/// answering UI and the resolution path treat `[q]` and a bare `q`
/// identically).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptQuestionSet {
    pub questions: Vec<InterruptQuestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", content = "data")]
pub enum ResolveResponse {
    Single {
        selected_id: String,
    },
    Multi {
        selected_ids: Vec<String>,
    },
    Freetext {
        text: String,
    },
    /// One answer per question in an [`InterruptQuestionSet`], in the
    /// same order the questions were posed. Each entry is a `Single` /
    /// `Multi` / `Freetext` — never a nested `Batch` or `Cancel`. The
    /// `question` tool maps these back to its result array; a
    /// single-question batch may equally arrive as a bare `Single` /
    /// `Multi` / `Freetext` (the resolver unwraps both shapes).
    Batch {
        responses: Vec<ResolveResponse>,
    },
    /// User dismissed the interrupt without answering. The agent
    /// receives an empty resolution and decides how to proceed.
    Cancel,
}

impl ResolveResponse {
    /// Normalize a resolution into the per-question answer list a
    /// [`InterruptQuestionSet`] of `n` questions expects. `Batch` is
    /// returned as-is; a bare single-question answer wraps to a
    /// one-element list; `Cancel` fans out to `n` `Cancel`s so every
    /// question reads as dismissed.
    pub fn into_batch(self, n: usize) -> Vec<ResolveResponse> {
        match self {
            ResolveResponse::Batch { responses } => responses,
            ResolveResponse::Cancel => std::iter::repeat_n(ResolveResponse::Cancel, n).collect(),
            other => vec![other],
        }
    }
}

// ---- Codec -----------------------------------------------------------------

/// NDJSON framed codec over an arbitrary byte stream. Use the same
/// type for both ends — the schema is symmetric, only the legal
/// `Body` variants differ per direction.
pub struct ProtoStream<S> {
    framed: Framed<S, LinesCodec>,
}

impl<S> ProtoStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    pub fn new(stream: S) -> Self {
        Self {
            framed: Framed::new(stream, LinesCodec::new_with_max_length(MAX_FRAME_BYTES)),
        }
    }

    /// Send one envelope. Serializes to a compact single-line JSON
    /// string and writes a trailing newline (`LinesCodec` adds the
    /// newline).
    pub async fn send(&mut self, env: &Envelope) -> Result<()> {
        let line = serde_json::to_string(env).context("serializing envelope")?;
        self.framed
            .send(line)
            .await
            .map_err(codec_error)
            .context("writing envelope")?;
        Ok(())
    }

    /// Receive the next envelope. Returns `Ok(None)` on clean EOF;
    /// returns `Err` on framing failure (frame too large, invalid UTF-8)
    /// or JSON deserialization failure.
    pub async fn recv(&mut self) -> Result<Option<Envelope>> {
        match self.framed.next().await {
            None => Ok(None),
            Some(Err(e)) => Err(codec_error(e)).context("reading envelope"),
            Some(Ok(line)) => {
                let env: Envelope =
                    serde_json::from_str(&line).context("deserializing envelope")?;
                if env.v != PROTOCOL_VERSION {
                    anyhow::bail!(
                        "wire protocol version mismatch: peer sent v{}, this binary speaks v{}",
                        env.v,
                        PROTOCOL_VERSION
                    );
                }
                Ok(Some(env))
            }
        }
    }
}

fn codec_error(err: LinesCodecError) -> io::Error {
    match err {
        LinesCodecError::Io(e) => e,
        LinesCodecError::MaxLineLengthExceeded => io::Error::new(
            io::ErrorKind::InvalidData,
            "NDJSON frame exceeded MAX_FRAME_BYTES",
        ),
    }
}

// ---- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::duplex;

    #[test]
    fn request_round_trip() {
        let env = Envelope::request(
            Uuid::new_v4(),
            Request::SendUserMessage {
                text: "hello".into(),
                images: Vec::new(),
            },
        );
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        match back.body {
            Body::Request {
                request: Request::SendUserMessage { text, .. },
                ..
            } => assert_eq!(text, "hello"),
            other => panic!("expected SendUserMessage, got {other:?}"),
        }
    }

    #[test]
    fn event_round_trip() {
        let sid = Uuid::new_v4();
        let env = Envelope::event(Event::AssistantTextDelta {
            session_id: sid,
            agent: "coder".into(),
            delta: "patch ".into(),
        });
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        match back.body {
            Body::Event {
                event:
                    Event::AssistantTextDelta {
                        session_id,
                        agent,
                        delta,
                    },
            } => {
                assert_eq!(session_id, sid);
                assert_eq!(agent, "coder");
                assert_eq!(delta, "patch ");
            }
            other => panic!("expected AssistantTextDelta, got {other:?}"),
        }
    }

    #[test]
    fn error_with_null_id() {
        let env = Envelope::error(
            None,
            ErrorPayload {
                code: ErrorCode::Shutdown,
                message: "daemon shutting down".into(),
            },
        );
        let s = serde_json::to_string(&env).unwrap();
        assert!(s.contains("\"id\":null"));
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back.body,
            Body::Error {
                id: None,
                error: ErrorPayload {
                    code: ErrorCode::Shutdown,
                    ..
                }
            }
        ));
    }

    #[test]
    fn session_live_status_round_trip() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let env = Envelope::request(
            Uuid::new_v4(),
            Request::SessionLiveStatus {
                session_ids: vec![a, b],
            },
        );
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        match back.body {
            Body::Request {
                request: Request::SessionLiveStatus { session_ids },
                ..
            } => assert_eq!(session_ids, vec![a, b]),
            other => panic!("expected SessionLiveStatus, got {other:?}"),
        }

        // Response side.
        let res = Envelope::response(
            Uuid::new_v4(),
            Response::SessionLiveStatus {
                statuses: vec![LiveStatus {
                    session_id: a,
                    has_active_jobs: true,
                    processing: false,
                }],
            },
        );
        let s = serde_json::to_string(&res).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        match back.body {
            Body::Response {
                response: Response::SessionLiveStatus { statuses },
                ..
            } => {
                assert_eq!(statuses.len(), 1);
                assert!(statuses[0].has_active_jobs);
                assert!(!statuses[0].processing);
            }
            other => panic!("expected SessionLiveStatus response, got {other:?}"),
        }
    }

    #[test]
    fn set_caffeinate_round_trip() {
        use crate::daemon::caffeinate::CaffeinateMode;

        // Request side: each mode survives the wire.
        for mode in [
            CaffeinateMode::Toggle,
            CaffeinateMode::On,
            CaffeinateMode::Off,
            CaffeinateMode::UntilIdle,
        ] {
            let env = Envelope::request(Uuid::new_v4(), Request::SetCaffeinate { mode });
            let s = serde_json::to_string(&env).unwrap();
            let back: Envelope = serde_json::from_str(&s).unwrap();
            match back.body {
                Body::Request {
                    request: Request::SetCaffeinate { mode: got },
                    ..
                } => assert_eq!(got, mode),
                other => panic!("expected SetCaffeinate, got {other:?}"),
            }
        }
        // `until-idle` serializes as snake_case `until_idle`.
        let env = Envelope::request(
            Uuid::new_v4(),
            Request::SetCaffeinate {
                mode: CaffeinateMode::UntilIdle,
            },
        );
        let v: Value = serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
        assert_eq!(v["params"]["mode"], json!("until_idle"));

        // Response side carries the honest message + lid-close flag.
        let res = Envelope::response(
            Uuid::new_v4(),
            Response::CaffeinateState {
                active: true,
                lid_close_guaranteed: false,
                message: "caffeinate on — note: lid-close not guaranteed".into(),
            },
        );
        let back: Envelope = serde_json::from_str(&serde_json::to_string(&res).unwrap()).unwrap();
        match back.body {
            Body::Response {
                response:
                    Response::CaffeinateState {
                        active,
                        lid_close_guaranteed,
                        message,
                    },
                ..
            } => {
                assert!(active);
                assert!(!lid_close_guaranteed);
                assert!(message.contains("note:"));
            }
            other => panic!("expected CaffeinateState response, got {other:?}"),
        }

        // Event side is the daemon-global broadcast (no session_id, no
        // message for non-originating clients).
        let evt = Envelope::event(Event::CaffeinateState {
            active: false,
            lid_close_guaranteed: false,
            message: None,
        });
        let back: Envelope = serde_json::from_str(&serde_json::to_string(&evt).unwrap()).unwrap();
        match back.body {
            Body::Event {
                event:
                    Event::CaffeinateState {
                        active, message, ..
                    },
            } => {
                assert!(!active);
                assert!(message.is_none());
            }
            other => panic!("expected CaffeinateState event, got {other:?}"),
        }
    }

    #[test]
    fn interrupt_question_serializes_as_tagged() {
        let q = InterruptQuestion::Single {
            prompt: "Backfill strategy?".into(),
            options: vec![
                InterruptOption {
                    id: "now".into(),
                    label: "Backfill now".into(),
                    description: None,
                },
                InterruptOption {
                    id: "later".into(),
                    label: "Defer".into(),
                    description: None,
                },
            ],
            allow_freetext: true,
            command_detail: None,
        };
        let s = serde_json::to_string(&q).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], json!("single"));
        assert_eq!(v["data"]["options"].as_array().unwrap().len(), 2);
        // A `None` command_detail is omitted from the wire (back-compat).
        assert!(v["data"].get("command_detail").is_none());
    }

    #[test]
    fn command_detail_round_trips_and_is_additive() {
        // A populated command_detail survives the wire and an old-shape
        // `Single` (no command_detail key) still deserializes.
        let q = InterruptQuestion::Single {
            prompt: "Run `cargo build`?".into(),
            options: vec![InterruptOption {
                id: "once".into(),
                label: "Yes, once".into(),
                description: None,
            }],
            allow_freetext: false,
            command_detail: Some(CommandDetail {
                full_command: "git push && cargo build".into(),
                highlight: Some(CharSpan { start: 11, end: 22 }),
                step: 2,
                step_count: 2,
            }),
        };
        let s = serde_json::to_string(&q).unwrap();
        let back: InterruptQuestion = serde_json::from_str(&s).unwrap();
        match back {
            InterruptQuestion::Single { command_detail, .. } => {
                let cd = command_detail.expect("command_detail survives");
                assert_eq!(cd.full_command, "git push && cargo build");
                assert_eq!(cd.highlight, Some(CharSpan { start: 11, end: 22 }));
                assert_eq!((cd.step, cd.step_count), (2, 2));
            }
            other => panic!("expected Single, got {other:?}"),
        }

        // Legacy shape (no command_detail field) deserializes to `None`.
        let legacy = json!({
            "kind": "single",
            "data": {
                "prompt": "Run `ls`?",
                "options": [{ "id": "once", "label": "Yes, once" }],
                "allow_freetext": false
            }
        });
        let back: InterruptQuestion = serde_json::from_value(legacy).unwrap();
        match back {
            InterruptQuestion::Single { command_detail, .. } => {
                assert!(command_detail.is_none());
            }
            other => panic!("expected Single, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn codec_round_trip_over_duplex() {
        let (a, b) = duplex(64 * 1024);
        let mut left = ProtoStream::new(a);
        let mut right = ProtoStream::new(b);

        let id = Uuid::new_v4();
        let out = Envelope::request(id, Request::DaemonStatus);
        left.send(&out).await.unwrap();

        let got = right.recv().await.unwrap().expect("EOF unexpected");
        match got.body {
            Body::Request {
                id: got_id,
                request: Request::DaemonStatus,
            } => assert_eq!(got_id, id),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn codec_rejects_wrong_version() {
        let (a, b) = duplex(4096);
        let mut left = ProtoStream::new(a);
        let mut right = ProtoStream::new(b);

        // Bypass the helper to inject a bad version.
        let bad = serde_json::json!({
            "v": 999,
            "kind": "req",
            "id": Uuid::new_v4(),
            "request": "daemon_status",
            "params": null,
        });
        let line = serde_json::to_string(&bad).unwrap();
        left.framed.send(line).await.unwrap();
        let err = right.recv().await.unwrap_err();
        assert!(format!("{err:#}").contains("wire protocol version mismatch"));
    }
}
