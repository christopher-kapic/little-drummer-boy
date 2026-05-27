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

use anyhow::Result;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use crate::daemon::proto;
use crate::engine::{Driver, TurnEvent};
use crate::engine::builtin::{self, SpawnArgs};
use crate::engine::model::{Model, ModelParams};
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

/// Handle one or more client tasks hold to drive a session. Cheap to
/// clone — both channels inside are reference-counted.
#[derive(Clone)]
pub struct SessionWorkerHandle {
    pub session_id: Uuid,
    pub project_root: PathBuf,
    pub active_agent_name: String,
    work_tx: mpsc::Sender<SessionWork>,
    event_tx: broadcast::Sender<proto::Event>,
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

    let handle = SessionWorkerHandle {
        session_id,
        project_root: project_root.clone(),
        active_agent_name: "orchestrator-build".into(),
        work_tx,
        event_tx: event_tx.clone(),
    };

    tokio::spawn(run_worker(
        session, locks, redact, model, project_root, work_rx, event_tx,
    ));

    handle
}

async fn run_worker(
    session: Arc<Session>,
    locks: Arc<LockManager>,
    redact: Arc<RedactionTable>,
    model: Arc<Model>,
    project_root: PathBuf,
    mut work_rx: mpsc::Receiver<SessionWork>,
    event_tx: broadcast::Sender<proto::Event>,
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
    let (engine_event_tx, mut engine_event_rx) = mpsc::channel::<TurnEvent>(WORK_QUEUE_CAPACITY);

    // Forward engine events → broadcast channel as proto::Event.
    let event_tx_for_forward = event_tx.clone();
    let forward = tokio::spawn(async move {
        while let Some(event) = engine_event_rx.recv().await {
            for ev in turn_event_to_proto(event, session_id) {
                // `send` returns `Err` only when there are no
                // subscribers — that's fine, nobody is listening.
                let _ = event_tx_for_forward.send(ev);
            }
        }
    });

    // Spawn the driver loop.
    let driver_handle = {
        let session = session.clone();
        let locks = locks.clone();
        let redact = redact.clone();
        let project_root = project_root.clone();
        tokio::spawn(async move {
            let mut driver = Driver::new(session, locks, redact, project_root, root);
            if let Err(e) = driver.run_main_loop(driver_input_rx, &engine_event_tx).await {
                tracing::error!(error = ?e, "driver loop terminated with error");
            }
        })
    };

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
                // v1: log only. Cancellation propagation through
                // `Model::complete` lands in a follow-up — it needs a
                // CancellationToken plumbed into rig's streaming
                // future. The wire path is in place so the TUI can
                // emit the request today; the engine acknowledges it
                // and the next inference will pick up any queued
                // messages.
                tracing::info!(session_id = %session_id, "Cancel requested (no-op in v1)");
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
                // Engine-side wakeup happens once the approval router
                // lands; for v1 the DB row update is sufficient to
                // record the response.
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
        } => vec![proto::Event::ToolError {
            session_id,
            agent,
            call_id,
            tool,
            error,
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
    }
}

/// Marker the registry uses when it constructs (or resumes) a session
/// row before passing the work off to a worker. Lives here so the
/// constants and event-translation helpers stay in one module.
pub(crate) fn initial_active_agent() -> &'static str {
    "orchestrator-build"
}
