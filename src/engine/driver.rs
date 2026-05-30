//! Multi-agent conversation driver.
//!
//! Holds a stack of `AgentSession`s — one per active agent in the
//! current invocation tree. The user always talks to the agent on top
//! of the stack. On a `task` tool call, the driver pushes a new
//! subagent; when that subagent finishes (final text + no tool calls
//! and the parent has an outstanding task call), the driver pops it
//! and delivers the subagent's text as the parent's tool result.
//!
//! This is the v0 implementation of GOALS §3b's *interactive subagent*:
//! the primary-agent identity swaps every time the stack height
//! changes, and the user's messages route to whoever's on top.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::engine::agent::{Agent, TurnEvent, TurnOutcome, turn};
use crate::engine::jobs::{JobAuthority, JobCommand, JobEvent};
use crate::engine::message::{Message, UserSubmission};
use crate::engine::prune;
use crate::redact::RedactionTable;
use crate::session::Session;

/// Out-of-band control requests routed to the driver from the daemon
/// worker — `/prune`, `/compact`, `/pin`. Drained on the same boundary
/// as user input and job events so they never interleave with a
/// mid-turn state (the safe-boundary rule, `plan.md` T6.e).
#[derive(Debug)]
pub enum DriverControl {
    /// Run snapshot dedup on the foreground agent now. `confirmed` is
    /// always true here — the confirm UX lives in the TUI; by the time a
    /// `Prune` reaches the driver the user has already accepted the
    /// before→after numbers.
    Prune,
    /// Assemble a `/compact` handoff for the foreground agent: prune
    /// first (fixed ordering), draft the model brief, append the
    /// deterministic appendix, derive seed-tools, create a fresh session,
    /// and emit `CompactReady`.
    Compact,
    /// Pin a user message verbatim for the next `/compact` (`/pin`).
    Pin { text: String },
    /// Swap the **primary** (root-frame) agent in place (`/plan` → `Plan`,
    /// `/build` → `Build`, `plan.md §4.6.d`). Handled at the idle boundary
    /// like other control requests; the root history is preserved so the
    /// new primary continues the same conversation with its own tool
    /// surface + system prompt. A no-op when an interactive subagent holds
    /// the foreground (stack depth > 1) or the name is already active.
    SwapPrimary { name: String },
    /// Switch the active `llm_mode` live (`/llm-mode`,
    /// `prompts/llm-modes-defensive-normal.md`). Rebuilds the root-frame
    /// agent so its tool-description verbosity + per-mode prompt re-render;
    /// busts the cached system prefix (the TUI shows the cache-break warning
    /// via the shared helper, suppressed on a no-cache provider). Root
    /// history is preserved — same conversation, new steering. A no-op when
    /// an interactive subagent holds the foreground or the mode is unchanged.
    /// `mode = None` toggles against the driver's authoritative current value
    /// (the `/llm-mode` / `toggle` default action); `Some(_)` sets it
    /// explicitly.
    SetLlmMode {
        mode: Option<crate::config::extended::LlmMode>,
    },
}

/// Maximum number of queued user messages to fold into a single
/// follow-up prompt. Generous because the worst case is a user
/// hammering Enter — concat-joining a dozen short messages is fine;
/// concat-joining a hundred would bloat the next inference. If we
/// hit this cap, extras stay in the channel for the *next* fold.
const MAX_FOLD: usize = 16;

/// Handle the session worker keeps to cancel the in-flight user-message
/// run on a ctrl+c (`SessionWork::Cancel`). Shares the driver's
/// `cancel_current` slot; cancelling the live token aborts the in-flight
/// inference and signals any running `bash` subprocess to die. Idempotent
/// and safe at idle — when no run is in flight the slot is `None` and
/// [`Self::cancel`] is a no-op.
#[derive(Clone)]
pub struct CancelHandle {
    current: Arc<std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>>,
}

impl CancelHandle {
    /// Cancel the in-flight run, if any. Safe to call when idle (no-op),
    /// when already cancelling (cancelling a cancelled token is a no-op),
    /// and concurrently from multiple callers.
    pub fn cancel(&self) {
        if let Ok(slot) = self.current.lock()
            && let Some(token) = slot.as_ref()
        {
            token.cancel();
        }
    }
}

/// RAII guard that clears the driver's `cancel_current` slot when a
/// user-message run ends (any exit path). Ensures a finished run's token
/// can never be cancelled by a late ctrl+c that should instead arm a
/// fresh first press.
struct CancelSlotGuard {
    slot: Arc<std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>>,
}

impl Drop for CancelSlotGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = None;
        }
    }
}

/// One agent's slice of state on the driver stack.
pub struct AgentSession {
    pub agent: Arc<Agent>,
    pub history: Vec<Message>,
    /// When this session was pushed by a parent's `task` tool, the
    /// parent's outstanding tool-call id (we have to answer it when we
    /// pop). `None` for the root session.
    pub answering: Option<PendingTaskCall>,
    /// This frame's deferred-log buffer (`plan.md §3d`). A subagent's
    /// `defer_to_orchestrator` calls append here; on pop the driver drains
    /// it and folds it into the report the parent ingests. The root frame's
    /// buffer is never read (the root has no parent to defer to).
    pub deferred_log: crate::engine::deferred::DeferredLog,
}

#[derive(Debug, Clone)]
pub struct PendingTaskCall {
    pub call_id: String,
    pub function_call_id: Option<String>,
}

pub struct Driver {
    pub session: Arc<Session>,
    pub locks: Arc<crate::locks::LockManager>,
    pub redact: Arc<RedactionTable>,
    pub cwd: std::path::PathBuf,
    pub stack: Vec<AgentSession>,
    /// Minutes between `[time: ...]` preludes injected on user
    /// messages (GOALS §17g). Loaded from
    /// `extended.system_prompt.time_injection_interval_minutes`;
    /// defaults to 5 if unset.
    pub time_injection_interval_minutes: u32,
    /// Back-to-back identical tool-call threshold for the loop guard
    /// (GOALS §1/§12): the number of consecutive identical calls before
    /// the approval prompt fires. Loaded from
    /// `extended.loop_guard.repeat_threshold` (default 2 = fire on the
    /// first exact repeat); set via [`Self::set_loop_guard_threshold`]
    /// before the loop starts.
    pub loop_guard_threshold: u32,
    /// The single async-job authority (GOALS §22). Owns the live-jobs
    /// registry + per-job tasks; the driver is the one place that mutates
    /// it (single-authority rule).
    pub jobs: JobAuthority,
    /// Job events drained at the turn boundary (loop-iteration-due,
    /// terminal completions). Same boundary as the user-input queue.
    job_event_rx: mpsc::Receiver<JobEvent>,
    /// Self-command channel for in-task timers to re-arm. The driver
    /// drains it alongside job events.
    job_cmd_rx: mpsc::Receiver<JobCommand>,
    /// Which cache-safe capability hints have already been appended to the
    /// active history (GOALS §22). A branch is enabled by two cache-safe
    /// moves: the dispatcher starts accepting the action (always, here),
    /// and a hint message is appended **once** announcing it — appended
    /// messages extend the cached prefix without reserializing the
    /// byte-stable tools array. We append the hint the first time the
    /// gating job kind appears.
    appended_hints: std::collections::HashSet<&'static str>,
    /// Per-foreground-agent "last prune watermark" (GOALS §10): the
    /// foreground history length at the last auto-prune. The cache-aware
    /// auto-prune short-circuits when the foreground history hasn't grown
    /// since — nothing new can be prunable. Keyed by stack depth so an
    /// interactive subagent's watermark doesn't bleed into the parent's.
    prune_watermark: std::collections::HashMap<usize, usize>,
    /// Re-executed seed-tool context for a `/compact` fresh session
    /// (T6.e). Set by [`Self::run_seed_tools`] before the loop starts;
    /// prepended to the **first** user message so the fresh agent's first
    /// inference carries the live working set, then cleared. Avoids two
    /// consecutive user messages on the wire.
    pending_seed_context: Option<String>,
    /// Interrupt wakeup hub (GOALS §3b) threaded into every tool call so
    /// the `question` tool can block on a human answer. Defaults to a
    /// [`detached`](crate::engine::interrupt::InterruptHub::detached) hub
    /// (no client fan-out); the session worker swaps in the client-wired
    /// one via [`Self::set_interrupt_hub`] before the loop starts, and
    /// keeps the same `Arc` so its `ResolveInterrupt` handler can wake
    /// the blocked tool.
    interrupts: Arc<crate::engine::interrupt::InterruptHub>,
    /// One-shot guard for the "skills auto-selection skipped: no
    /// utility_model" notice (GOALS §5). Logged at most once per driver
    /// so an unconfigured utility model doesn't spam the log every turn.
    skills_no_utility_model_logged: bool,
    /// Cancellation handle for the in-flight user-message run (ctrl+c →
    /// `CancelTurn`, GOALS §3a). `run_user_input` installs a fresh
    /// [`CancellationToken`] here at the start of each run and clears it on
    /// exit; the session worker holds a clone of the `Arc` so a
    /// `SessionWork::Cancel` can read the live token and fire it. `None`
    /// when idle — cancelling then is a safe no-op. Threaded into every
    /// `turn()` (to abort the in-flight inference) and `ToolCtx` (to kill a
    /// long-running `bash` subprocess) within the run.
    cancel_current: Arc<std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>>,
    /// Command/path approval driver (sandboxing part 2). Threaded into
    /// every [`ToolCtx`] so `bash`'s run-fail-escalate and the native
    /// tools' out-of-boundary path checks can prompt + remember. `None`
    /// until the session worker installs it via
    /// [`Self::set_approver`] before the loop starts (same shape as the
    /// interrupt hub); seed-tool re-execution before that runs with no
    /// approver (skips the prompt, never denies).
    approver: Option<Arc<crate::approval::Approver>>,
    /// Compact-after-delegation trackers for **interactive** subagent
    /// delegations (`SpawnSubagent`), keyed by the paused parent frame's
    /// stack depth (its index in `self.stack`). The lazy shrink for the
    /// parent runs in a background task whose handle rides alongside the
    /// tracker; on the child's `Done` pop we resolve full-vs-shrunk for the
    /// now-top parent frame (`prompts/compact-after-delegation.md`). A
    /// `Vec` indexed by depth would also work, but the map makes the
    /// "no tracker at this depth" case explicit.
    deleg_shrinks: std::collections::HashMap<usize, PendingDelegationShrink>,
}

/// An in-flight compact-after-delegation: the decision tracker plus the
/// background shrink task's join handle (`None` once joined, or when the
/// shrink was synchronous). Held per delegation so the parent can resolve
/// full-vs-shrunk on the sub-agent's return.
struct PendingDelegationShrink {
    tracker: crate::engine::deleg_shrink::DelegationShrink,
    handle: Option<tokio::task::JoinHandle<Vec<Message>>>,
}

/// Inbound channel capacity for job events / commands. Generous; job
/// lifecycle traffic is tiny.
const JOB_CHANNEL_CAPACITY: usize = 256;

impl Driver {
    pub fn new(
        session: Arc<Session>,
        locks: Arc<crate::locks::LockManager>,
        redact: Arc<RedactionTable>,
        cwd: std::path::PathBuf,
        root: Arc<Agent>,
    ) -> Self {
        Self::with_max_jobs(
            session,
            locks,
            redact,
            cwd,
            root,
            crate::engine::jobs::DEFAULT_MAX_CONCURRENT_JOBS,
        )
    }

    /// Build a driver with a configurable max-concurrent-jobs cap (GOALS
    /// §22). The authority's [`JobContext`] is rooted on `root` — the
    /// agent ephemeral-fork loops run on (same model/provider config).
    pub fn with_max_jobs(
        session: Arc<Session>,
        locks: Arc<crate::locks::LockManager>,
        redact: Arc<RedactionTable>,
        cwd: std::path::PathBuf,
        root: Arc<Agent>,
        max_concurrent_jobs: usize,
    ) -> Self {
        let (job_event_tx, job_event_rx) = mpsc::channel::<JobEvent>(JOB_CHANNEL_CAPACITY);
        let (job_cmd_tx, job_cmd_rx) = mpsc::channel::<JobCommand>(JOB_CHANNEL_CAPACITY);
        let ctx = crate::engine::jobs::authority::JobContext {
            session: session.clone(),
            locks: locks.clone(),
            redact: redact.clone(),
            cwd: cwd.clone(),
            agent: root.clone(),
        };
        // The authority needs the engine UI-event channel (`tx`) to emit
        // started/progress/note signals, but `tx` isn't known until
        // `run_main_loop`. Build with a dummy sender now; `run_main_loop`
        // rebinds it via [`JobAuthority::set_turn_tx`] before any job can
        // start, so no UI signal is ever lost.
        let (dummy_tx, _dummy_rx) = mpsc::channel::<TurnEvent>(1);
        let jobs = JobAuthority::new(job_event_tx, job_cmd_tx, dummy_tx, ctx, max_concurrent_jobs);
        Self {
            session,
            locks,
            redact,
            cwd,
            stack: vec![AgentSession {
                agent: root,
                history: Vec::new(),
                answering: None,
                deferred_log: crate::engine::deferred::DeferredLog::new(),
            }],
            time_injection_interval_minutes: 5,
            loop_guard_threshold: crate::config::extended::MIN_LOOP_GUARD_THRESHOLD,
            jobs,
            job_event_rx,
            job_cmd_rx,
            appended_hints: std::collections::HashSet::new(),
            prune_watermark: std::collections::HashMap::new(),
            pending_seed_context: None,
            interrupts: Arc::new(crate::engine::interrupt::InterruptHub::detached()),
            skills_no_utility_model_logged: false,
            cancel_current: Arc::new(std::sync::Mutex::new(None)),
            approver: None,
            deleg_shrinks: std::collections::HashMap::new(),
        }
    }

    /// Swap in the session worker's client-wired interrupt hub (GOALS
    /// §3b) before the main loop starts. The worker keeps the same
    /// `Arc` so its `ResolveInterrupt` handler wakes whatever tool call
    /// is blocked on the answer. Same shape as [`JobAuthority`]'s
    /// `set_turn_tx`: the channel-bearing dependency isn't known at
    /// construction.
    pub fn set_interrupt_hub(&mut self, hub: Arc<crate::engine::interrupt::InterruptHub>) {
        self.interrupts = hub;
    }

    /// Install the command/path approval driver (sandboxing part 2)
    /// before the main loop starts. The session worker builds it with the
    /// session's grant store + the client-wired interrupt hub, so the
    /// approval prompt fans out to the attached client exactly like a
    /// `question`. Must be set after [`Self::set_interrupt_hub`] (the
    /// approver captures the same hub).
    pub fn set_approver(&mut self, approver: Arc<crate::approval::Approver>) {
        self.approver = Some(approver);
    }

    /// Set the loop-guard threshold (GOALS §1/§12) from the layered
    /// config before the loop starts. Clamped to a minimum of 2 — the
    /// guard only fires on a *repeat*, so a smaller value is meaningless.
    pub fn set_loop_guard_threshold(&mut self, threshold: u32) {
        self.loop_guard_threshold =
            threshold.max(crate::config::extended::MIN_LOOP_GUARD_THRESHOLD);
    }

    /// Wrap `user_text` with the `[time: ...]` prelude when the
    /// session's interval has elapsed. Side-effect: stamps the
    /// session's last-prelude timestamp on success. No-op when the
    /// interval hasn't elapsed.
    fn with_time_prelude(&self, user_text: String) -> String {
        match self
            .session
            .take_time_prelude(self.time_injection_interval_minutes)
        {
            Some(prelude) => format!("{prelude}\n\n{user_text}"),
            None => user_text,
        }
    }

    /// Skills auto-selection seam (GOALS §5). Loads the layered config,
    /// consults the cheap utility model with the skill catalog + the
    /// user message, and—if a skill is selected—returns `user_text` with
    /// the (`!`-processed, scrubbed) skill body prepended so the main
    /// agent's first inference carries it. Returns `user_text` unchanged
    /// when no skill is chosen.
    ///
    /// Graceful degradation: an unset `utility_model` skips the pass
    /// (logged at most once) and returns `user_text` untouched — no
    /// error, no main-model fallback. The cheap model only ever sees the
    /// `(name, description)` catalog (token economy, GOALS §10).
    async fn maybe_inject_skill(&mut self, user_text: &str) -> String {
        let (extended, providers) = crate::auto_title::load_configs_for(&self.cwd);

        if extended.utility_model.is_none() {
            if !self.skills_no_utility_model_logged {
                self.skills_no_utility_model_logged = true;
                tracing::info!("skills auto-selection skipped: no `utility_model` configured");
            }
            return user_text.to_string();
        }

        let selection = crate::skills::auto_select::select(
            &self.cwd,
            &extended,
            &providers,
            &self.redact,
            user_text,
        )
        .await;

        match selection {
            crate::skills::auto_select::Selection::Skill { name, body } => {
                tracing::debug!(skill = %name, "skills auto-selection injected skill body");
                format!("Skill `{name}` (auto-selected):\n\n{body}\n\n---\n\n{user_text}")
            }
            crate::skills::auto_select::Selection::None => user_text.to_string(),
        }
    }

    /// Name of the agent currently holding the user's conversation.
    /// Used by the TUI for the active-agent slot.
    pub fn active_agent(&self) -> &str {
        self.stack
            .last()
            .map(|a| a.agent.name.as_str())
            .unwrap_or("")
    }

    /// A sender into the async-job command channel (GOALS §22). The
    /// session worker keeps a clone so a **human** cancel (`/jobs cancel
    /// <id>`, "stop checking the deploy") reaches the single async-job
    /// authority on the same boundary as model-issued commands. Drained
    /// in [`Self::run_main_loop`].
    pub fn job_command_sender(&self) -> mpsc::Sender<JobCommand> {
        self.jobs.command_sender()
    }

    /// A handle the session worker keeps so a user ctrl+c
    /// (`SessionWork::Cancel`) can abort the in-flight user-message run.
    /// Cheap to clone — it shares the driver's `cancel_current` slot. See
    /// [`CancelHandle::cancel`].
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            current: self.cancel_current.clone(),
        }
    }

    /// Long-running main loop: pulls user input from `input_rx` and
    /// drives it through the agent stack, **folding queued user
    /// messages** (GOALS §1c) at every inference boundary. The fold
    /// runs `try_recv` until the channel is empty, joins the
    /// collected texts with a blank line, and uses that as the next
    /// inference's user content.
    ///
    /// Per GOALS §1c, the queue is delivered at the *next inference
    /// call* — not the next user turn. Mid-tool-loop: the next
    /// tool-result → inference round-trip carries the queue alongside
    /// the tool result. End-of-turn: the queue is delivered as the
    /// first content of the next request. Empty queue: standard
    /// behavior.
    pub async fn run_main_loop(
        &mut self,
        mut input_rx: mpsc::Receiver<UserSubmission>,
        mut control_rx: mpsc::Receiver<DriverControl>,
        tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        // Rebind the async-job authority's UI-event channel now that we
        // have `tx`. Done before the first message so no job can start
        // (and thus emit a started/progress signal) beforehand.
        self.jobs.set_turn_tx(tx.clone());

        loop {
            // Wait for the next thing to do: a user message, a control
            // request (/prune /compact /pin), a job event (loop iteration
            // due / job completed), or a job command (an in-task timer
            // re-arm). Async results inject "as a late-arriving turn at
            // the next turn boundary" — at idle, the next boundary is
            // right here.
            tokio::select! {
                msg = input_rx.recv() => {
                    let Some(first) = msg else { break };
                    // Fold anything else that's already queued behind the
                    // first message (rare but harmless).
                    let mut batch = vec![first];
                    drain_queue(&mut input_rx, &mut batch);
                    // Fold texts (scrubbed) and collect image parts in
                    // order. Image bytes bypass redaction — they're raw
                    // PNG, not env-scannable text — so only the text side
                    // goes through `scrub`.
                    let folded = fold_submissions(batch);
                    let submission = UserSubmission {
                        text: self.redact.scrub(&folded.text),
                        images: folded.images,
                    };
                    self.run_user_input(submission, &mut input_rx, tx).await?;
                }
                ctl = control_rx.recv() => {
                    match ctl {
                        // Control requests arrive at idle (the stack is at
                        // the foreground agent and no turn is in flight) —
                        // a safe compaction boundary by construction.
                        Some(control) => self.run_control(control, tx).await,
                        None => break,
                    }
                }
                ev = self.job_event_rx.recv() => {
                    match ev {
                        Some(event) => self.run_job_event(event, &mut input_rx, tx).await?,
                        None => break,
                    }
                }
                cmd = self.job_cmd_rx.recv() => {
                    if let Some(cmd) = cmd {
                        self.jobs.handle_command(cmd);
                        continue;
                    } else {
                        break;
                    }
                }
            }
            // Stack has unwound to the root and the queue is drained —
            // the agent is idle until the next message. Emit the falling
            // edge so the TUI can stop its working-indicator clock, and
            // refresh the "% prunable" projection from the now-settled
            // foreground history.
            self.emit_context_projection(tx).await;
            let _ = tx.send(TurnEvent::AgentIdle).await;
        }
        Ok(())
    }

    /// Whether the conversation is at a safe boundary for context
    /// reduction (`plan.md` T6.e). The driver evaluates control requests
    /// and auto-prune only at the inference boundary (between tool loops
    /// / at idle), where by construction no tool call is mid-dispatch and
    /// the foreground agent is the one being targeted. The remaining
    /// concern is an interactive subagent: pruning/compacting always
    /// targets the **top** of the stack (the foreground agent), so a
    /// deeper frame is never touched — the predicate is consulted to keep
    /// the contract explicit and to gate the auto-fire.
    fn at_safe_boundary(&self) -> bool {
        // No tool call is in flight at the call sites that consult this
        // (idle / inference boundary); no pending user interaction model
        // exists in v1. The only live concern is captured by always
        // operating on `stack.last_mut()`.
        crate::engine::is_at_safe_compaction_boundary(false, false, false)
    }

    /// Run an out-of-band control request against the **foreground**
    /// agent (top of stack) — never a hardcoded root. Scope == current
    /// conversational agent (GOALS §3b).
    async fn run_control(&mut self, control: DriverControl, tx: &mpsc::Sender<TurnEvent>) {
        if !self.at_safe_boundary() {
            // Not safe — drop rather than corrupt the transcript split.
            // The TUI re-issues on the next idle (control requests are
            // user-initiated, so a retry is a keystroke away). v1 reaches
            // here only at idle, so this is defensive.
            tracing::warn!("control request at unsafe boundary; ignoring");
            return;
        }
        match control {
            DriverControl::Prune => {
                self.do_prune(false, tx).await;
            }
            DriverControl::Compact => {
                self.do_compact(tx).await;
            }
            DriverControl::Pin { text } => {
                self.session.pin_message(&text);
            }
            DriverControl::SwapPrimary { name } => {
                self.swap_primary(&name, tx).await;
            }
            DriverControl::SetLlmMode { mode } => {
                self.set_llm_mode(mode, tx).await;
            }
        }
    }

    /// Swap the root-frame agent to `name` in place, preserving the root
    /// history so the new primary continues the same conversation. Only the
    /// root frame is swapped, and only at idle (the control boundary) — a
    /// deeper interactive subagent frame is never touched. No-op when an
    /// interactive subagent holds the foreground or the agent is already
    /// active. The new agent is built through [`crate::engine::builtin::load`]
    /// so a user override of `Plan`/`Build` takes effect.
    async fn swap_primary(&mut self, name: &str, tx: &mpsc::Sender<TurnEvent>) {
        if self.stack.len() != 1 {
            tracing::warn!(
                requested = %name,
                "primary swap ignored: an interactive subagent holds the foreground"
            );
            return;
        }
        if self.stack[0].agent.name == name {
            return;
        }
        match crate::engine::builtin::load(name, &self.spawn_args(true)) {
            Ok(agent) => {
                self.stack[0].agent = Arc::new(agent);
                // The job authority's fork context is rooted on the old
                // agent; rebind it so any future loop fork runs on the new
                // primary's model/tool surface (single-authority rule).
                self.jobs.set_agent(self.stack[0].agent.clone());
                tracing::info!(agent = %name, "primary agent swapped");
                // Tell the client chrome's active-agent slot about the new
                // primary, then refresh the prunable projection.
                let _ = tx
                    .send(TurnEvent::PrimarySwapped {
                        name: name.to_string(),
                    })
                    .await;
                self.emit_context_projection(tx).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, requested = %name, "primary swap failed to load agent");
            }
        }
    }

    /// Switch the active `llm_mode` live (`/llm-mode`). Rebuilds the
    /// root-frame agent under the new mode so its tool-description verbosity
    /// and per-mode prompt re-render, preserving the root history (same
    /// conversation, new steering). Busts the cached system prefix — the
    /// client warns the user (suppressed on a no-cache provider via the
    /// shared cache-break helper) before sending the switch. Only the root
    /// frame at idle is touched; a deeper interactive subagent frame is left
    /// alone. No-op when the mode is unchanged or a subagent holds the
    /// foreground.
    async fn set_llm_mode(
        &mut self,
        requested: Option<crate::config::extended::LlmMode>,
        tx: &mpsc::Sender<TurnEvent>,
    ) {
        // Resolve the target: an explicit mode, or a toggle against the
        // authoritative current value (the `/llm-mode` default action).
        let current = self.stack[0].agent.llm_mode;
        let mode = requested.unwrap_or_else(|| current.toggled());
        if self.stack.len() != 1 {
            tracing::warn!(
                requested = %mode.as_str(),
                "llm_mode switch ignored: an interactive subagent holds the foreground"
            );
            return;
        }
        if current == mode {
            return;
        }
        let name = self.stack[0].agent.name.clone();
        // Spawn args carry the *new* mode (read from the root agent inside
        // `spawn_args`), so set it on the root first, then reload.
        let mut args = self.spawn_args(true);
        args.llm_mode = mode;
        match crate::engine::builtin::load(&name, &args) {
            Ok(agent) => {
                self.stack[0].agent = Arc::new(agent);
                // Rebind the job authority's fork context to the rebuilt
                // primary (single-authority rule), same as `swap_primary`.
                self.jobs.set_agent(self.stack[0].agent.clone());
                tracing::info!(mode = %mode.as_str(), "llm_mode switched");
                let _ = tx.send(TurnEvent::LlmModeChanged { mode }).await;
                self.emit_context_projection(tx).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, requested = %mode.as_str(), "llm_mode switch failed to reload agent");
            }
        }
    }

    /// Snapshot-dedup the foreground agent's history. `auto` distinguishes
    /// the cache-aware auto-fire from a manual `/prune`. Emits `Pruned` +
    /// a refreshed `ContextProjection`.
    async fn do_prune(&mut self, auto: bool, tx: &mpsc::Sender<TurnEvent>) {
        let depth = self.stack.len();
        let agent_name = self.active_agent().to_string();
        let top = self.stack.last_mut().expect("stack never empty");
        // Snapshot wire-token total + message count before the prune so
        // the timeline event (Part C) can record the before/after delta.
        let messages_before = top.history.len();
        let tokens_before = wire_token_total(&top.history);
        // This prune's targets (the bodies elided *this* call) — the
        // `original_event_id`s describing what was removed.
        let this_prune = prune::dedup_plan(&top.history);
        let this_elided: Vec<String> = this_prune
            .targets
            .iter()
            .map(|t| t.elision.original_event_id.clone())
            .collect();
        let reason = this_prune
            .targets
            .first()
            .map(|t| t.elision.reason.to_string())
            .unwrap_or_else(|| "snapshot superseded".to_string());

        let applied = prune::prune_history(&mut top.history);
        let bodies = applied.targets.len();
        let tokens_saved = applied.tokens_saved() as u64;
        let messages_after = top.history.len();
        let tokens_after = wire_token_total(&top.history);
        // The full live elided set (cumulative across prunes), so the TUI
        // dims every currently-elided body — not just this prune's targets.
        let elided = prune::current_elided_ids(&top.history);
        // Update the watermark so auto-prune short-circuits until the
        // foreground history grows again.
        self.prune_watermark.insert(depth, top.history.len());

        // Timeline event (Part C): record the prune so the export can
        // audit it. Only when something was actually elided — an empty
        // prune is not a meaningful timeline entry. Ordered immediately
        // before the next `inference_request` event by construction
        // (auto-prune fires right before a `turn`).
        if bodies > 0
            && let Err(e) = self.session.record_context_pruned(
                &agent_name,
                auto,
                messages_before,
                messages_after,
                tokens_before,
                tokens_after,
                &this_elided,
                &reason,
            )
        {
            tracing::warn!(error = %e, "record context_pruned event failed");
        }

        let _ = tx
            .send(TurnEvent::Pruned {
                auto,
                bodies,
                tokens_saved,
                elided,
            })
            .await;
        self.emit_context_projection(tx).await;
    }

    /// Cache-aware auto-prune (GOALS §10): before an inference call, if
    /// the cache-cold predicate holds, the foreground history has grown
    /// since the last prune, and there is something prunable, fire
    /// `/prune` with no user prompt. Returns `true` if a prune happened.
    async fn maybe_auto_prune(&mut self, tx: &mpsc::Sender<TurnEvent>) -> bool {
        if !self.at_safe_boundary() {
            return false;
        }
        let depth = self.stack.len();
        let history_len = self.stack.last().expect("stack never empty").history.len();
        // Short-circuit: nothing new since the last prune at this depth.
        if self.prune_watermark.get(&depth).copied() == Some(history_len) {
            return false;
        }
        // Cache-cold? Resolve the active provider/model cache config and
        // evaluate the predicate. `upstream_bust = false` here: v1 has no
        // mid-prefix tool-result edit path that busts the anchor before a
        // send, so cases (a) and (b) carry the predicate.
        let cache = self.resolve_cache_config();
        let secs = self.session.seconds_since_last_send();
        let state = prune::cache_state(&cache, secs, false);
        if !state.is_cold() {
            return false;
        }
        // Is anything actually prunable? Avoid an empty Pruned event.
        let plan = {
            let top = self.stack.last().expect("stack never empty");
            prune::dedup_plan(&top.history)
        };
        if plan.is_empty() {
            // Advance the watermark so we don't re-walk until growth.
            self.prune_watermark.insert(depth, history_len);
            return false;
        }
        self.do_prune(true, tx).await;
        true
    }

    /// Resolve the cache config for the session's active (provider,
    /// model) from the layered providers config. Defaults to `none`
    /// (cold) when the config can't be loaded — the conservative choice
    /// is "pruning is free," matching local/no-cache providers.
    fn resolve_cache_config(&self) -> crate::config::providers::CacheConfig {
        let Some((providers, provider, model)) = self.active_providers_config() else {
            return crate::config::providers::CacheConfig::default();
        };
        providers.resolve_cache(&provider, &model)
    }

    /// Resolve the delegation-shrink config for the session's active
    /// (provider, model). Defaults to (`prune`, 30s margin) when the
    /// config can't be loaded — the lossless, lowest-quality-loss
    /// strategy (priority #1).
    fn resolve_shrink_config(&self) -> crate::config::providers::ShrinkConfig {
        let Some((providers, provider, model)) = self.active_providers_config() else {
            return crate::config::providers::ShrinkConfig::default();
        };
        providers.resolve_shrink(&provider, &model)
    }

    /// Load the layered providers config plus the session's active
    /// (provider, model). `None` when no model is selected or the config
    /// can't be loaded — callers fall back to conservative defaults. Same
    /// first-hit rule as `daemon::server::load_configs`.
    fn active_providers_config(
        &self,
    ) -> Option<(crate::config::providers::ProvidersConfig, String, String)> {
        use crate::config::dirs::discover_config_dirs;
        use crate::config::providers::ConfigDoc;
        let provider = self.session.active_provider()?;
        let model = self.session.active_model()?;
        let providers = discover_config_dirs(&self.cwd)
            .first()
            .map(|d| d.path.join("config.json"))
            .filter(|p| p.exists())
            .and_then(|p| ConfigDoc::load(&p).ok())
            .map(|doc| doc.providers())?;
        Some((providers, provider, model))
    }

    /// Compute and emit the live "% prunable" projection for the
    /// foreground agent (GOALS §1a). The same `dedup_plan` `/prune`
    /// executes drives the figure, so display == execution.
    async fn emit_context_projection(&self, tx: &mpsc::Sender<TurnEvent>) {
        let top = self.stack.last().expect("stack never empty");
        let plan = prune::dedup_plan(&top.history);
        let cache = self.resolve_cache_config();
        let cache_cold =
            prune::cache_state(&cache, self.session.seconds_since_last_send(), false).is_cold();
        let _ = tx
            .send(TurnEvent::ContextProjection {
                prunable_tokens: plan.tokens_saved() as u64,
                cache_cold,
            })
            .await;
    }

    /// Assemble a `/compact` handoff for the foreground agent (T6.e).
    /// Prune-first (fixed ordering), draft the model brief, append the
    /// deterministic appendix, derive seed-tools, create a fresh session
    /// row, and emit `CompactReady`. The old session is left whole.
    async fn do_compact(&mut self, tx: &mpsc::Sender<TurnEvent>) {
        use crate::engine::compact;

        // 0. Prune-first (lossless; denser transcript → tighter brief).
        self.do_prune(false, tx).await;

        // 1. Model brief from the foreground agent's current history.
        let brief = self.draft_brief().await;

        // 2. Deterministic appendix from the runtime ledger.
        let calls = self
            .session
            .db
            .list_tool_calls_for_session(self.session.id)
            .unwrap_or_default();
        let pins = self.session.pinned_messages();
        let appendix = compact::build_appendix(&calls, &self.cwd, &pins, &[]);

        // 3. Seed-tools (read-only/idempotent; re-executed, not replayed).
        let seeds = compact::derive_seed_tools(&calls);
        let seed_tool_tokens: u64 = seeds
            .iter()
            .map(|s| crate::tokens::count(&s.args.to_string()) as u64)
            .sum();

        // 4. Assemble the review-ready handoff.
        let handoff = compact::assemble_handoff(&brief, &appendix);

        // 5. Create the fresh session row (the worker spawns when the TUI
        // re-attaches). Seed-tools are persisted on the new session so its
        // worker re-executes them before the first inference call.
        let new_session = match crate::session::Session::create(
            self.session.db.clone(),
            self.cwd.clone(),
            self.stack
                .last()
                .expect("stack never empty")
                .agent
                .name
                .as_str(),
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "compact: creating new session failed");
                return;
            }
        };
        // Carry the active model onto the fresh session.
        if let (Some(p), Some(m)) = (self.session.active_provider(), self.session.active_model()) {
            let _ = new_session.set_active_model(&p, &m);
        }
        // Persist the seed-tool plan so the new session's worker can
        // re-execute it on its first turn.
        if let Err(e) = self.session.db.set_seed_tools(new_session.id, &seeds) {
            tracing::warn!(error = %e, "compact: persisting seed tools failed");
        }

        // Timeline boundary (Part C): `/compact` started a fresh successor
        // session. The export follows this link like the fork tree so both
        // sessions land in one unified `events.json`. Modeled as a session
        // boundary, NOT a `context_pruned` event.
        if let Err(e) = self.session.record_session_compacted(
            self.active_agent(),
            new_session.id,
            &new_session.short_id,
            seeds.len(),
        ) {
            tracing::warn!(error = %e, "record session_compacted event failed");
        }

        let _ = tx
            .send(TurnEvent::CompactReady {
                new_session_id: new_session.id,
                handoff,
                seed_tool_count: seeds.len(),
                seed_tool_tokens,
            })
            .await;
    }

    /// Run one model round-trip asking the foreground agent to draft the
    /// self-contained handoff brief (T6.e step 1). Falls back to a terse
    /// placeholder if the model call fails so `/compact` always produces
    /// a usable handoff (the deterministic appendix is the real safety
    /// net).
    async fn draft_brief(&self) -> String {
        let top = self.stack.last().expect("stack never empty");
        let prompt = Message::user(crate::engine::compact::brief_prompt());
        // Always-on capture (Part A): the `/compact` brief is an inference
        // call too, so persist its request body + a timeline event keyed by
        // a fresh round-trip id.
        let call_id = uuid::Uuid::new_v4();
        match top
            .agent
            .model
            .complete_captured(
                &top.agent.system,
                &top.history,
                prompt,
                &[],
                top.agent.params.clone(),
                &top.agent.name,
                &self.silent_event_tx(),
                // The `/compact` brief is a short utility round-trip, not a
                // user-message turn; it isn't tied to the run's ctrl+c
                // cancel slot. A fresh never-cancelled token keeps the
                // signature uniform.
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
        {
            Ok(((_, choice, usage), captured)) => {
                if let Err(e) = self.session.record_inference_request(call_id, &captured) {
                    tracing::warn!(error = %e, "compact brief: record_inference_request failed");
                }
                let usage_json = usage.map(|u| {
                    serde_json::json!({
                        "input_tokens": u.input_tokens,
                        "output_tokens": u.output_tokens,
                        "cached_input_tokens": u.cached_input_tokens,
                    })
                });
                if let Err(e) = self.session.record_event(
                    crate::db::session_log::SessionEventKind::InferenceRequest,
                    Some(&top.agent.name),
                    Some(&call_id.to_string()),
                    &serde_json::json!({ "usage": usage_json, "purpose": "compact_brief" }),
                ) {
                    tracing::warn!(error = %e, "compact brief: record inference_request event failed");
                }
                let text = crate::engine::message::extract_text(&choice);
                if text.trim().is_empty() {
                    "(model produced no brief; rely on the state appendix below)".to_string()
                } else {
                    text
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "compact: brief generation failed");
                "(brief generation failed; rely on the state appendix below)".to_string()
            }
        }
    }

    /// A throwaway event sender for the brief round-trip (its streaming
    /// deltas are not shown — only the final brief text matters).
    fn silent_event_tx(&self) -> mpsc::Sender<TurnEvent> {
        let (tx, mut rx) = mpsc::channel::<TurnEvent>(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        tx
    }

    /// Re-execute a `/compact` seed-tool plan into the foreground agent's
    /// initial context, *before* the first inference (T6.e). Each seed is
    /// a read-only / idempotent tool call (`read`, the read-only intel
    /// tools); we dispatch it fresh and fold the results into one
    /// synthetic user message prepended to history — so the fresh agent
    /// starts with the live working set without a round-trip, and never
    /// sees a stale snapshot. Tools the agent doesn't have, or that fail,
    /// are skipped (the brief/appendix still carry the context). A
    /// `ToolStart`/`ToolEnd` pair is emitted per seed so the cost is
    /// visible on the new agent's first turn.
    pub async fn run_seed_tools(
        &mut self,
        seeds: &[crate::engine::compact::SeedTool],
        tx: &mpsc::Sender<TurnEvent>,
    ) {
        let agent = self.stack.last().expect("stack never empty").agent.clone();
        let ctx = crate::engine::tool::ToolCtx {
            agent_id: agent.name.clone(),
            locks: self.locks.clone(),
            session: self.session.clone(),
            cwd: self.cwd.clone(),
            redact: self.redact.clone(),
            interrupts: self.interrupts.clone(),
            // Seed-tool re-execution runs before the first user turn; it
            // has no run-scoped cancel slot, so a fresh never-cancelled
            // token suffices.
            cancel: tokio_util::sync::CancellationToken::new(),
            // Seeds are read-only/idempotent and run before the approver
            // is consulted in earnest; a missing approver skips the
            // boundary prompt (never denies).
            approver: self.approver.clone(),
            // Seed re-exec runs read-only tools only; nothing defers.
            deferred_log: crate::engine::deferred::DeferredLog::new(),
        };
        let mut blocks: Vec<String> = Vec::new();
        for seed in seeds {
            // Restrict defensively to read-only/idempotent tools and to
            // tools this agent actually has — never dispatch a write path.
            let Some(tool) = agent.tools.get(&seed.tool) else {
                continue;
            };
            let call_id = format!("seed-{}", uuid::Uuid::new_v4());
            let _ = tx
                .send(TurnEvent::ToolStart {
                    agent: agent.name.clone(),
                    call_id: call_id.clone(),
                    tool: seed.tool.clone(),
                    args: seed.args.clone(),
                })
                .await;
            let result = tool.call(seed.args.clone(), &ctx).await;
            let body = match result {
                Ok(out) => self.redact.scrub(&out.content),
                Err(e) => format!("Error: {e}"),
            };
            let _ = tx
                .send(TurnEvent::ToolEnd {
                    agent: agent.name.clone(),
                    call_id,
                    tool: seed.tool.clone(),
                    output: body.clone(),
                    truncated: false,
                })
                .await;
            let label = crate::tui::agent_runner::short_args(&seed.args);
            blocks.push(format!(
                "<seed tool=\"{}\" {}>\n{}\n</seed>",
                seed.tool, label, body
            ));
        }
        if !blocks.is_empty() {
            let combined = format!(
                "[compaction handoff — re-executed working-set context; the live results follow]\n\n{}",
                blocks.join("\n\n")
            );
            // Prepend to the first user message rather than pushing a bare
            // user turn (which would put two user messages back-to-back).
            self.pending_seed_context = Some(combined);
        }
    }

    /// Begin compact-after-delegation tracking for the paused parent frame
    /// (`prompts/compact-after-delegation.md`). `parent_full` is a clone of
    /// the parent's full history at delegation start. Resolves the cache +
    /// shrink config, decides eager-vs-lazy timing, and — for the
    /// no-cache (eager) case — spawns the shrink task immediately so its
    /// latency hides under the (synchronous or interactive) child run. For
    /// the cache-capable (lazy) case the task sleeps until `ttl - margin`
    /// and only then shrinks: a child that returns first means the task is
    /// still sleeping and produces nothing (no wasted shrink).
    ///
    /// Returns the decision tracker plus the background task handle (if a
    /// task was spawned). The tracker measures elapsed-since-delegation
    /// from its own captured instant, NEVER the session-global send timer
    /// the child resets every turn (the staleness trap).
    fn begin_delegation_shrink(
        &self,
        parent_full: Vec<Message>,
    ) -> (
        crate::engine::deleg_shrink::DelegationShrink,
        Option<tokio::task::JoinHandle<Vec<Message>>>,
    ) {
        use crate::engine::deleg_shrink::{DelegationShrink, ShrinkTiming};

        let cache = self.resolve_cache_config();
        let shrink_cfg = self.resolve_shrink_config();
        let tracker = DelegationShrink::new(cache.clone(), &shrink_cfg);
        let timing = crate::engine::deleg_shrink::decide_timing(&cache, &shrink_cfg);

        // The shrink runs on a clone of the parent history; the parent
        // frame's own history is never touched until we resolve.
        let agent = self.stack.last().expect("stack never empty").agent.clone();
        let strategy = tracker.strategy();
        // Reuse the run-scoped cancel so a user ctrl+c aborts a `compact`
        // shrink's model call too — never a parallel cancel.
        let cancel = self
            .cancel_current
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default();

        let delay = match timing {
            ShrinkTiming::Eager => std::time::Duration::ZERO,
            ShrinkTiming::LazyAt(d) => d,
        };

        let handle = tokio::spawn(async move {
            // Lazy: wait until `ttl - margin`. If the child returns first,
            // the parent aborts this task before the sleep elapses, so no
            // shrink runs. Eager: ZERO delay → runs immediately.
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            run_shrink(strategy, &parent_full, agent, cancel).await
        });

        (tracker, Some(handle))
    }

    /// Resolve a finished delegation: collect any shrunk history the
    /// parallel task produced, decide full-vs-shrunk via the cache-cold
    /// predicate (elapsed-since-delegation), and — when cold — replace the
    /// **top** (now-resumed parent) frame's history with the shrunk copy.
    /// A hot return keeps the full context (the lazy task is aborted before
    /// it ever shrinks). Idempotent: a missing/None handle is a no-op.
    async fn finish_delegation_shrink(
        &mut self,
        mut tracker: crate::engine::deleg_shrink::DelegationShrink,
        handle: Option<tokio::task::JoinHandle<Vec<Message>>>,
        tx: &mpsc::Sender<TurnEvent>,
    ) {
        if let Some(handle) = handle {
            if handle.is_finished() {
                // The (eager, or lazy-and-already-fired) shrink completed
                // while the child ran — adopt its result.
                if let Ok(shrunk) = handle.await {
                    tracker.set_shrunk(shrunk);
                }
            } else {
                // The child returned before the lazy trigger fired: abort
                // the still-sleeping task so no shrink ever runs (the
                // fast-delegation case wastes nothing).
                handle.abort();
            }
        }
        // `resolve` reuses the single cache-cold predicate
        // (`prune::cache_state`) over elapsed-since-delegation: cold with a
        // computed shrink ⇒ `Some(shrunk)`; hot, or cold-without-shrink ⇒
        // `None` (keep the full context).
        match tracker.resolve() {
            Some(shrunk) => {
                let before = self.stack.last().expect("stack never empty").history.len();
                let after = shrunk.len();
                self.stack.last_mut().expect("stack never empty").history = shrunk;
                tracing::info!(
                    before,
                    after,
                    "delegation-shrink: parent cache cold, resumed on shrunk context"
                );
                // Refresh the prunable projection from the now-shrunk
                // foreground history.
                self.emit_context_projection(tx).await;
            }
            None => {
                tracing::debug!("delegation-shrink: parent resuming on full context");
            }
        }
    }

    /// Run a job event as a late-arriving turn in **main** context. A
    /// loop-iteration-due event runs the loop's prompt as a real turn (and
    /// reports back so the authority schedules the next tick); a terminal
    /// completion injects the budget-capped result, then surfaces any
    /// fork-emitted spawn requests for the model to decide on.
    async fn run_job_event(
        &mut self,
        event: JobEvent,
        input_rx: &mut mpsc::Receiver<UserSubmission>,
        tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        match event {
            JobEvent::LoopIterationDue { job_id, prompt } => {
                let framed = format!("[loop {job_id}] {prompt}");
                self.run_user_input(
                    UserSubmission::text(self.redact.scrub(&framed)),
                    input_rx,
                    tx,
                )
                .await?;
                // The iteration's turn finished — advance the schedule.
                self.jobs.iteration_finished(&job_id);
            }
            JobEvent::Completed {
                job_id,
                label,
                kind,
                result,
                failed,
                requests,
            } => {
                // UI marker for the strip / transcript.
                let _ = tx
                    .send(TurnEvent::JobCompleted {
                        job_id: job_id.clone(),
                        label: label.clone(),
                        kind: kind.as_str().to_string(),
                        failed,
                    })
                    .await;
                // Flag the needs-attention queue on every job end (GOALS
                // §22) so a detached client still sees it on reconnect.
                let note = if failed {
                    format!("async {} `{}` failed", kind.as_str(), label)
                } else {
                    format!("async {} `{}` completed", kind.as_str(), label)
                };
                if let Err(e) =
                    self.session
                        .db
                        .raise_interrupt(self.session.id, "jobs", &note, None)
                {
                    tracing::warn!(error = %e, "raising needs_attention on job end failed");
                }
                // Inject the budget-capped result as a late-arriving turn.
                let mut injected = format!("[async result · {}]\n{result}", kind.as_str());
                // Surface any fork-emitted spawn requests (anti-runaway:
                // forks request, main decides). The model sees them and
                // can re-issue a `jobs` call to honour them.
                if !requests.is_empty() {
                    injected
                        .push_str("\n\nThis loop requested new jobs (not started — you decide):");
                    for req in &requests {
                        injected.push_str(&format!("\n- {}", req.summary()));
                    }
                }
                self.run_user_input(
                    UserSubmission::text(self.redact.scrub(&injected)),
                    input_rx,
                    tx,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Dispatch a `jobs` meta-tool action against the authority and return
    /// the tool-result string the model sees. The single async-job
    /// authority lives here on the driver (GOALS §22), which is why the
    /// engine routes `jobs` calls back via [`TurnOutcome::JobAction`]
    /// rather than dispatching them inline.
    fn dispatch_job_action(&mut self, args: &serde_json::Value) -> Result<String> {
        use crate::engine::jobs::{JobAction, JobKind};
        use crate::tools::jobs::split_action;

        let (action, action_args) = split_action(args)?;
        match action {
            JobAction::LoopStart => {
                if self.jobs.at_capacity() {
                    anyhow::bail!(
                        "max concurrent jobs reached ({}); cancel one before starting another",
                        self.jobs.max_concurrent
                    );
                }
                let parsed = crate::engine::jobs::parse_loop_start(&action_args)?;
                let kind = parsed.kind();
                let job_id = if parsed.keep_in_context {
                    self.jobs.start_loop_in_context(parsed)
                } else {
                    self.jobs.start_loop_forked(parsed)
                };
                let noun = if kind == JobKind::Timer {
                    "timer"
                } else {
                    "loop"
                };
                Ok(format!(
                    "started {noun} `{job_id}` — cancel with jobs(action=\"loop.cancel\", args={{\"job_id\":\"{job_id}\"}})"
                ))
            }
            JobAction::LoopCancel => {
                let parsed = crate::engine::jobs::parse_loop_cancel(&action_args)?;
                if self.jobs.cancel(&parsed.job_id) {
                    Ok(format!("cancelled `{}`", parsed.job_id))
                } else {
                    Ok(format!("no live job `{}`", parsed.job_id))
                }
            }
            JobAction::BackgroundStart => {
                if self.jobs.at_capacity() {
                    anyhow::bail!(
                        "max concurrent jobs reached ({}); cancel one before starting another",
                        self.jobs.max_concurrent
                    );
                }
                let parsed = crate::engine::jobs::parse_background_start(&action_args)?;
                let job_id = self.jobs.start_background(parsed);
                Ok(format!(
                    "started background `{job_id}` — tail with jobs(action=\"background.tail\", args={{\"job_id\":\"{job_id}\"}})"
                ))
            }
            JobAction::BackgroundTail => {
                let parsed = crate::engine::jobs::parse_background_tail(&action_args)?;
                match self.jobs.background_handle(&parsed.job_id) {
                    Some(handle) => Ok(handle.tail(parsed.lines, &self.redact)),
                    None => Ok(format!("no live background `{}`", parsed.job_id)),
                }
            }
            JobAction::BackgroundCancel => {
                let parsed = crate::engine::jobs::parse_background_cancel(&action_args)?;
                if self.jobs.cancel(&parsed.job_id) {
                    Ok(format!("cancelled background `{}`", parsed.job_id))
                } else {
                    Ok(format!("no live background `{}`", parsed.job_id))
                }
            }
            JobAction::List => {
                let snap = self.jobs.snapshot();
                if snap.is_empty() {
                    return Ok("no active jobs".to_string());
                }
                let mut out = String::new();
                for j in snap {
                    let progress = match j.limit {
                        Some(limit) => format!("{}/{}", j.iteration, limit),
                        None => format!("{} (unlimited)", j.iteration),
                    };
                    out.push_str(&format!(
                        "{} {} [{}] {}\n",
                        j.job_id,
                        j.kind.as_str(),
                        progress,
                        j.label
                    ));
                }
                Ok(out)
            }
        }
    }

    /// Drive one user message through the stack. Between inference
    /// rounds we drain any queued messages and fold them — see
    /// [`Self::run_main_loop`] for the contract.
    pub async fn run_user_input(
        &mut self,
        submission: UserSubmission,
        input_rx: &mut mpsc::Receiver<UserSubmission>,
        tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        // Pasted image parts (vision models only) ride alongside the text
        // through every text-only step below (titling, skills, seed,
        // time prelude) and are reattached when the prompt `Message` is
        // built. Non-vision callers already folded images into `text` and
        // pass none here (composer-paste-handling).
        let images = submission.images;
        let user_text = submission.text;
        // Install a fresh cancellation token for this run so a user ctrl+c
        // (`SessionWork::Cancel` → `CancelHandle::cancel`) can abort the
        // in-flight inference and kill any running `bash` subprocess. The
        // guard clears the slot on every exit path (normal, cancel, error)
        // so a stale token can never affect a later run.
        let cancel = tokio_util::sync::CancellationToken::new();
        let _cancel_guard = {
            *self.cancel_current.lock().unwrap() = Some(cancel.clone());
            CancelSlotGuard {
                slot: self.cancel_current.clone(),
            }
        };
        // Timeline event (session-log-export Part B): the unit of user /
        // injected input that drives this run. Tagged with the foreground
        // agent. Recorded before prelude/seed wrapping so the export shows
        // the user's actual text.
        if let Err(e) = self.session.record_event(
            crate::db::session_log::SessionEventKind::UserMessage,
            Some(self.active_agent()),
            None,
            &serde_json::json!({ "text": user_text }),
        ) {
            tracing::warn!(error = %e, "record user_message event failed");
        }

        // Auto-title hook (GOALS §17d). `note_user_content` returns
        // true only when this call's tokens cross the threshold for
        // the first time *and* the session is eligible (no title,
        // not user-renamed). Spawn the inference in a detached task
        // so the driver loop isn't blocked on a network round-trip;
        // failures inside the task silently drop the title.
        if self.session.note_user_content(&user_text) {
            let session = self.session.clone();
            let cwd = self.cwd.clone();
            let content_prefix = user_text.clone();
            tokio::spawn(async move {
                let (extended, providers) = crate::auto_title::load_configs_for(&cwd);
                crate::auto_title::generate_session_title(
                    session,
                    extended,
                    providers,
                    content_prefix,
                )
                .await;
            });
        }

        // Prepend any pending `/compact` seed-tool context to the first
        // user message so the fresh agent's first inference carries the
        // re-executed working set (T6.e). One-shot.
        let user_text = match self.pending_seed_context.take() {
            Some(seed) => format!("{seed}\n\n{user_text}"),
            None => user_text,
        };

        // Skills auto-selection (GOALS §5): consult the cheap utility
        // model with the skill catalog + this message; if it picks one,
        // prepend the (`!`-processed, scrubbed) body so the main agent's
        // first inference carries it. Skipped gracefully (logged once)
        // when no utility model is configured — never falls back to the
        // main model.
        let user_text = self.maybe_inject_skill(&user_text).await;

        let mut next_prompt = crate::engine::message::build_user_message(UserSubmission {
            text: self.with_time_prelude(user_text),
            images,
        });

        loop {
            // Cache-aware auto-prune (GOALS §10): before talking to the
            // model, if the cache is cold and the foreground history has
            // grown something prunable, collapse it for free.
            self.maybe_auto_prune(tx).await;

            let agent = {
                let top = self.stack.last().expect("stack never empty");
                top.agent.clone()
            };

            // The session-root conversation is the only one with a frozen
            // system block reused across requests — it's where the live
            // instructions-file diff (`instructions-file-live-diff.md`)
            // injects. Subagents (stack depth > 1) recompose a fresh system
            // prompt on spawn, so they skip it.
            let is_root = self.stack.len() == 1;
            let turn_result = {
                let top = self.stack.last_mut().expect("stack never empty");
                // The foreground frame's deferred-log buffer (`plan.md §3d`):
                // a subagent's `defer_to_orchestrator` calls land here, and
                // the driver folds them into the report when the frame pops.
                let deferred_log = top.deferred_log.clone();
                turn(
                    &agent,
                    &mut top.history,
                    next_prompt,
                    self.session.clone(),
                    self.locks.clone(),
                    self.redact.clone(),
                    self.cwd.clone(),
                    self.interrupts.clone(),
                    cancel.clone(),
                    self.approver.clone(),
                    self.loop_guard_threshold,
                    is_root,
                    deferred_log,
                    tx,
                )
                .await
            };
            // A user ctrl+c (`CancelTurn`) aborts the in-flight inference
            // via `cancel`; `turn` surfaces it as an `InferenceCancelled`
            // sentinel. Unwind cleanly back to idle rather than treating it
            // as a real error: the agent stack stays consistent (the
            // assistant turn was never pushed), the worker's main loop
            // proceeds to emit `AgentIdle`, and the composer becomes usable
            // again. Any queued messages stay in `input_rx` for the next
            // run. Real errors still propagate.
            let outcome = match turn_result {
                Ok(outcome) => outcome,
                Err(e) if crate::engine::model::is_cancelled(&e) => {
                    tracing::info!(agent = %agent.name, "turn cancelled by user");
                    return Ok(());
                }
                // The daemon began draining (`daemon-graceful-drain-shutdown.md`):
                // the inference-dispatch chokepoint refused this *new* round-
                // trip. Unwind cleanly back to idle exactly like a cancel —
                // the worker proceeds to its `Shutdown`/drain teardown rather
                // than logging a real error.
                Err(e) if crate::engine::model::is_gated(&e) => {
                    tracing::info!(agent = %agent.name, "turn refused: daemon draining");
                    return Ok(());
                }
                Err(e) => return Err(e),
            };

            match outcome {
                TurnOutcome::Continue => {
                    let top = self.stack.last_mut().expect("stack never empty");
                    let last_tool_result = top
                        .history
                        .pop()
                        .expect("Continue with empty history is unreachable");

                    // Fold any queued user messages onto the upcoming
                    // inference. The tool result still has to be
                    // delivered, so push it back onto history and use
                    // the queued user content as the next prompt.
                    let mut queued: Vec<UserSubmission> = Vec::new();
                    drain_queue(input_rx, &mut queued);
                    if queued.is_empty() {
                        next_prompt = last_tool_result;
                    } else {
                        top.history.push(last_tool_result);
                        let folded = fold_submissions(queued);
                        next_prompt = crate::engine::message::build_user_message(UserSubmission {
                            text: self.with_time_prelude(self.redact.scrub(&folded.text)),
                            images: folded.images,
                        });
                    }
                    continue;
                }
                TurnOutcome::Done => {
                    if self.stack.len() > 1 {
                        let child = self.stack.pop().unwrap();
                        // Drop any locks the child still held — the
                        // §3c invariant doesn't extend across the
                        // child's lifetime, and lingering locks would
                        // block whatever takes its slot next.
                        if let Err(e) = self.locks.suspend_agent(&child.agent.name, self.session.id)
                        {
                            tracing::warn!(error = ?e, agent = %child.agent.name, "suspend_agent on pop failed");
                        }
                        // The agent now back on top regains its lock
                        // set for files whose hash matches the snapshot
                        // taken when it was suspended (see SpawnSubagent
                        // below).
                        if let Some(parent) = self.stack.last() {
                            if let Err(e) =
                                self.locks.resume_agent(&parent.agent.name, self.session.id)
                            {
                                tracing::warn!(error = ?e, agent = %parent.agent.name, "resume_agent on pop failed");
                            }
                        }
                        // Resolve compact-after-delegation for the
                        // now-resumed parent frame (the new top). Cold ⇒
                        // swap in the shrunk parent context; hot ⇒ keep
                        // full and abort the still-sleeping lazy shrink.
                        let parent_depth = self.stack.len() - 1;
                        if let Some(pending) = self.deleg_shrinks.remove(&parent_depth) {
                            let PendingDelegationShrink { tracker, handle } = pending;
                            self.finish_delegation_shrink(tracker, handle, tx).await;
                        }
                        // Fold the child frame's deferred-log into the report
                        // (`plan.md §3d`): the parent ingests `{ report,
                        // deferred_log }` as one tool result and addresses each
                        // deferred item. Drained once on pop; nothing-deferred
                        // is the common path and adds no framing.
                        let report = if child.deferred_log.is_empty() {
                            collect_final_text(&child.history)
                        } else {
                            let deferred = child.deferred_log.drain();
                            format!(
                                "{}{}",
                                collect_final_text(&child.history),
                                crate::engine::deferred::format_section(&deferred)
                            )
                        };
                        if let Err(e) = self.session.record_event(
                            crate::db::session_log::SessionEventKind::SubagentReport,
                            Some(&child.agent.name),
                            child.answering.as_ref().map(|p| p.call_id.as_str()),
                            &serde_json::json!({ "report": report }),
                        ) {
                            tracing::warn!(error = %e, "record subagent_report event failed");
                        }
                        let _ = tx
                            .send(TurnEvent::SubagentReport {
                                agent: child.agent.name.clone(),
                                report: report.clone(),
                            })
                            .await;
                        if let Some(pending) = child.answering {
                            // The task call's tool_result becomes the
                            // parent's next prompt. The parent's
                            // history already ends with the assistant
                            // turn that emitted the task call.
                            next_prompt = Message::tool_result_with_call_id(
                                pending.call_id,
                                pending.function_call_id,
                                report,
                            );
                            continue;
                        }
                    }
                    // Root agent is done with this user message. Before
                    // we wait for the next user input, check if more
                    // landed in the queue while we were busy — fold
                    // them and start a new run with the combined text.
                    let mut queued: Vec<UserSubmission> = Vec::new();
                    drain_queue(input_rx, &mut queued);
                    if !queued.is_empty() {
                        let folded = fold_submissions(queued);
                        next_prompt = crate::engine::message::build_user_message(UserSubmission {
                            text: self.redact.scrub(&folded.text),
                            images: folded.images,
                        });
                        continue;
                    }
                    return Ok(());
                }
                TurnOutcome::SpawnSubagent {
                    child_agent,
                    prompt: brief,
                    task_call_id,
                    task_function_call_id,
                } => {
                    // Snapshot the outgoing primary's locks before the
                    // child takes over. If the parent ever resumes (the
                    // child pops via TurnOutcome::Done above), the
                    // matching-hash files can come back without a re-
                    // readlock round-trip.
                    if let Some(parent) = self.stack.last() {
                        if let Err(e) = self
                            .locks
                            .suspend_agent(&parent.agent.name, self.session.id)
                        {
                            tracing::warn!(error = ?e, agent = %parent.agent.name, "suspend_agent on push failed");
                        }
                    }
                    // Begin compact-after-delegation tracking for the
                    // parent frame about to be paused below the interactive
                    // child (`prompts/compact-after-delegation.md`). Keyed
                    // by the parent's depth (its index, = pre-push height
                    // minus one). Captured here so elapsed-since-delegation
                    // measures from the parent's last inference — the turn
                    // that emitted this `task` call — not the session-global
                    // send timer the child resets every turn (the trap).
                    let parent_depth = self.stack.len() - 1;
                    let parent_full = self
                        .stack
                        .last()
                        .expect("stack never empty")
                        .history
                        .clone();
                    let (tracker, handle) = self.begin_delegation_shrink(parent_full);
                    self.deleg_shrinks
                        .insert(parent_depth, PendingDelegationShrink { tracker, handle });
                    let child = crate::engine::builtin::load(&child_agent, &self.spawn_args(true))?;
                    self.stack.push(AgentSession {
                        agent: Arc::new(child),
                        history: Vec::new(),
                        answering: Some(PendingTaskCall {
                            call_id: task_call_id,
                            function_call_id: task_function_call_id,
                        }),
                        deferred_log: crate::engine::deferred::DeferredLog::new(),
                    });
                    next_prompt = Message::user(self.redact.scrub(&brief));
                    continue;
                }
                TurnOutcome::SpawnNoninteractive {
                    child_agent,
                    prompt: brief,
                    task_call_id,
                    task_function_call_id,
                } => {
                    // Surface the delegation as a live `{parent} delegated
                    // to {child}…` line in the parent's history (the TUI
                    // renders SubagentSpawned/SubagentReport as one rich,
                    // self-replacing block — `prompts/noninteractive-
                    // subagent-display.md`). The brief is intentionally
                    // dropped from the event: the running line shows no
                    // prompt preview.
                    let _ = tx
                        .send(TurnEvent::SubagentSpawned {
                            parent: self.stack.last().unwrap().agent.name.clone(),
                            child: child_agent.clone(),
                            prompt: brief.clone(),
                        })
                        .await;
                    // Begin compact-after-delegation tracking for the
                    // paused parent frame (the current top of the stack).
                    // The shrink runs IN PARALLEL with the synchronous
                    // child below; on return we pick full-vs-shrunk
                    // (`prompts/compact-after-delegation.md`). Captured here
                    // — the parent's last inference was the turn that
                    // emitted this `task` call — so elapsed-since-delegation
                    // is immune to the child's `note_send()` (the trap).
                    let parent_full = self
                        .stack
                        .last()
                        .expect("stack never empty")
                        .history
                        .clone();
                    let (tracker, shrink_handle) = self.begin_delegation_shrink(parent_full);

                    // `docs` is a fixed two-stage pipeline (Docs.1
                    // resolver in caller cwd → Docs.2 answerer in the
                    // resolved package dir). Everything else is a single
                    // noninteractive agent loop.
                    let report = if child_agent == "docs" {
                        match crate::engine::docs_pipeline::run(
                            &brief,
                            &self.spawn_args(false),
                            self.session.clone(),
                            self.locks.clone(),
                            self.redact.clone(),
                            cancel.clone(),
                        )
                        .await
                        {
                            Ok(text) => text,
                            Err(e) => format!("Error: {e:#}"),
                        }
                    } else {
                        let child =
                            crate::engine::builtin::load(&child_agent, &self.spawn_args(false))?;
                        match run_noninteractive(
                            child,
                            self.redact.scrub(&brief),
                            self.session.clone(),
                            self.locks.clone(),
                            self.redact.clone(),
                            self.cwd.clone(),
                            self.interrupts.clone(),
                            cancel.clone(),
                            self.approver.clone(),
                            self.loop_guard_threshold,
                            EXPLORE_MAX_TURNS,
                        )
                        .await
                        {
                            Ok(text) => text,
                            Err(e) => format!("Error: {e:#}"),
                        }
                    };

                    // The child returned. Fold in any shrink that the
                    // parallel task produced, then decide full-vs-shrunk
                    // and apply to the paused parent frame.
                    self.finish_delegation_shrink(tracker, shrink_handle, tx)
                        .await;
                    // Timeline event (Part B): the noninteractive subagent's
                    // report. This path emits ToolStart/End directly (not
                    // through `turn`'s dispatch loop), so the report is
                    // recorded here rather than as a `tool_call` event.
                    if let Err(e) = self.session.record_event(
                        crate::db::session_log::SessionEventKind::SubagentReport,
                        Some(&child_agent),
                        Some(&task_call_id),
                        &serde_json::json!({
                            "child_agent": child_agent,
                            "report": report,
                        }),
                    ) {
                        tracing::warn!(error = %e, "record subagent_report event failed");
                    }
                    let _ = tx
                        .send(TurnEvent::SubagentReport {
                            agent: child_agent.clone(),
                            report: report.clone(),
                        })
                        .await;
                    // Deliver the result as the parent's next prompt.
                    next_prompt = Message::tool_result_with_call_id(
                        task_call_id,
                        task_function_call_id,
                        report,
                    );
                    continue;
                }
                TurnOutcome::JobAction {
                    args,
                    task_call_id,
                    task_function_call_id,
                } => {
                    // The single async-job authority lives on the driver
                    // (GOALS §22). Dispatch the action, emit one
                    // ToolStart/End pair so the user sees a single row,
                    // and deliver the result as this `jobs` call's
                    // tool_result.
                    let agent_name = self.stack.last().unwrap().agent.name.clone();
                    let _ = tx
                        .send(TurnEvent::ToolStart {
                            agent: agent_name.clone(),
                            call_id: task_call_id.clone(),
                            tool: "jobs".to_string(),
                            args: args.clone(),
                        })
                        .await;
                    let (mut output, hard_fail, kind) = match self.dispatch_job_action(&args) {
                        Ok(text) => (self.redact.scrub(&text), false, None),
                        Err(e) => (
                            format!("Error: {e}"),
                            true,
                            Some(crate::engine::tool::classify_failure(&e)),
                        ),
                    };
                    // Cache-safe capability growth (GOALS §22): the first
                    // time a loop or background exists, append a hint to
                    // this tool result announcing the now-available
                    // branches. Appended text extends the prefix; the
                    // byte-stable tools array never changes.
                    if !hard_fail {
                        for hint in self.pending_capability_hints() {
                            output.push('\n');
                            output.push_str(hint);
                        }
                    }
                    if hard_fail {
                        let _ = tx
                            .send(TurnEvent::ToolError {
                                agent: agent_name.clone(),
                                call_id: task_call_id.clone(),
                                tool: "jobs".to_string(),
                                error: output.clone(),
                                kind: kind.unwrap_or(crate::engine::tool::ToolFailKind::Execution),
                            })
                            .await;
                    } else {
                        let _ = tx
                            .send(TurnEvent::ToolEnd {
                                agent: agent_name.clone(),
                                call_id: task_call_id.clone(),
                                tool: "jobs".to_string(),
                                output: output.clone(),
                                truncated: false,
                            })
                            .await;
                    }
                    next_prompt = Message::tool_result_with_call_id(
                        task_call_id,
                        task_function_call_id,
                        output,
                    );
                    continue;
                }
            }
        }
    }

    /// Return any capability-hint strings that should be appended now: the
    /// first time a loop exists, announce `loop.cancel`; the first time a
    /// background exists, announce `background.tail`/`background.cancel`.
    /// Each hint fires at most once per session (tracked in
    /// `appended_hints`).
    fn pending_capability_hints(&mut self) -> Vec<&'static str> {
        let mut hints = Vec::new();
        if self.jobs.has_loop() && self.appended_hints.insert("loop") {
            hints.push(
                "(jobs: loop.cancel is now available — args {\"job_id\": <id>} — to end a live loop)",
            );
        }
        if self.jobs.has_background() && self.appended_hints.insert("background") {
            hints.push(
                "(jobs: background.tail and background.cancel are now available — args {\"job_id\": <id>})",
            );
        }
        hints
    }

    /// Build [`SpawnArgs`] for a child agent. `interactive` distinguishes
    /// a user-facing handoff (an interactive subagent — e.g. `coder`,
    /// which gets the cross-session recall tools) from a one-shot leaf
    /// delegation run via [`run_noninteractive`] (explore / docs, which
    /// do not). This is the spawn-time analog of the runtime
    /// interactive-mode gate.
    fn spawn_args(&self, interactive: bool) -> crate::engine::builtin::SpawnArgs {
        crate::engine::builtin::SpawnArgs {
            model: self.stack[0].agent.model.clone(),
            params: self.stack[0].agent.params.clone(),
            cwd: self.cwd.clone(),
            session_short_id: self.session.short_id.clone(),
            interactive,
            // The active LLM mode rides on the root agent; child spawns
            // inherit it so the whole invocation tree renders one mode.
            llm_mode: self.stack[0].agent.llm_mode,
        }
    }
}

/// Drain queued user submissions from the channel without blocking.
/// Stops at the [`MAX_FOLD`] cap; anything beyond stays for a later fold.
fn drain_queue(rx: &mut mpsc::Receiver<UserSubmission>, into: &mut Vec<UserSubmission>) {
    while into.len() < MAX_FOLD {
        match rx.try_recv() {
            Ok(s) => into.push(s),
            Err(_) => break,
        }
    }
}

/// Fold multiple user submissions into one inference payload per GOALS
/// §1c: blank-line text separator, no special framing or numbering, and
/// all image parts concatenated in order. The user composed them as
/// separate thoughts; the model sees one coherent message. The folded
/// `text` preserves each submission's `IMAGE_PART_SENTINEL` markers in
/// place, so the marker order still lines up with `images`.
fn fold_submissions(submissions: Vec<UserSubmission>) -> UserSubmission {
    let mut texts = Vec::with_capacity(submissions.len());
    let mut images = Vec::new();
    for s in submissions {
        texts.push(s.text);
        images.extend(s.images);
    }
    UserSubmission {
        text: texts.join("\n\n"),
        images,
    }
}

/// Estimate the wire-side token total of a message history via the
/// cl100k_base fallback counter over each message's serialized form. Used
/// only for the `context_pruned` timeline event's before/after figures
/// (session-log-export Part C) — a faithful proxy, the same basis the
/// tokenizer-calibration sampler uses, not an exact provider count.
fn wire_token_total(history: &[Message]) -> u64 {
    history
        .iter()
        .map(|m| match serde_json::to_string(m) {
            Ok(s) => crate::tokens::count(&s) as u64,
            Err(_) => 0,
        })
        .sum()
}

/// Turn cap for the explore subagent's noninteractive loop. Real
/// exploration work needs headroom; 64 turns bounds runaway loops
/// without cutting legitimate work short.
pub(crate) const EXPLORE_MAX_TURNS: usize = 64;

/// Run a child agent's loop to completion synchronously. Used for
/// noninteractive subagents — explore primarily. Drops the child's
/// per-turn events on the floor (the parent's history already has a
/// ToolStart/End representing this call); only the final text comes
/// back. The loop is bounded by the `max_turns` parameter (each role
/// passes its own named constant — explore/docs-answerer 64, docs
/// resolver 24) to bound runaway loops; the over-limit error reports
/// that limit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_noninteractive(
    child: Agent,
    brief: String,
    session: Arc<Session>,
    locks: Arc<crate::locks::LockManager>,
    redact: Arc<RedactionTable>,
    cwd: std::path::PathBuf,
    interrupts: Arc<crate::engine::interrupt::InterruptHub>,
    cancel: tokio_util::sync::CancellationToken,
    approver: Option<Arc<crate::approval::Approver>>,
    loop_guard_threshold: u32,
    max_turns: usize,
) -> Result<String> {
    use crate::engine::agent::turn;

    // The child needs an event channel; we drain and discard.
    let (sink_tx, mut sink_rx) = mpsc::channel::<TurnEvent>(64);
    let drain = tokio::spawn(async move { while sink_rx.recv().await.is_some() {} });

    let agent = Arc::new(child);
    let mut history: Vec<Message> = Vec::new();
    let mut next_prompt = Message::user(brief);
    // A noninteractive subagent's own deferred-log (`plan.md §3d`). The
    // bundled leaves (explore/docs) lack `defer_to_orchestrator`, so this
    // stays empty for them; a custom subagent that holds the tool gets its
    // deferred items folded into the leaf report it returns up.
    let deferred_log = crate::engine::deferred::DeferredLog::new();

    for _ in 0..max_turns {
        let outcome = turn(
            &agent,
            &mut history,
            next_prompt,
            session.clone(),
            locks.clone(),
            redact.clone(),
            cwd.clone(),
            interrupts.clone(),
            cancel.clone(),
            approver.clone(),
            loop_guard_threshold,
            // A noninteractive child delegation recomposes its own fresh
            // system prompt on spawn, so it never needs the live
            // instructions-file diff injection.
            false,
            deferred_log.clone(),
            &sink_tx,
        )
        .await?;
        match outcome {
            TurnOutcome::Continue => {
                next_prompt = history
                    .pop()
                    .expect("Continue with empty history is unreachable");
            }
            TurnOutcome::Done => {
                drop(sink_tx);
                let _ = drain.await;
                let deferred = deferred_log.drain();
                return Ok(format!(
                    "{}{}",
                    collect_final_text(&history),
                    crate::engine::deferred::format_section(&deferred)
                ));
            }
            TurnOutcome::SpawnSubagent { .. }
            | TurnOutcome::SpawnNoninteractive { .. }
            | TurnOutcome::JobAction { .. } => {
                // explore is a leaf without `task`/`jobs`; this shouldn't
                // happen, but if it does we bail rather than spin (the
                // single async-job authority is the main driver, never a
                // noninteractive subagent — §22 anti-runaway).
                drop(sink_tx);
                let _ = drain.await;
                anyhow::bail!(
                    "noninteractive agent `{}` attempted to delegate or schedule a job",
                    agent.name
                );
            }
        }
    }
    drop(sink_tx);
    let _ = drain.await;
    anyhow::bail!(
        "noninteractive agent `{}` exceeded {max_turns} turns",
        agent.name
    )
}

/// Produce the shrunk version of a parent history for a delegation
/// (`prompts/compact-after-delegation.md`). `prune` is lossless + sync
/// (snapshot-dedup on a clone); `compact` reuses `compact.rs`'s brief
/// machinery to summarize the (pre-pruned) parent context into a single
/// dense message, with a prune-only fallback on model failure. Runs on the
/// background shrink task, off the parent's frame.
async fn run_shrink(
    strategy: crate::config::providers::ShrinkStrategy,
    parent_full: &[Message],
    agent: Arc<Agent>,
    cancel: tokio_util::sync::CancellationToken,
) -> Vec<Message> {
    use crate::config::providers::ShrinkStrategy;
    use crate::engine::deleg_shrink;
    match strategy {
        ShrinkStrategy::Prune => deleg_shrink::prune_shrink(parent_full),
        ShrinkStrategy::Compact => {
            let drafter = deleg_shrink::ModelBriefDrafter { agent, cancel };
            deleg_shrink::compact_shrink(parent_full, &drafter).await
        }
    }
}

fn collect_final_text(history: &[Message]) -> String {
    // The last assistant message in the history is the subagent's
    // final text. Walk back to find it.
    for msg in history.iter().rev() {
        if let Message::Assistant { content, .. } = msg {
            let text = crate::engine::message::extract_text(content);
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a driver rooted on a keyless localhost agent (the model is
    /// never called by the action-dispatch paths under test).
    fn test_driver(max_jobs: usize) -> (Driver, tempfile::TempDir) {
        use crate::config::providers::{ActiveModelRef, ProviderEntry, ProvidersConfig};
        use std::collections::BTreeMap;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let db = crate::db::Db::open_in_memory().unwrap();
        let session = Arc::new(Session::create(db.clone(), root.clone(), "Build").unwrap());
        let locks = Arc::new(crate::locks::LockManager::from_db(db).unwrap());
        let rcfg = crate::config::extended::RedactConfig::default();
        let redact = Arc::new(RedactionTable::build(&rcfg, &root).unwrap());

        let mut providers = BTreeMap::new();
        providers.insert(
            "lmstudio".to_string(),
            ProviderEntry {
                url: "http://localhost:1/v1".into(),
                headers: vec![],
                ..ProviderEntry::default()
            },
        );
        let pcfg = ProvidersConfig {
            providers,
            active_model: Some(ActiveModelRef {
                provider: "lmstudio".into(),
                model: "local".into(),
                thinking_mode: None,
            }),
            ..ProvidersConfig::default()
        };
        let model = Arc::new(crate::engine::model::Model::from_config(&pcfg).unwrap());
        let agent = Arc::new(Agent {
            name: "Build".into(),
            system: String::new(),
            tools: crate::engine::tool::ToolBox::new(),
            model,
            params: crate::engine::model::ModelParams::default(),
            llm_mode: crate::config::extended::LlmMode::default(),
        });
        let driver = Driver::with_max_jobs(session, locks, redact, root, agent, max_jobs);
        (driver, tmp)
    }

    #[test]
    fn new_constructs_idle_driver() {
        // `Driver::new` is the public default-cap constructor; exercise it
        // so the default path stays alive + correct.
        let (driver, _t) = test_driver(crate::engine::jobs::DEFAULT_MAX_CONCURRENT_JOBS);
        let agent = driver.stack[0].agent.clone();
        let d2 = Driver::new(
            driver.session.clone(),
            driver.locks.clone(),
            driver.redact.clone(),
            driver.cwd.clone(),
            agent,
        );
        assert_eq!(d2.active_agent(), "Build");
        assert!(!d2.jobs.has_loop());
        assert_eq!(
            d2.jobs.max_concurrent,
            crate::engine::jobs::DEFAULT_MAX_CONCURRENT_JOBS
        );
    }

    /// Build a tiny history with two identical `read` snapshots (one
    /// elidable). Mirrors the prune module's wire shape.
    fn dup_read_history() -> Vec<Message> {
        use rig::OneOrMany;
        use rig::message::{AssistantContent, ToolResult, ToolResultContent, UserContent};
        let call = |id: &str| Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::ToolCall(
                crate::engine::message::ToolCall {
                    id: id.to_string(),
                    call_id: None,
                    function: rig::message::ToolFunction {
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": "/abs/foo.rs" }),
                    },
                    signature: None,
                    additional_params: None,
                },
            )),
        };
        let result = |id: &str| Message::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: id.to_string(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text(
                    "FULL SNAPSHOT BODY with enough tokens to matter here",
                )),
            })),
        };
        vec![call("c1"), result("c1"), call("c2"), result("c2")]
    }

    /// `/prune` (and auto-prune) target the **foreground** agent only —
    /// the top of the interactive-agent stack. A suspended parent frame's
    /// history is never touched (GOALS §3b scope).
    #[tokio::test]
    async fn prune_targets_foreground_subagent_only() {
        let (mut driver, _tmp) = test_driver(8);
        let (tx, mut rx) = mpsc::channel::<TurnEvent>(64);

        // Parent (root) frame carries elidable duplicate reads.
        driver.stack[0].history = dup_read_history();

        // Push an interactive subagent frame with its OWN duplicate reads.
        let child = driver.stack[0].agent.clone();
        driver.stack.push(AgentSession {
            agent: child,
            history: dup_read_history(),
            answering: None,
            deferred_log: crate::engine::deferred::DeferredLog::new(),
        });

        // Prune the foreground (the subagent on top).
        driver.do_prune(false, &tx).await;
        drop(tx);
        while rx.recv().await.is_some() {}

        // Foreground (top) was pruned: older body became a marker.
        let top = driver.stack.last().unwrap();
        let plan_top = prune::dedup_plan(&top.history);
        assert!(plan_top.is_empty(), "foreground should be fully pruned");

        // Parent (suspended) is untouched: still has an elidable dup.
        let parent = &driver.stack[0];
        let plan_parent = prune::dedup_plan(&parent.history);
        assert!(
            !plan_parent.is_empty(),
            "suspended parent frame must NOT be pruned"
        );
    }

    /// The watermark short-circuits auto-prune: after a prune, with no
    /// history growth, `maybe_auto_prune` is a no-op even when cold.
    #[tokio::test]
    async fn auto_prune_watermark_short_circuits() {
        let (mut driver, _tmp) = test_driver(8);
        let (tx, mut rx) = mpsc::channel::<TurnEvent>(64);
        driver.stack[0].history = dup_read_history();

        // Cache is cold (no send yet) and there's something prunable →
        // first auto-prune fires.
        assert!(driver.maybe_auto_prune(&tx).await, "first auto-prune fires");
        // History length unchanged since → watermark short-circuits.
        assert!(
            !driver.maybe_auto_prune(&tx).await,
            "watermark short-circuits with no growth"
        );
        drop(tx);
        while rx.recv().await.is_some() {}
    }

    /// Nothing prunable → auto-prune is a no-op and emits no Pruned event.
    #[tokio::test]
    async fn auto_prune_noop_when_nothing_prunable() {
        let (mut driver, _tmp) = test_driver(8);
        let (tx, _rx) = mpsc::channel::<TurnEvent>(64);
        // Empty foreground history: nothing to prune.
        assert!(!driver.maybe_auto_prune(&tx).await);
    }

    #[tokio::test]
    async fn dispatch_loop_start_and_cancel() {
        let (mut driver, _tmp) = test_driver(8);
        let out = driver
            .dispatch_job_action(&serde_json::json!({
                "action": "loop.start",
                "args": { "interval": 60, "prompt": "poll", "limit": 0 }
            }))
            .unwrap();
        assert!(out.starts_with("started loop"), "got {out}");
        assert!(driver.jobs.has_loop());
        // The capability hint for loop.cancel fires exactly once.
        let hints = driver.pending_capability_hints();
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("loop.cancel"));
        assert!(
            driver.pending_capability_hints().is_empty(),
            "hint is one-shot"
        );

        let job_id = out
            .split('`')
            .nth(1)
            .expect("job id in backticks")
            .to_string();
        let cancel = driver
            .dispatch_job_action(&serde_json::json!({
                "action": "loop.cancel",
                "args": { "job_id": job_id }
            }))
            .unwrap();
        assert!(cancel.starts_with("cancelled"), "got {cancel}");
        assert!(!driver.jobs.has_loop());
    }

    #[tokio::test]
    async fn dispatch_timer_is_loop_with_limit_one() {
        let (mut driver, _tmp) = test_driver(8);
        let out = driver
            .dispatch_job_action(&serde_json::json!({
                "action": "loop.start",
                "args": { "interval": 5, "prompt": "fire", "limit": 1 }
            }))
            .unwrap();
        assert!(out.starts_with("started timer"), "got {out}");
    }

    #[tokio::test]
    async fn dispatch_list_and_capacity_error() {
        let (mut driver, _tmp) = test_driver(1);
        assert_eq!(
            driver
                .dispatch_job_action(&serde_json::json!({ "action": "list" }))
                .unwrap(),
            "no active jobs"
        );
        driver
            .dispatch_job_action(&serde_json::json!({
                "action": "loop.start",
                "args": { "interval": 60, "prompt": "p", "limit": 0 }
            }))
            .unwrap();
        let listed = driver
            .dispatch_job_action(&serde_json::json!({ "action": "list" }))
            .unwrap();
        assert!(listed.contains("loop"), "got {listed}");
        // Cap is 1 — a second start errors.
        let err = driver
            .dispatch_job_action(&serde_json::json!({
                "action": "loop.start",
                "args": { "interval": 60, "prompt": "q", "limit": 0 }
            }))
            .unwrap_err();
        assert!(format!("{err}").contains("max concurrent jobs"));
    }

    #[test]
    fn dispatch_background_tail_unknown_id() {
        let (mut driver, _tmp) = test_driver(8);
        let out = driver
            .dispatch_job_action(&serde_json::json!({
                "action": "background.tail",
                "args": { "job_id": "job-nope" }
            }))
            .unwrap();
        assert!(out.contains("no live background"), "got {out}");
    }

    /// Config resolution: with no `config.json` on disk, the
    /// delegation-shrink strategy defaults to `prune` (lowest quality
    /// loss, priority #1) and a 30s margin.
    #[test]
    fn resolve_shrink_config_defaults_to_prune() {
        use crate::config::providers::ShrinkStrategy;
        let (driver, _tmp) = test_driver(8);
        let shrink = driver.resolve_shrink_config();
        assert_eq!(shrink.strategy, ShrinkStrategy::Prune);
        assert_eq!(shrink.margin_secs, 30);
    }

    /// `finish_delegation_shrink`: a COLD-at-return parent (no-cache
    /// provider → always cold) with a computed prune-shrink resumes on the
    /// SHRUNK context — the driver swaps the foreground frame's history.
    #[tokio::test]
    async fn finish_delegation_shrink_cold_swaps_parent_history() {
        use crate::config::providers::{CacheConfig, CacheMode, ShrinkConfig};
        use crate::engine::deleg_shrink::DelegationShrink;

        let (mut driver, _tmp) = test_driver(8);
        let (tx, mut rx) = mpsc::channel::<TurnEvent>(64);

        // Parent (foreground) frame carries elidable duplicate reads.
        driver.stack[0].history = dup_read_history();
        assert!(
            !prune::dedup_plan(&driver.stack[0].history).is_empty(),
            "parent has something prunable"
        );

        // A tracker on a no-cache provider is always cold; pre-compute the
        // prune-shrink as the parallel task would have.
        let none = CacheConfig {
            mode: CacheMode::None,
            ttl_secs: 300,
        };
        let mut tracker = DelegationShrink::new(none, &ShrinkConfig::default());
        let shrunk = crate::engine::deleg_shrink::prune_shrink(&driver.stack[0].history);
        tracker.set_shrunk(shrunk);

        driver.finish_delegation_shrink(tracker, None, &tx).await;
        drop(tx);
        while rx.recv().await.is_some() {}

        // Cold → resumed on the shrunk context: the foreground history is
        // now fully pruned (nothing left elidable).
        assert!(
            prune::dedup_plan(&driver.stack[0].history).is_empty(),
            "cold parent resumed on the shrunk (pruned) context"
        );
    }

    /// `finish_delegation_shrink`: a HOT-at-return parent (cache-capable,
    /// within TTL) keeps its FULL context even when a shrink was computed —
    /// no quality loss, the cache is paid for.
    #[tokio::test]
    async fn finish_delegation_shrink_hot_keeps_full() {
        use crate::config::providers::{CacheConfig, CacheMode, ShrinkConfig};
        use crate::engine::deleg_shrink::DelegationShrink;

        let (mut driver, _tmp) = test_driver(8);
        let (tx, mut rx) = mpsc::channel::<TurnEvent>(64);

        driver.stack[0].history = dup_read_history();

        // Ephemeral cache, generous TTL, tracker started "now" → hot.
        let ephemeral = CacheConfig {
            mode: CacheMode::Ephemeral,
            ttl_secs: 3600,
        };
        let mut tracker = DelegationShrink::new(ephemeral, &ShrinkConfig::default());
        tracker.set_shrunk(vec![Message::user("shrunk")]);

        driver.finish_delegation_shrink(tracker, None, &tx).await;
        drop(tx);
        while rx.recv().await.is_some() {}

        // Hot → full context retained: still has the elidable duplicate.
        assert!(
            !prune::dedup_plan(&driver.stack[0].history).is_empty(),
            "hot parent kept its full (un-shrunk) context"
        );
    }

    /// `begin_delegation_shrink` on a no-cache provider spawns an EAGER
    /// shrink task that finishes promptly (ZERO delay); the prune-shrink
    /// result is adopted on `finish`. Exercises the full begin→finish path.
    #[tokio::test]
    async fn begin_delegation_shrink_eager_on_no_cache() {
        let (mut driver, _tmp) = test_driver(8);
        let (tx, mut rx) = mpsc::channel::<TurnEvent>(64);

        // Default test driver uses provider `lmstudio` with no cache config
        // → CacheMode::None → eager.
        driver.stack[0].history = dup_read_history();
        let parent_full = driver.stack[0].history.clone();
        let (tracker, handle) = driver.begin_delegation_shrink(parent_full);
        assert!(handle.is_some(), "a shrink task was spawned");

        // Let the eager task run to completion.
        let handle = handle.unwrap();
        let shrunk = handle.await.unwrap();
        assert!(
            prune::dedup_plan(&shrunk).is_empty(),
            "eager prune-shrink produced a pruned history"
        );

        // Re-run begin to get a fresh tracker + handle to finish with the
        // already-computed result (the prior handle was consumed above).
        let (mut tracker2, h) = driver.begin_delegation_shrink(driver.stack[0].history.clone());
        if let Some(h) = h {
            h.abort();
        }
        tracker2.set_shrunk(shrunk);
        let _ = tracker; // first tracker not needed further
        driver.finish_delegation_shrink(tracker2, None, &tx).await;
        drop(tx);
        while rx.recv().await.is_some() {}

        // No-cache provider is always cold → swapped to the shrunk context.
        assert!(prune::dedup_plan(&driver.stack[0].history).is_empty());
    }
}
