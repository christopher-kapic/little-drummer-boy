//! Per-session worker. One alive at a time per session_id.
//!
//! Owns the [`crate::engine::Driver`] for the session, the
//! per-session redaction table, and the model client. Accepts work
//! requests from any number of attached clients via an
//! `mpsc::Sender<SessionWork>` and fans events out to all attached
//! clients via `broadcast::Sender<proto::Event>`.
//!
//! Lifecycle:
//!
//! - **Spawned** lazily on the first `Attach` to a session_id.
//! - **Stays alive** across client disconnects — per GOALS §8b a
//!   session outlives its TUI client.
//! - **Exits** on explicit `Shutdown` (daemon teardown) or when the
//!   session ends (`Session::end`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::Result;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use crate::daemon::proto;
use crate::engine::builtin::{self, SpawnArgs};
use crate::engine::model::{Model, ModelParams};
use crate::engine::{Driver, TurnEvent};
use crate::locks::LockManager;
use crate::redact::RedactionTable;
use crate::session::Session;

/// Channel capacity for outbound events fanned to attached clients.
/// Lagging clients lose events (consistent with the fire-and-forget
/// event-stream contract); a client that lags has to reattach to
/// re-sync.
const EVENT_BROADCAST_CAPACITY: usize = 1024;

/// Inbound work-queue capacity. Generous — user messages, cancels,
/// and resolves are tiny.
const WORK_QUEUE_CAPACITY: usize = 64;

/// Live in-daemon status of a session, maintained by the event
/// forwarder (GOALS §17f / §22). The `JobAuthority` and the driver turn
/// loop are the authorities for jobs and turn-state respectively; their
/// emissions all funnel through the worker's single forwarding seam, so
/// observing them there keeps the single-authority rule intact while
/// giving the browser a cheap, lock-free read for tiers 1-2.
#[derive(Default)]
pub struct LiveState {
    /// Count of live async jobs (loop/timer/background). `JobStarted`
    /// increments, `JobCompleted` decrements.
    active_jobs: AtomicUsize,
    /// Whether a turn is in flight: set on `ThinkingStarted`, cleared on
    /// `AgentIdle`.
    processing: AtomicBool,
}

impl LiveState {
    pub fn has_active_jobs(&self) -> bool {
        self.active_jobs.load(Ordering::Relaxed) > 0
    }

    pub fn processing(&self) -> bool {
        self.processing.load(Ordering::Relaxed)
    }
}

/// Handle one or more client tasks hold to drive a session. Cheap to
/// clone — both channels inside are reference-counted.
#[derive(Clone)]
pub struct SessionWorkerHandle {
    pub session_id: Uuid,
    pub project_root: PathBuf,
    pub active_agent_name: String,
    work_tx: mpsc::Sender<SessionWork>,
    event_tx: broadcast::Sender<proto::Event>,
    /// Live job/turn status for the `/sessions` browser (GOALS §17f).
    live: Arc<LiveState>,
    /// Count of attached *interactive* clients — ones that can answer an
    /// interrupt (the loop guard reads this to decide headless behavior,
    /// GOALS §1/§12). Shared with the worker's [`InterruptHub`]; the
    /// server bumps/decrements it as interactive clients attach/detach via
    /// [`Self::register_interactive_client`].
    interactive_clients: Arc<std::sync::atomic::AtomicUsize>,
    /// Shared session handle (sandboxing part 2): lets the server flip
    /// the per-session sandbox-enabled flag (`/sandbox`) directly and
    /// reply synchronously — the flag is an atomic on the `Arc<Session>`
    /// the worker's driver also reads per tool call.
    session: Arc<Session>,
}

impl SessionWorkerHandle {
    /// Set or toggle the session's filesystem-sandbox flag (sandboxing
    /// part 2). `None` toggles; `Some(b)` sets explicitly. Returns the
    /// resulting state. Effective immediately for the next tool call (the
    /// driver reads the same atomic). Broadcasts a `SandboxState` event
    /// so every attached client stays in sync.
    pub fn set_sandbox(&self, enabled: Option<bool>) -> bool {
        let new = match enabled {
            Some(b) => self.session.set_sandbox_enabled(b),
            None => self.session.toggle_sandbox_enabled(),
        };
        let _ = self.event_tx.send(proto::Event::SandboxState {
            session_id: self.session_id,
            enabled: new,
        });
        new
    }

    /// Register an interactive client (one that can answer interrupts —
    /// the TUI; later the remote dashboard) for the lifetime of the
    /// returned guard. The loop guard (GOALS §1/§12) reads the resulting
    /// count to tell an interactive session from a headless run: while at
    /// least one guard is alive, a back-to-back repeat prompts; with none,
    /// it auto-rejects without blocking. Dropping the guard (client
    /// detach / disconnect) decrements the count.
    pub fn register_interactive_client(&self) -> InteractiveClientGuard {
        self.interactive_clients
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        InteractiveClientGuard {
            counter: self.interactive_clients.clone(),
        }
    }
}

/// RAII guard for an attached interactive client. Decrements the worker's
/// interactive-client count on drop, so a disconnect (even an abrupt one)
/// correctly returns the session to headless behavior.
pub struct InteractiveClientGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for InteractiveClientGuard {
    fn drop(&mut self) {
        // Saturating: never underflow even on a double-drop path.
        let _ = self.counter.fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |n| Some(n.saturating_sub(1)),
        );
    }
}

impl SessionWorkerHandle {
    pub async fn send_work(&self, work: SessionWork) -> Result<()> {
        self.work_tx
            .send(work)
            .await
            .map_err(|_| anyhow::anyhow!("session worker {} has shut down", self.session_id))
    }

    /// Subscribe to the event stream. Each attached client gets its
    /// own receiver; a lagging receiver drops events (per the design).
    pub fn subscribe(&self) -> broadcast::Receiver<proto::Event> {
        self.event_tx.subscribe()
    }

    /// Live job/turn status snapshot for the browser's tiers 1-2.
    pub fn live_status(&self) -> (bool, bool) {
        (self.live.has_active_jobs(), self.live.processing())
    }

    /// The session's project id — read from the in-memory session so it is
    /// available before the `sessions` row is persisted
    /// (session-id-display-and-lazy-persist).
    pub fn project_id(&self) -> String {
        self.session.project_id.clone()
    }

    /// The session's 6-char display id — read from the in-memory session so
    /// it is available before the `sessions` row is persisted.
    pub fn short_id(&self) -> String {
        self.session.short_id.clone()
    }
}

/// Work items a client can ask the worker to perform.
#[derive(Debug)]
pub enum SessionWork {
    UserMessage(crate::engine::message::UserSubmission),
    Cancel,
    ResolveInterrupt {
        interrupt_id: Uuid,
        response: proto::ResolveResponse,
    },
    SetActiveModel {
        provider: String,
        model: String,
    },
    SetAgent {
        name: String,
    },
    /// Switch the active `llm_mode` live (`/llm-mode`,
    /// `prompts/llm-modes-defensive-normal.md`). `mode = None` toggles.
    SetLlmMode {
        mode: Option<crate::config::extended::LlmMode>,
    },
    /// Cancel a live async job (loop / timer / background, GOALS §22) by
    /// id, on behalf of the **human** ("stop checking the deploy" /
    /// `/jobs cancel <id>`). Routed to the driver's single async-job
    /// authority.
    CancelJob {
        job_id: String,
    },
    /// Run `/prune` (snapshot dedup) on the foreground agent now.
    Prune,
    /// Run `/compact` (fresh-thread handoff) on the foreground agent.
    Compact,
    /// Pin a user message verbatim for the next `/compact` (`/pin`).
    Pin {
        text: String,
    },
    Shutdown,
}

/// One-shot constructor: spawn the worker and return its handle.
///
/// `client_no_sandbox` is the attaching client's `--no-sandbox` flag
/// (sandboxing part 2): `Some(true)` means the client asked for new
/// sessions it creates to be unsandboxed. The session-spawn default is
/// resolved here by the precedence daemon-flag → client-flag → ON.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    session: Arc<Session>,
    locks: Arc<LockManager>,
    redact: Arc<RedactionTable>,
    model: Arc<Model>,
    model_override: Option<Arc<Model>>,
    project_root: PathBuf,
    client_no_sandbox: bool,
    extended_cfg: &crate::config::extended::ExtendedConfig,
) -> (SessionWorkerHandle, tokio::task::JoinHandle<()>) {
    let session_id = session.id;
    // The primary the chrome's active-agent slot opens on: the stored agent
    // (resume) or the configured default (`Auto` unless pinned). The worker
    // re-derives the same value via `resolve_root_agent` and emits
    // `PrimarySwapped` on any later swap, so this is purely the start state.
    let initial_agent = resolve_root_agent(session_id, &session.db, extended_cfg);
    // Resolve the new-session sandbox default (highest wins):
    //   (a) daemon launched `--no-sandbox` → OFF for ALL sessions.
    //   (b) else this client passed `--no-sandbox` → OFF for the
    //       sessions it creates.
    //   (c) else ON.
    // A later `/sandbox` flip overrides this for the session.
    session.set_sandbox_enabled(resolve_sandbox_default(client_no_sandbox));
    // Command-approval mode (`prompts/utility-command-safety-gate.md`): new
    // sessions start in the configured default (`manual` unless overridden).
    // A later `/settings` change re-resolves on the next session.
    session.set_approval_mode(extended_cfg.default_approval_mode);
    let (work_tx, work_rx) = mpsc::channel::<SessionWork>(WORK_QUEUE_CAPACITY);
    let (event_tx, _initial_rx) = broadcast::channel::<proto::Event>(EVENT_BROADCAST_CAPACITY);
    let live = Arc::new(LiveState::default());
    // Shared interactive-client counter (GOALS §1/§12). Owned here, handed
    // to the worker's `InterruptHub` and stored on the handle so attach /
    // detach and the loop guard read the same cell.
    let interactive_clients = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let handle = SessionWorkerHandle {
        session_id,
        project_root: project_root.clone(),
        active_agent_name: initial_agent,
        work_tx,
        event_tx: event_tx.clone(),
        live: live.clone(),
        interactive_clients: interactive_clients.clone(),
        session: session.clone(),
    };

    // Return the worker's `JoinHandle` so the registry can *await* it on a
    // graceful drain (`daemon-graceful-drain-shutdown.md`) — today's
    // `shutdown_all` fires `Shutdown` and forgets, with no way to know the
    // in-flight turn finished. The handle also lets the force path
    // `abort()` a worker whose provider call hung past the grace deadline.
    let join = tokio::spawn(run_worker(
        session,
        locks,
        redact,
        model,
        model_override,
        project_root,
        work_rx,
        event_tx,
        live,
        interactive_clients,
    ));

    (handle, join)
}

#[allow(clippy::too_many_arguments)]
async fn run_worker(
    session: Arc<Session>,
    locks: Arc<LockManager>,
    redact: Arc<RedactionTable>,
    model: Arc<Model>,
    model_override: Option<Arc<Model>>,
    project_root: PathBuf,
    mut work_rx: mpsc::Receiver<SessionWork>,
    event_tx: broadcast::Sender<proto::Event>,
    live: Arc<LiveState>,
    interactive_clients: Arc<std::sync::atomic::AtomicUsize>,
) {
    let session_id = session.id;

    // The layered `extended-config.json` resolved once at session start.
    // The active LLM mode (`prompts/llm-modes-defensive-normal.md`) and the
    // default primary agent (the auto-router feature) both read it; the live
    // `/llm-mode` switch overrides the mode in place via `DriverControl`.
    let extended_cfg = crate::config::extended::load_for_cwd(&project_root);
    let llm_mode = extended_cfg.llm_mode;
    let spawn_args = SpawnArgs {
        model,
        params: ModelParams::default(),
        cwd: project_root.clone(),
        session_short_id: session.short_id.clone(),
        // The daemon root is always the user-facing interactive agent —
        // it gets the cross-session recall tools.
        interactive: true,
        llm_mode,
        // Plan-level model override (`plan-duplication-and-model-override.md`):
        // when set, the root and every spawned subagent run under it.
        model_override: model_override.clone(),
    };
    // Root primary: the session's stored active agent (so a resume restarts
    // on `Plan` after a `/plan` swap or whichever primary `Auto` handed off
    // to, `plan.md §4.6.d`), falling back to the configured default
    // (`Auto` unless the user pinned another) when it's unset/unknown.
    let root_agent_name = resolve_root_agent(session_id, &session.db, &extended_cfg);
    let root = Arc::new(
        builtin::load(&root_agent_name, &spawn_args)
            .unwrap_or_else(|_| builtin::build(&spawn_args)),
    );

    // Snapshot the resolved agent-guidance file body that just went into
    // the frozen system block (live instructions-file diff injection,
    // prompt `instructions-file-live-diff.md`). This is the start-of-
    // session baseline a later in-place edit is diffed against; the driver
    // checks it on every outbound request. Recomputed on each worker spawn
    // (fresh or resumed) because `builtin::build` re-composes the system
    // block from the current file each time.
    session.snapshot_guidance_baseline(&project_root);

    let (driver_input_tx, driver_input_rx) =
        mpsc::channel::<crate::engine::message::UserSubmission>(WORK_QUEUE_CAPACITY);
    let (driver_control_tx, driver_control_rx) =
        mpsc::channel::<crate::engine::driver::DriverControl>(WORK_QUEUE_CAPACITY);
    let (engine_event_tx, mut engine_event_rx) = mpsc::channel::<TurnEvent>(WORK_QUEUE_CAPACITY);

    // Forward engine events → broadcast channel as proto::Event, and
    // maintain the live job/turn status (GOALS §17f) off the same
    // authoritative stream. These signals originate from the driver turn
    // loop (`ThinkingStarted` / `AgentIdle`) and the single `JobAuthority`
    // (`JobStarted` / `JobCompleted`); the forwarder is the one seam they
    // all pass through, so updating here never duplicates the authority.
    let event_tx_for_forward = event_tx.clone();
    let live_for_forward = live.clone();
    let forward = tokio::spawn(async move {
        while let Some(event) = engine_event_rx.recv().await {
            for ev in turn_event_to_proto(event, session_id) {
                match &ev {
                    proto::Event::ThinkingStarted { .. } => {
                        live_for_forward.processing.store(true, Ordering::Relaxed);
                    }
                    proto::Event::AgentIdle { .. } => {
                        live_for_forward.processing.store(false, Ordering::Relaxed);
                    }
                    proto::Event::JobStarted { .. } => {
                        live_for_forward.active_jobs.fetch_add(1, Ordering::Relaxed);
                    }
                    proto::Event::JobCompleted { .. } => {
                        // Saturating: never underflow if a completion is
                        // ever seen without its start (defensive).
                        let _ = live_for_forward.active_jobs.fetch_update(
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                            |n| Some(n.saturating_sub(1)),
                        );
                    }
                    _ => {}
                }
                // `send` returns `Err` only when there are no
                // subscribers — that's fine, nobody is listening.
                let _ = event_tx_for_forward.send(ev);
            }
        }
    });

    // Build the driver, then capture its async-job command sender (GOALS
    // §22) so a human-initiated `/jobs cancel` reaches the single
    // authority before moving the driver into its task.
    let max_concurrent_jobs = max_concurrent_jobs_for(&project_root);
    let mut driver = Driver::with_max_jobs(
        session.clone(),
        locks.clone(),
        redact.clone(),
        project_root.clone(),
        root,
        max_concurrent_jobs,
    );
    // Propagate any plan-level model override to the whole delegation tree
    // (`plan-duplication-and-model-override.md`): the root already runs under
    // it (loaded with the override `SpawnArgs`); this carries it down to
    // delegated subagents whose frontmatter would otherwise win.
    driver.set_model_override(model_override);
    let job_cmd_tx = driver.job_command_sender();
    // Capture the driver's cancel handle (GOALS §3a) before moving it into
    // its task, so a user ctrl+c (`SessionWork::Cancel`) can abort the
    // in-flight user-message run — aborting the streaming inference and
    // killing any running `bash` subprocess.
    let cancel_handle = driver.cancel_handle();

    // Interrupt wakeup hub (GOALS §3b): wire the driver's tool calls to
    // the client event fan-out so the `question` tool can raise an
    // interrupt and block on the answer. We keep the same `Arc` so the
    // `ResolveInterrupt` handler below can wake the blocked tool. The
    // hub must be installed before the driver loop starts.
    let interrupts = Arc::new(crate::engine::interrupt::InterruptHub::new(
        event_tx.clone(),
        interactive_clients,
    ));
    driver.set_interrupt_hub(interrupts.clone());

    // Command/path approval driver (sandboxing part 2). Built on the
    // session's grant store + the client-wired interrupt hub above, so a
    // `bash` run-fail-escalate or a native out-of-boundary path access
    // raises a prompt that fans out to the attached client exactly like a
    // `question`. The driver threads it into every `ToolCtx`. Installed
    // after the hub (the approver captures the same `Arc`). The active
    // agent for the prompt is the foreground primary agent at spawn time;
    // a delegated coder shares the same approver via the `ToolCtx`
    // `Arc`, so grants persist across the delegation tree.
    let grant_store = crate::approval::store::GrantStore::new(
        session.db.clone(),
        session_id,
        project_root.clone(),
    );
    let approver = Arc::new(crate::approval::Approver::new(
        grant_store,
        session.db.clone(),
        session_id,
        initial_active_agent(&extended_cfg),
        interrupts.clone(),
    ));
    driver.set_approver(approver);

    // Loop-guard threshold (GOALS §1/§12) from the layered config, same
    // discovery the jobs cap uses. Clamped to ≥ 2 by the setter.
    driver.set_loop_guard_threshold(loop_guard_threshold_for(&project_root));

    // Seed-tool re-execution (`/compact` handoff, T6.e): if this session
    // was created by `/compact`, its derived seed-tool plan was persisted
    // keyed by this session id. Drain it and dispatch the calls (read-only
    // / idempotent only) into the fresh agent's initial context *before*
    // the first inference — re-executed, never replayed from a stale
    // transcript. Done synchronously before the driver loop starts so it
    // can never race the first user message. Best-effort.
    if let Ok(seeds) = session.db.take_seed_tools(session_id)
        && !seeds.is_empty()
    {
        driver.run_seed_tools(&seeds, &engine_event_tx).await;
    }

    // Spawn the driver loop.
    let driver_handle = tokio::spawn(async move {
        if let Err(e) = driver
            .run_main_loop(driver_input_rx, driver_control_rx, &engine_event_tx)
            .await
        {
            tracing::error!(error = ?e, "driver loop terminated with error");
        }
    });

    // Main work loop.
    while let Some(work) = work_rx.recv().await {
        match work {
            SessionWork::UserMessage(submission) => {
                // Lazy persistence (session-id-display-and-lazy-persist): the
                // first user message is what commits the `sessions` row.
                // Flush it *before* `touch()` and before the driver runs, so
                // the row exists ahead of any dependent write (tool_calls,
                // inference_calls, locks). A persist failure aborts the
                // message rather than letting dependents reference a missing
                // row.
                match session.persist_if_needed() {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!(error = %e, session_id = %session_id,
                            "persisting session on first message failed; dropping message");
                        continue;
                    }
                }
                if let Err(e) = session.touch() {
                    tracing::warn!(error = %e, "session touch failed");
                }
                if driver_input_tx.send(submission).await.is_err() {
                    tracing::warn!(session_id = %session_id, "driver input channel closed");
                    break;
                }
            }
            SessionWork::Cancel => {
                // User ctrl+c (`CancelTurn`). Fire the in-flight run's
                // cancellation token: the driver's `turn` aborts the
                // streaming inference (returning an `InferenceCancelled`
                // sentinel that unwinds the run cleanly), and any running
                // `bash` subprocess is killed via its process group. Safe
                // and idempotent at idle / mid-cancel — `CancelHandle::cancel`
                // is a no-op when no run is in flight. The driver then emits
                // `AgentIdle`, clearing the TUI's busy state.
                tracing::info!(session_id = %session_id, "cancel requested");
                cancel_handle.cancel();
            }
            SessionWork::ResolveInterrupt {
                interrupt_id,
                response,
            } => {
                if let Err(e) = session.db.resolve_interrupt(interrupt_id, &response) {
                    tracing::warn!(error = %e, %interrupt_id, "resolve_interrupt failed");
                }
                let _ = event_tx.send(proto::Event::InterruptResolved {
                    session_id,
                    interrupt_id,
                });
                // Engine-side wakeup (GOALS §3b): hand the resolution to
                // whatever tool call is blocked on this interrupt id (the
                // `question` tool). `false` just means nobody was blocked
                // locally — e.g. a `jobs` needs-attention nudge — and the
                // DB row update above is the only effect.
                interrupts.resolve(interrupt_id, response);
            }
            SessionWork::SetActiveModel { provider, model } => {
                if let Err(e) = session.set_active_model(&provider, &model) {
                    tracing::warn!(error = %e, "set_active_model failed");
                }
                // Active Model swap takes effect on the next session.
                // Mid-session swap isn't supported in v1 because the
                // Driver holds the model client by Arc.
            }
            SessionWork::SetAgent { name } => {
                // Persist the active-agent choice so a resume restarts on it,
                // then swap the live primary in place at the idle boundary
                // (`/plan` → `Plan`, `/build` → `Build`, `plan.md §4.6.d`).
                if let Err(e) = session.set_active_agent(&name) {
                    tracing::warn!(error = %e, "set_active_agent failed");
                }
                if driver_control_tx
                    .send(crate::engine::driver::DriverControl::SwapPrimary { name })
                    .await
                    .is_err()
                {
                    tracing::warn!(session_id = %session_id, "driver control channel closed");
                }
            }
            SessionWork::SetLlmMode { mode } => {
                // Resolve toggle against the current config value (the
                // single source of truth shared with `/settings` + the
                // config file), persist the resolved value so a resume keeps
                // it, then route the explicit mode to the driver to rebuild
                // the root agent in place.
                let current = crate::config::extended::load_for_cwd(&project_root).llm_mode;
                let resolved = mode.unwrap_or_else(|| current.toggled());
                if let Err(e) = persist_llm_mode(&project_root, resolved) {
                    tracing::warn!(error = %e, "persisting llm_mode failed");
                }
                if driver_control_tx
                    .send(crate::engine::driver::DriverControl::SetLlmMode {
                        mode: Some(resolved),
                    })
                    .await
                    .is_err()
                {
                    tracing::warn!(session_id = %session_id, "driver control channel closed");
                }
            }
            SessionWork::CancelJob { job_id } => {
                if job_cmd_tx
                    .send(crate::engine::jobs::JobCommand::Cancel { job_id })
                    .await
                    .is_err()
                {
                    tracing::warn!(session_id = %session_id, "job command channel closed");
                }
            }
            SessionWork::Prune => {
                if driver_control_tx
                    .send(crate::engine::driver::DriverControl::Prune)
                    .await
                    .is_err()
                {
                    tracing::warn!(session_id = %session_id, "driver control channel closed");
                }
            }
            SessionWork::Compact => {
                if driver_control_tx
                    .send(crate::engine::driver::DriverControl::Compact)
                    .await
                    .is_err()
                {
                    tracing::warn!(session_id = %session_id, "driver control channel closed");
                }
            }
            SessionWork::Pin { text } => {
                if driver_control_tx
                    .send(crate::engine::driver::DriverControl::Pin { text })
                    .await
                    .is_err()
                {
                    tracing::warn!(session_id = %session_id, "driver control channel closed");
                }
            }
            SessionWork::Shutdown => {
                break;
            }
        }
    }

    // Drain: close the driver input → the driver finishes its current
    // turn (if any) and exits. Then the engine event channel closes
    // and the forwarder task exits.
    drop(driver_input_tx);
    let _ = driver_handle.await;
    let _ = forward.await;

    // Mark session ended in DB.
    if let Err(e) = session.end() {
        tracing::warn!(error = %e, "session.end() failed during shutdown");
    }
    let _ = event_tx.send(proto::Event::SessionEnded {
        session_id,
        reason: "worker stopped".into(),
    });
    tracing::info!(session_id = %session_id, "session worker exited");
}

/// Convert a single engine `TurnEvent` into one or more wire
/// `proto::Event`s. Some events (e.g. `ThinkingStarted`) map 1:1;
/// others (subagent spawn / report) are kept as the natural-enough
/// proto equivalents. Returning a `Vec` keeps the door open for a
/// 1:N expansion when, e.g., we attach a recovery chip alongside a
/// `ToolEnd` in the future.
fn turn_event_to_proto(event: TurnEvent, session_id: Uuid) -> Vec<proto::Event> {
    match event {
        TurnEvent::ThinkingStarted { agent } => {
            vec![proto::Event::ThinkingStarted { session_id, agent }]
        }
        TurnEvent::Reconnecting { agent, attempt } => {
            vec![proto::Event::Reconnecting {
                session_id,
                agent,
                attempt,
            }]
        }
        TurnEvent::AssistantTextDelta { agent, delta } => {
            vec![proto::Event::AssistantTextDelta {
                session_id,
                agent,
                delta,
            }]
        }
        TurnEvent::ReasoningDelta { agent, delta } => {
            vec![proto::Event::ReasoningDelta {
                session_id,
                agent,
                delta,
            }]
        }
        TurnEvent::AssistantText { agent, text } => {
            vec![proto::Event::AssistantText {
                session_id,
                agent,
                text,
            }]
        }
        TurnEvent::Notice { text } => {
            vec![proto::Event::Notice { session_id, text }]
        }
        TurnEvent::ToolStart {
            agent,
            call_id,
            tool,
            args,
        } => vec![proto::Event::ToolStart {
            session_id,
            agent,
            call_id,
            tool,
            args,
        }],
        TurnEvent::ToolEnd {
            agent,
            call_id,
            tool,
            output,
            truncated,
        } => vec![proto::Event::ToolEnd {
            session_id,
            agent,
            call_id,
            tool,
            output,
            truncated,
        }],
        TurnEvent::ToolError {
            agent,
            call_id,
            tool,
            error,
            kind,
        } => vec![proto::Event::ToolError {
            session_id,
            agent,
            call_id,
            tool,
            error,
            kind,
        }],
        TurnEvent::SubagentSpawned {
            parent,
            child,
            prompt,
        } => vec![proto::Event::SubagentSpawned {
            session_id,
            parent,
            child,
            prompt,
        }],
        TurnEvent::SubagentReport { agent, report } => {
            vec![proto::Event::SubagentReport {
                session_id,
                agent,
                report,
            }]
        }
        TurnEvent::Usage { agent, usage } => {
            vec![proto::Event::Usage {
                session_id,
                agent,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cached_input_tokens: usage.cached_input_tokens,
            }]
        }
        TurnEvent::AgentIdle => vec![proto::Event::AgentIdle { session_id }],
        TurnEvent::PrimarySwapped { name } => {
            vec![proto::Event::PrimarySwapped { session_id, name }]
        }
        TurnEvent::LlmModeChanged { mode } => {
            vec![proto::Event::LlmModeChanged { session_id, mode }]
        }
        // Engine→proto direction never produces this — the `question`
        // tool emits `proto::Event::InterruptRaised` directly through
        // the interrupt hub, and the TUI-client direction
        // (`proto_event_to_turn_event`) is the only place that
        // synthesizes the `TurnEvent` form. No wire event to forward.
        TurnEvent::InterruptRaised { .. } => vec![],
        TurnEvent::JobStarted {
            // The engine stamps the originating session; the worker's own
            // `session_id` is authoritative for the wire event and equals it.
            session_id: _,
            job_id,
            label,
            kind,
        } => vec![proto::Event::JobStarted {
            session_id,
            job_id,
            label,
            kind,
        }],
        TurnEvent::JobProgress { job_id } => {
            vec![proto::Event::JobProgress { session_id, job_id }]
        }
        TurnEvent::JobNote { job_id, text } => {
            vec![proto::Event::JobNote {
                session_id,
                job_id,
                text,
            }]
        }
        TurnEvent::JobCompleted {
            job_id,
            label,
            kind,
            failed,
        } => vec![proto::Event::JobCompleted {
            session_id,
            job_id,
            label,
            kind,
            failed,
        }],
        TurnEvent::ContextProjection {
            prunable_tokens,
            cache_cold,
        } => {
            vec![proto::Event::ContextProjection {
                session_id,
                prunable_tokens,
                cache_cold,
            }]
        }
        TurnEvent::Pruned {
            auto,
            bodies,
            tokens_saved,
            elided,
        } => vec![proto::Event::Pruned {
            session_id,
            auto,
            bodies,
            tokens_saved,
            elided,
        }],
        TurnEvent::CompactReady {
            new_session_id,
            handoff,
            seed_tool_count,
            seed_tool_tokens,
        } => vec![proto::Event::CompactReady {
            session_id,
            new_session_id,
            handoff,
            seed_tool_count,
            seed_tool_tokens,
        }],
        // The engine never emits `SandboxState` — the daemon's
        // `SetSandbox` handler broadcasts the wire event directly (it
        // carries `session_id`). This arm exists only for exhaustiveness.
        TurnEvent::SandboxState { enabled } => {
            vec![proto::Event::SandboxState {
                session_id,
                enabled,
            }]
        }
        // Caffeination is daemon-global, not a session event: the
        // `SetCaffeinate` handler / until-idle watcher broadcast
        // `proto::Event::CaffeinateState` over the global bus directly.
        // The engine never emits this; the arm is for exhaustiveness.
        TurnEvent::CaffeinateState { .. } => vec![],
        // The drain notice is daemon-global, broadcast by the daemon's
        // graceful-shutdown path directly (`server::request_shutdown`); the
        // engine never emits it. This arm is for exhaustiveness only.
        TurnEvent::DaemonDraining { .. } => vec![],
        // The plan-status chrome state is daemon-global, computed + broadcast
        // by the daemon's `broadcast_plan_status` directly (on attach,
        // interrupt raise/resolve, and the executor's `RefreshPlanStatus`); the
        // engine never emits it. This arm is for exhaustiveness only.
        TurnEvent::PlanStatusState { .. } => vec![],
    }
}

/// The primary agent a new session starts on: the user's configured
/// `defaultPrimaryAgent`, falling back to `Auto` (the conversational
/// front-door router) when unset. The registry uses this when it
/// constructs a fresh session row; the worker uses it for the approver's
/// prompt-attribution agent. Lives here so the constants and
/// event-translation helpers stay in one module.
pub(crate) fn initial_active_agent(cfg: &crate::config::extended::ExtendedConfig) -> &'static str {
    cfg.default_primary_agent.agent_name()
}

/// Resolve the root-frame primary for a session: its stored active agent
/// (so a resume restarts on whatever `Auto` handed off to, or a `/plan`
/// swap landed on), falling back to the configured default
/// ([`initial_active_agent`]) when unset/unknown. `Auto`, `Build`, and
/// `Plan` are the primary-mode agents; anything else degrades to the
/// default. Shared by [`spawn`] (the handle's initial chrome slot) and
/// [`run_worker`] (the agent it actually loads) so both agree.
fn resolve_root_agent(
    session_id: Uuid,
    db: &crate::db::Db,
    cfg: &crate::config::extended::ExtendedConfig,
) -> String {
    db.get_session(session_id)
        .ok()
        .flatten()
        .map(|row| row.active_agent)
        .filter(|name| name == "Auto" || name == "Plan" || name == "Build")
        .unwrap_or_else(|| initial_active_agent(cfg).to_string())
}

/// Persist a live `/llm-mode` switch to the layered config so a resume
/// keeps it (`prompts/llm-modes-defensive-normal.md`). Writes to the
/// highest-precedence existing `extended-config.json` on the discovered
/// path (the layer `load_for_cwd` would read), or — when none exists yet —
/// scaffolds one in the project `.cockpit/` so `/settings` + the config
/// file + `/llm-mode` all resolve to the same value. Round-trips through
/// [`ExtendedConfigDoc`] so unknown keys survive.
fn persist_llm_mode(
    project_root: &std::path::Path,
    mode: crate::config::extended::LlmMode,
) -> anyhow::Result<()> {
    use crate::config::dirs::discover_config_dirs;
    use crate::config::extended::ExtendedConfigDoc;
    let target = discover_config_dirs(project_root)
        .into_iter()
        .map(|d| d.path.join("extended-config.json"))
        .find(|p| p.exists())
        .unwrap_or_else(|| project_root.join(".cockpit").join("extended-config.json"));
    let mut doc = ExtendedConfigDoc::load(&target)?;
    let mut cfg = doc.config();
    cfg.llm_mode = mode;
    doc.write(&cfg)?;
    Ok(())
}

/// Env var the daemon sets at boot when launched with `--no-sandbox`
/// (sandboxing part 2). Read per session-spawn to apply the
/// highest-precedence "OFF for ALL sessions" rule. Set internally only
/// (Layer B style); never a user-facing surface.
pub const DAEMON_NO_SANDBOX_ENV: &str = "COCKPIT_DAEMON_NO_SANDBOX";

/// Whether the running daemon was launched with `--no-sandbox`.
fn daemon_no_sandbox() -> bool {
    std::env::var_os(DAEMON_NO_SANDBOX_ENV).is_some()
}

/// Resolve the new-session sandbox default from the live daemon flag.
fn resolve_sandbox_default(client_no_sandbox: bool) -> bool {
    resolve_sandbox_default_with(daemon_no_sandbox(), client_no_sandbox)
}

/// Pure precedence resolver (highest wins): daemon `--no-sandbox` →
/// client `--no-sandbox` → ON. Returns `true` when sandboxing should
/// start enabled. Factored out from [`resolve_sandbox_default`] so the
/// precedence can be unit-tested without touching process env.
fn resolve_sandbox_default_with(daemon_no_sandbox: bool, client_no_sandbox: bool) -> bool {
    if daemon_no_sandbox {
        false
    } else {
        !client_no_sandbox
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_default_precedence_daemon_wins() {
        // (a) daemon `--no-sandbox` → OFF regardless of the client flag.
        assert!(!resolve_sandbox_default_with(true, false));
        assert!(!resolve_sandbox_default_with(true, true));
    }

    #[test]
    fn sandbox_default_precedence_client_then_on() {
        // (b) no daemon flag, client `--no-sandbox` → OFF.
        assert!(!resolve_sandbox_default_with(false, true));
        // (c) neither flag → ON.
        assert!(resolve_sandbox_default_with(false, false));
    }
}

/// Resolve the per-session async-jobs concurrency cap (GOALS §22) from the
/// layered `extended-config.json` rooted at `project_root`, falling back
/// to the default when none is configured.
fn max_concurrent_jobs_for(project_root: &std::path::Path) -> usize {
    use crate::config::dirs::discover_config_dirs;
    use crate::config::extended::ExtendedConfigDoc;
    discover_config_dirs(project_root)
        .into_iter()
        .find_map(|d| ExtendedConfigDoc::load(&d.path.join("extended-config.json")).ok())
        .map(|d| d.config().jobs.max_concurrent)
        .unwrap_or(crate::engine::jobs::DEFAULT_MAX_CONCURRENT_JOBS)
}

/// Resolve the loop-guard threshold (GOALS §1/§12) from the layered
/// `extended-config.json` rooted at `project_root`, falling back to the
/// default (2 = fire on the first exact repeat) when none is configured.
fn loop_guard_threshold_for(project_root: &std::path::Path) -> u32 {
    use crate::config::dirs::discover_config_dirs;
    use crate::config::extended::ExtendedConfigDoc;
    discover_config_dirs(project_root)
        .into_iter()
        .find_map(|d| ExtendedConfigDoc::load(&d.path.join("extended-config.json")).ok())
        .map(|d| d.config().loop_guard.effective_threshold())
        .unwrap_or(crate::config::extended::MIN_LOOP_GUARD_THRESHOLD)
}
