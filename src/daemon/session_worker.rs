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
}

/// Work items a client can ask the worker to perform.
#[derive(Debug)]
pub enum SessionWork {
    UserMessage(String),
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
pub fn spawn(
    session: Arc<Session>,
    locks: Arc<LockManager>,
    redact: Arc<RedactionTable>,
    model: Arc<Model>,
    project_root: PathBuf,
) -> SessionWorkerHandle {
    let session_id = session.id;
    let (work_tx, work_rx) = mpsc::channel::<SessionWork>(WORK_QUEUE_CAPACITY);
    let (event_tx, _initial_rx) = broadcast::channel::<proto::Event>(EVENT_BROADCAST_CAPACITY);
    let live = Arc::new(LiveState::default());

    let handle = SessionWorkerHandle {
        session_id,
        project_root: project_root.clone(),
        active_agent_name: "orchestrator-build".into(),
        work_tx,
        event_tx: event_tx.clone(),
        live: live.clone(),
    };

    tokio::spawn(run_worker(
        session,
        locks,
        redact,
        model,
        project_root,
        work_rx,
        event_tx,
        live,
    ));

    handle
}

#[allow(clippy::too_many_arguments)]
async fn run_worker(
    session: Arc<Session>,
    locks: Arc<LockManager>,
    redact: Arc<RedactionTable>,
    model: Arc<Model>,
    project_root: PathBuf,
    mut work_rx: mpsc::Receiver<SessionWork>,
    event_tx: broadcast::Sender<proto::Event>,
    live: Arc<LiveState>,
) {
    let session_id = session.id;

    let spawn_args = SpawnArgs {
        model,
        params: ModelParams::default(),
        cwd: project_root.clone(),
        session_short_id: session.short_id.clone(),
    };
    let root = Arc::new(builtin::orchestrator_build(&spawn_args));

    let (driver_input_tx, driver_input_rx) = mpsc::channel::<String>(WORK_QUEUE_CAPACITY);
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
    ));
    driver.set_interrupt_hub(interrupts.clone());

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
            SessionWork::UserMessage(text) => {
                if let Err(e) = session.touch() {
                    tracing::warn!(error = %e, "session touch failed");
                }
                if driver_input_tx.send(text).await.is_err() {
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
                if let Err(e) = session.set_active_agent(&name) {
                    tracing::warn!(error = %e, "set_active_agent failed");
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
        // Engine→proto direction never produces this — the `question`
        // tool emits `proto::Event::InterruptRaised` directly through
        // the interrupt hub, and the TUI-client direction
        // (`proto_event_to_turn_event`) is the only place that
        // synthesizes the `TurnEvent` form. No wire event to forward.
        TurnEvent::InterruptRaised { .. } => vec![],
        TurnEvent::JobStarted {
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
    }
}

/// Marker the registry uses when it constructs (or resumes) a session
/// row before passing the work off to a worker. Lives here so the
/// constants and event-translation helpers stay in one module.
pub(crate) fn initial_active_agent() -> &'static str {
    "orchestrator-build"
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
