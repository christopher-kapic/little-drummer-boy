//! Daemon server — accept loop + per-client task.
//!
//! Bound to the daemon's Unix socket. Each accepted connection spawns
//! a [`handle_client`] task that owns a [`ProtoStream`] and routes
//! requests to / forwards events from the [`SessionRegistry`].
//!
//! See `GOALS.md` §8 for the architecture and §8c for the wire-schema
//! contract that lets this layer ship without bikeshedding transport.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::config::dirs::discover_config_dirs;
use crate::config::extended::ExtendedConfig;
use crate::config::providers::{ConfigDoc, ProvidersConfig};
use crate::daemon::DaemonPaths;
use crate::daemon::proto::{
    self, Body, Envelope, ErrorCode, ErrorPayload, ProtoStream, Request, Response,
};
use crate::daemon::registry::SessionRegistry;
use crate::daemon::session_worker::{SessionWork, SessionWorkerHandle};
use crate::db::Db;
use crate::locks::LockManager;

/// Daemon-wide broadcast capacity for global (non-session) events such as
/// [`proto::Event::CaffeinateState`]. Generous — these are rare.
const GLOBAL_EVENT_CAPACITY: usize = 64;

/// Daemon-wide singletons. Held in an `Arc` so per-client tasks can
/// share without copying.
pub struct DaemonContext {
    pub db: Db,
    pub locks: Arc<LockManager>,
    pub registry: SessionRegistry,
    pub paths: DaemonPaths,
    pub started_at: Instant,
    /// Caffeination authority (`/caffeinate`, GOALS §1a chrome glyph).
    /// Holds the OS sleep assertion **in the daemon process** so it
    /// survives TUI-client exit, plus the on/off + until-idle state.
    pub caffeinate: Arc<crate::daemon::caffeinate::CaffeineController>,
    /// Daemon-global event bus. Unlike the per-session broadcast on each
    /// worker, every client task subscribes to this regardless of which
    /// (if any) session it's attached to — so a daemon-global event like
    /// [`proto::Event::CaffeinateState`] reaches *all* connected clients.
    global_events: broadcast::Sender<proto::Event>,
    /// Live count of connected clients. Each [`handle_client`] task
    /// increments on accept and decrements on exit. The ephemeral
    /// self-reaping watchdog (Layer C) watches the receiver side for
    /// "no clients" transitions; the persistent daemon ignores it.
    client_count: tokio::sync::watch::Sender<usize>,
    /// Daemon-wide graceful-shutdown gate
    /// (`daemon-graceful-drain-shutdown.md`). Shared with the registry
    /// (installed into worker models). New `SendUserMessage` requests are
    /// refused while it reports draining.
    shutdown: crate::daemon::shutdown::ShutdownSignal,
}

impl DaemonContext {
    pub fn new(db: Db, locks: Arc<LockManager>, paths: DaemonPaths) -> Self {
        // The daemon-wide graceful-shutdown gate
        // (`daemon-graceful-drain-shutdown.md`) — the central drain
        // authority. Built here and shared into the registry (which installs
        // it into every worker's model) so the inference-dispatch chokepoint,
        // the new-user-work gate, and teardown all read one state.
        let shutdown = crate::daemon::shutdown::ShutdownSignal::new();
        let registry = SessionRegistry::new(db.clone(), locks.clone(), shutdown.clone());
        let (client_count, _) = tokio::sync::watch::channel(0usize);
        let (global_events, _) = broadcast::channel(GLOBAL_EVENT_CAPACITY);
        Self {
            db,
            locks,
            registry,
            paths,
            started_at: Instant::now(),
            caffeinate: Arc::new(crate::daemon::caffeinate::CaffeineController::new()),
            global_events,
            client_count,
            shutdown,
        }
    }

    /// The daemon's graceful-shutdown gate. New-user-work rejection and the
    /// single drain path both read it.
    pub fn shutdown_signal(&self) -> &crate::daemon::shutdown::ShutdownSignal {
        &self.shutdown
    }

    /// Subscribe to the daemon-global event bus. Every client task holds
    /// one of these for its lifetime.
    pub fn subscribe_global(&self) -> broadcast::Receiver<proto::Event> {
        self.global_events.subscribe()
    }

    /// Broadcast a daemon-global event to all connected clients.
    pub fn broadcast_global(&self, event: proto::Event) {
        let _ = self.global_events.send(event);
    }

    /// Subscribe to the live connected-client count. Used by the
    /// ephemeral idle watchdog (Layer C).
    pub fn client_presence(&self) -> tokio::sync::watch::Receiver<usize> {
        self.client_count.subscribe()
    }

    /// RAII guard: bumps the connected-client count on construction and
    /// decrements it on drop, so the count stays correct on every exit
    /// path of a client task (clean EOF, decode error, send failure).
    fn track_client(self: &Arc<Self>) -> ClientGuard {
        self.client_count.send_modify(|n| *n += 1);
        ClientGuard { ctx: self.clone() }
    }
}

/// Decrements the daemon's connected-client count when a client task
/// ends, regardless of how it ends.
struct ClientGuard {
    ctx: Arc<DaemonContext>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.ctx
            .client_count
            .send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// Bootstrap the daemon: open the DB, build the lock manager, return
/// a ready-to-use context. Called from `daemon::run_foreground`.
pub fn boot(paths: DaemonPaths) -> Result<DaemonContext> {
    let db = Db::open_default().context("opening session DB")?;
    let locks = Arc::new(LockManager::from_db(db.clone()).context("loading lock state")?);
    // Drop autocomplete-tally rows that have aged out of the 30-day
    // window. Best-effort — a prune failure shouldn't block boot.
    let before = chrono::Utc::now().timestamp() - crate::db::usage_events::USAGE_WINDOW_SECS;
    if let Err(e) = db.prune_usage_events(before) {
        tracing::warn!(error = %e, "pruning usage_events on boot failed");
    }
    Ok(DaemonContext::new(db, locks, paths))
}

/// Bind the Unix socket and run the accept loop until the daemon's
/// graceful-shutdown gate leaves `Running`. Each accepted connection spawns
/// a detached client task. Breaking the loop hands control back to
/// [`crate::daemon::run_foreground_inner`], which drains the workers.
pub async fn run_accept_loop(ctx: Arc<DaemonContext>, listener: UnixListener) -> Result<()> {
    set_socket_perms(&ctx.paths.socket);

    let mut shutdown = ctx.shutdown.subscribe();
    // A drain may already have begun before we subscribed (begin_drain on a
    // very fast StopDaemon); break immediately if so.
    if ctx.shutdown.is_draining() {
        return Ok(());
    }

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                // Any transition out of `Running` (drain begun) closes the
                // accept loop; `changed()` only errs if the sender dropped,
                // which also means we should stop accepting.
                if changed.is_err() || ctx.shutdown.is_draining() {
                    tracing::info!("daemon: drain begun, closing accept loop");
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, ctx).await {
                                tracing::warn!(error = ?e, "client task ended with error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed; backing off");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Chmod the socket to 0600 so other users on the box can't connect.
/// Best-effort: any failure (Windows, weird filesystem) is logged but
/// doesn't kill the daemon — the principle is defense-in-depth, not
/// a hard gate.
fn set_socket_perms(socket: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(error = %e, "failed to chmod socket to 0600");
        }
    }
}

// ---- per-client state -----------------------------------------------------

struct ClientState {
    attached: Option<AttachedSession>,
}

struct AttachedSession {
    handle: SessionWorkerHandle,
    event_rx: broadcast::Receiver<proto::Event>,
    /// Held for the lifetime of the attachment when this client is
    /// interactive (can answer interrupts). Dropping it on detach /
    /// re-attach / disconnect decrements the worker's interactive-client
    /// count so the loop guard reverts to headless behavior. `None` for a
    /// non-interactive attach (e.g. `cockpit run`'s event pump).
    _interactive_guard: Option<crate::daemon::session_worker::InteractiveClientGuard>,
}

async fn handle_client(stream: UnixStream, ctx: Arc<DaemonContext>) -> Result<()> {
    // Count this client for the lifetime of the task. The guard
    // decrements on every return below (Layer C presence tracking).
    let _client_guard = ctx.track_client();
    let mut proto = ProtoStream::new(stream);

    // Emit a "hello" envelope immediately so cheap probes
    // (`probe_blocking`, third-party reachability checks) can confirm
    // the daemon is alive without doing a full proto handshake. The
    // envelope is a self-contained `DaemonStatus` response with
    // `id = Nil`, which `DaemonClient` ignores (no pending request
    // matches it).
    let hello = Envelope::response(
        Uuid::nil(),
        Response::DaemonStatus {
            pid: std::process::id(),
            uptime_secs: ctx.started_at.elapsed().as_secs(),
            active_sessions: ctx.registry.active_session_ids().len() as u32,
            socket_path: ctx.paths.socket.display().to_string(),
        },
    );
    if proto.send(&hello).await.is_err() {
        return Ok(());
    }

    let mut state = ClientState { attached: None };

    // Daemon-global events (caffeinate, …) reach every client regardless
    // of attachment, so this receiver lives for the whole client task.
    let mut global_rx = ctx.subscribe_global();

    // On connect, sync the client's caffeinate glyph to the daemon's
    // current state (a TUI that attaches while caffeination is already on
    // must show ☕ immediately). Fire-and-forget; a send failure just
    // means the client went away.
    {
        let snap = ctx.caffeinate.snapshot();
        let _ = proto
            .send(&Envelope::event(proto::Event::CaffeinateState {
                active: snap.active,
                lid_close_guaranteed: false,
                message: None,
            }))
            .await;
    }

    loop {
        // The select! pulls from whichever side of the socket has work.
        // We have to expand `recv_event` inline because Future<Output=
        // …> from `broadcast::Receiver::recv` borrows the receiver.
        let inbound = async {
            match proto.recv().await {
                Ok(Some(env)) => Some(Ok(env)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            }
        };

        // If there's an attached session, listen for its events too.
        // When there isn't, the `event_branch` future is `pending`.
        let event_branch = async {
            match state.attached.as_mut() {
                Some(att) => Some(att.event_rx.recv().await),
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            biased;
            global = global_rx.recv() => {
                match global {
                    Ok(ev) => {
                        if let Err(e) = proto.send(&Envelope::event(ev)).await {
                            tracing::debug!(error = ?e, "client disconnected during global event send");
                            return Ok(());
                        }
                    }
                    // A lagging global bus is non-fatal: caffeinate state
                    // is re-synced on the next change + at attach time.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "client global event stream lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // The daemon is tearing down; let the socket close.
                    }
                }
            }
            event = event_branch => {
                match event {
                    Some(Ok(ev)) => {
                        if let Err(e) = proto.send(&Envelope::event(ev)).await {
                            tracing::debug!(error = ?e, "client disconnected during event send");
                            return Ok(());
                        }
                    }
                    Some(Err(broadcast::error::RecvError::Lagged(n))) => {
                        tracing::warn!(missed = n, "client event stream lagged; reattach to resync");
                        // Per design, lagging clients re-attach. We
                        // emit a synthetic error so the TUI surfaces it.
                        let _ = proto
                            .send(&Envelope::error(
                                None,
                                ErrorPayload {
                                    code: ErrorCode::Internal,
                                    message: format!("event stream lagged by {n}; re-attach"),
                                },
                            ))
                            .await;
                    }
                    Some(Err(broadcast::error::RecvError::Closed)) => {
                        // The session worker exited. Detach so the
                        // client can attach to a different session
                        // without churning.
                        state.attached = None;
                    }
                    None => unreachable!("event_branch is pending when not attached"),
                }
            }
            recv = inbound => {
                match recv {
                    None => return Ok(()), // clean EOF
                    Some(Err(e)) => {
                        tracing::debug!(error = ?e, "envelope decode failed; closing client");
                        return Ok(());
                    }
                    Some(Ok(env)) => {
                        handle_envelope(env, &mut state, &ctx, &mut proto).await?;
                    }
                }
            }
        }
    }
}

async fn handle_envelope(
    env: Envelope,
    state: &mut ClientState,
    ctx: &Arc<DaemonContext>,
    proto: &mut ProtoStream<UnixStream>,
) -> Result<()> {
    match env.body {
        Body::Request { id, request } => {
            let result = handle_request(request, state, ctx).await;
            let envelope = match result {
                Ok(response) => Envelope::response(id, response),
                Err(err) => Envelope::error(Some(id), err),
            };
            let _ = proto.send(&envelope).await;
        }
        Body::Response { id, .. } => {
            tracing::warn!(id = %id, "client sent a response envelope; ignoring");
        }
        Body::Event { event } => {
            tracing::warn!(?event, "client sent an event envelope; ignoring");
        }
        Body::Error { id, error } => {
            tracing::warn!(?id, ?error, "client sent an error envelope; ignoring");
        }
    }
    Ok(())
}

async fn handle_request(
    request: Request,
    state: &mut ClientState,
    ctx: &Arc<DaemonContext>,
) -> std::result::Result<Response, ErrorPayload> {
    match request {
        Request::Attach {
            session_id,
            project_root,
            no_sandbox,
            interactive,
        } => attach(
            state,
            ctx,
            session_id,
            project_root,
            no_sandbox,
            interactive,
        ),

        Request::SendUserMessage { text, images } => {
            // New-user-work gate (`daemon-graceful-drain-shutdown.md`): once
            // a drain begins, reject new turns with a short notice rather
            // than silently dropping or queuing them. In-flight turns keep
            // running; this only stops *new* work from starting.
            if ctx.shutdown.is_draining() {
                return Err(ErrorPayload {
                    code: ErrorCode::Shutdown,
                    message: "daemon is shutting down; not accepting new messages".into(),
                });
            }
            let att = require_attached(state)?;
            att.handle
                .send_work(SessionWork::UserMessage(
                    crate::engine::message::UserSubmission { text, images },
                ))
                .await
                .map_err(internal)?;
            Ok(Response::Ack)
        }

        Request::CancelTurn => {
            let att = require_attached(state)?;
            att.handle
                .send_work(SessionWork::Cancel)
                .await
                .map_err(internal)?;
            Ok(Response::Ack)
        }

        Request::ResolveInterrupt {
            interrupt_id,
            response,
        } => {
            let att = require_attached(state)?;
            att.handle
                .send_work(SessionWork::ResolveInterrupt {
                    interrupt_id,
                    response,
                })
                .await
                .map_err(internal)?;
            Ok(Response::Ack)
        }

        Request::ListSessions {
            project_id,
            parent_session_id,
        } => list_sessions(ctx, project_id, parent_session_id),

        Request::SessionLiveStatus { session_ids } => {
            let statuses = session_ids
                .into_iter()
                .filter_map(|id| {
                    ctx.registry
                        .live_status(id)
                        .map(|(has_active_jobs, processing)| proto::LiveStatus {
                            session_id: id,
                            has_active_jobs,
                            processing,
                        })
                })
                .collect();
            Ok(Response::SessionLiveStatus { statuses })
        }

        Request::ArchiveSession {
            session_id,
            cascade,
        } => archive_session(ctx, session_id, cascade).await,

        Request::UnarchiveSession { session_id } => unarchive_session(ctx, session_id),

        Request::ForkSession {
            parent_session_id,
            fork_point_turn_id,
        } => fork_session(ctx, parent_session_id, fork_point_turn_id),

        Request::RenameSession { session_id, title } => rename_session(ctx, session_id, &title),

        Request::DeleteSession {
            session_id,
            cascade,
        } => delete_session(ctx, session_id, cascade).await,

        Request::GetConfig => {
            // The /config TUI payload lands in P3. For now stub it
            // with an empty layer list and the resolved providers
            // config of the attached session (or the daemon's cwd).
            Err(not_implemented("GetConfig (lands with /config TUI)"))
        }

        Request::ListSkills { project_root } => {
            // Resolve the configured scan dirs from the client's cwd so
            // per-project skills config applies, then run the shared
            // discovery used by the `skill` tool and auto-select path.
            let cwd = Path::new(&project_root);
            let extended = crate::config::extended::load_for_cwd(cwd);
            let skills = crate::skills::discover(cwd, &extended.skills).map_err(internal)?;
            let skills = skills
                .into_iter()
                .map(|s| proto::SkillSummary {
                    name: s.frontmatter.name,
                    description: s.frontmatter.description,
                    source: s.source.display().to_string(),
                })
                .collect();
            Ok(Response::Skills { skills })
        }
        Request::ListPlans => list_plans(ctx),
        Request::PlanDetail { plan_id } => plan_detail(ctx, plan_id),

        Request::ListAgents => Err(not_implemented("ListAgents")),
        Request::ListModels { .. } => Err(not_implemented("ListModels")),

        Request::SetActiveModel { provider, model } => {
            let att = require_attached(state)?;
            att.handle
                .send_work(SessionWork::SetActiveModel { provider, model })
                .await
                .map_err(internal)?;
            Ok(Response::Ack)
        }

        Request::SetAgent { name } => {
            let att = require_attached(state)?;
            att.handle
                .send_work(SessionWork::SetAgent { name })
                .await
                .map_err(internal)?;
            Ok(Response::Ack)
        }

        Request::SetCaffeinate { mode } => set_caffeinate(state, ctx, mode),

        Request::CancelJob { job_id } => {
            let att = require_attached(state)?;
            att.handle
                .send_work(SessionWork::CancelJob { job_id })
                .await
                .map_err(internal)?;
            Ok(Response::Ack)
        }

        Request::SetSandbox { enabled } => {
            // Sandboxing part 2: flip the session's sandbox flag directly
            // (it's a shared atomic) and reply with the resulting state.
            // The handle also broadcasts a `SandboxState` event so every
            // attached client stays in sync.
            let att = require_attached(state)?;
            let new = att.handle.set_sandbox(enabled);
            Ok(Response::SandboxState { enabled: new })
        }

        Request::Prune => {
            let att = require_attached(state)?;
            att.handle
                .send_work(SessionWork::Prune)
                .await
                .map_err(internal)?;
            Ok(Response::Ack)
        }

        Request::Compact => {
            let att = require_attached(state)?;
            att.handle
                .send_work(SessionWork::Compact)
                .await
                .map_err(internal)?;
            Ok(Response::Ack)
        }

        Request::Pin { text } => {
            let att = require_attached(state)?;
            att.handle
                .send_work(SessionWork::Pin { text })
                .await
                .map_err(internal)?;
            Ok(Response::Ack)
        }

        Request::DaemonStatus => Ok(Response::DaemonStatus {
            pid: std::process::id(),
            uptime_secs: ctx.started_at.elapsed().as_secs(),
            active_sessions: ctx.registry.active_session_ids().len() as u32,
            socket_path: ctx.paths.socket.display().to_string(),
        }),

        Request::RefreshEnv { vars } => {
            // SAFETY: `std::env::set_var` mutates process-global state.
            // The daemon is multi-threaded but only the model-call /
            // header-resolution paths read these vars, and a stale read
            // mid-overwrite is bounded — at worst the next inference
            // call uses the pre-update value, which is the same situation
            // we'd be in had the refresh never happened.
            for (k, v) in vars {
                unsafe {
                    std::env::set_var(&k, &v);
                }
            }
            Ok(Response::Ack)
        }

        Request::RecordUsage {
            kind,
            key,
            project_id,
        } => {
            // Global tally — no attached session required.
            ctx.db
                .record_usage(
                    kind.as_str(),
                    &key,
                    project_id.as_deref(),
                    chrono::Utc::now().timestamp(),
                )
                .map_err(internal)?;
            Ok(Response::Ack)
        }

        Request::GetUsageCounts { project_id } => {
            let since = chrono::Utc::now().timestamp() - crate::db::usage_events::USAGE_WINDOW_SECS;
            let models = ctx
                .db
                .usage_counts("model", None, since)
                .map_err(internal)?;
            let slash = ctx
                .db
                .usage_counts("slash", None, since)
                .map_err(internal)?;
            // Tags are per-project; with no project there's nothing to
            // scope to, so the map is empty rather than a global mash-up.
            let tags = match project_id.as_deref() {
                Some(pid) => ctx
                    .db
                    .usage_counts("tag", Some(pid), since)
                    .map_err(internal)?,
                None => std::collections::HashMap::new(),
            };
            Ok(Response::UsageCounts {
                models,
                slash,
                tags,
            })
        }

        Request::GuidanceEstimate {
            project_root,
            provider,
            model,
        } => {
            // Resolve the single guidance file the engine would load and
            // estimate, with the calibrated tokenizer for the active model
            // (cl100k fallback when uncalibrated), two figures: the
            // guidance-file body (the `… in <file>` label) and the full
            // composed system prompt (the fresh-context baseline the
            // running estimate folds in). No session exists yet at the
            // fresh-chat indicator, so the system prompt omits the
            // `Session:` line — matching what the engine then sends.
            let cwd = Path::new(&project_root);
            let (strategy, scale) = ctx.db.resolve_tokenizer(
                provider.as_deref().unwrap_or(""),
                model.as_deref().unwrap_or(""),
            );
            let system_prompt = crate::engine::builtin::default_chat_system_prompt(cwd, "");
            let system_tokens = crate::tokens::scaled_estimate(&system_prompt, strategy, scale);
            match crate::engine::builtin::load_agent_guidance(cwd) {
                Some((path, body)) => {
                    let tokens = crate::tokens::scaled_estimate(&body, strategy, scale);
                    let file = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    Ok(Response::GuidanceEstimate {
                        file,
                        tokens,
                        system_tokens,
                    })
                }
                None => Ok(Response::GuidanceEstimate {
                    file: None,
                    tokens: 0,
                    system_tokens,
                }),
            }
        }

        Request::StopDaemon => {
            tracing::info!("StopDaemon requested via client");
            // Route through the single graceful-shutdown path
            // (`daemon-graceful-drain-shutdown.md`): the same begin-drain /
            // shorten-to-force transition SIGINT/SIGTERM and the ephemeral
            // teardown use. A second `StopDaemon` while already draining
            // shortens to an immediate force-exit instead of starting a
            // second drain or resetting the deadline.
            request_shutdown(ctx);
            Ok(Response::Ack)
        }
    }
}

// ---- shutdown -------------------------------------------------------------

/// The single entry point every stop trigger (SIGINT/SIGTERM, explicit
/// `StopDaemon`, the ephemeral last-client/owner-exit teardown) routes
/// through (`daemon-graceful-drain-shutdown.md`).
///
/// First call begins the drain: it broadcasts the `DaemonDraining { forced:
/// false }` notice (TUIs show "finishing in-flight work, shutting down…"
/// and start refusing new input) and flips the central gate so the
/// inference-dispatch chokepoint refuses new provider requests. A *second*
/// call while already draining **shortens** to an immediate force-exit —
/// it promotes the gate to `Forced` and broadcasts `DaemonDraining { forced:
/// true }`. Both transitions are monotonic/idempotent, so a redundant
/// trigger never starts a second drain, resets the deadline, or deadlocks.
pub fn request_shutdown(ctx: &Arc<DaemonContext>) {
    if ctx.shutdown.begin_drain() {
        tracing::info!("daemon: graceful drain begun");
        ctx.broadcast_global(proto::Event::DaemonDraining { forced: false });
    } else if !ctx.shutdown.is_forced() {
        // Already draining and a second trigger arrived: shorten to force.
        ctx.shutdown.force();
        tracing::warn!("daemon: second stop request during drain; forcing exit");
        ctx.broadcast_global(proto::Event::DaemonDraining { forced: true });
    }
}

// ---- helpers --------------------------------------------------------------

/// Apply a `/caffeinate` request: resolve the display-awake scope from
/// config, drive the daemon-held [`CaffeineController`], broadcast the
/// resulting state to **all** clients, and (for `until-idle`) arm the
/// daemon's auto-off watcher. The OS assertion lives in this process so it
/// survives the requesting client's exit.
fn set_caffeinate(
    state: &ClientState,
    ctx: &Arc<DaemonContext>,
    mode: crate::daemon::caffeinate::CaffeinateMode,
) -> std::result::Result<Response, ErrorPayload> {
    use crate::daemon::caffeinate::InhibitScope;

    // Display-awake is a config setting; resolve it from the attached
    // session's project root when available, else the daemon's cwd.
    let cfg_root = state
        .attached
        .as_ref()
        .map(|att| att.handle.project_root.clone())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let scope: InhibitScope = match load_configs(&cfg_root) {
        Ok((_, extended)) => extended.tui.sleep_scope().into(),
        // Config read failure must not block caffeination: fall back to
        // the safe default (system-only, display free to sleep).
        Err(_) => InhibitScope {
            keep_display_on: false,
        },
    };

    match ctx.caffeinate.apply(mode, scope) {
        Ok(applied) => {
            // Broadcast to every client so the ☕ glyph stays in sync.
            ctx.broadcast_global(proto::Event::CaffeinateState {
                active: applied.state.active,
                lid_close_guaranteed: applied.lid_close_guaranteed,
                message: None,
            });
            // Arm the daemon-owned until-idle watcher: it polls "is any
            // agent running?" and auto-offs once none are.
            if applied.state.until_idle {
                spawn_until_idle_watcher(ctx.clone());
            }
            Ok(Response::CaffeinateState {
                active: applied.state.active,
                lid_close_guaranteed: applied.lid_close_guaranteed,
                message: applied.message,
            })
        }
        // Missing-mechanism / acquire failure: report it so the TUI shows
        // an honest, actionable toast (never silent). State stays off.
        Err(message) => Ok(Response::CaffeinateState {
            active: false,
            lid_close_guaranteed: false,
            message,
        }),
    }
}

/// Poll interval for the until-idle auto-off watcher. Short enough that
/// the machine doesn't stay awake long after the last agent finishes,
/// long enough to be negligible overhead.
const UNTIL_IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// Spawn the daemon's `until-idle` auto-off watcher. The daemon owns the
/// session workers / `JobAuthority`, so it is the authority for "is an
/// agent running anywhere?". The watcher polls that and, once no agent is
/// running, releases the assertion and broadcasts the off-state to all
/// clients. It exits if the mode is no longer until-idle (a later
/// `on`/`off`/`toggle` superseded it) so a fresh `until-idle` can re-arm
/// without stacking watchers racing each other.
fn spawn_until_idle_watcher(ctx: Arc<DaemonContext>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(UNTIL_IDLE_POLL).await;
            // Superseded (explicit on/off, or already auto-offed): stop.
            if !ctx.caffeinate.is_until_idle() {
                return;
            }
            let running = ctx.registry.any_agent_running();
            if let Some(applied) = ctx.caffeinate.idle_check(running) {
                ctx.broadcast_global(proto::Event::CaffeinateState {
                    active: applied.state.active,
                    lid_close_guaranteed: applied.lid_close_guaranteed,
                    message: None,
                });
                return;
            }
        }
    });
}

fn attach(
    state: &mut ClientState,
    ctx: &DaemonContext,
    session_id: Option<Uuid>,
    project_root: Option<String>,
    no_sandbox: bool,
    interactive: bool,
) -> std::result::Result<Response, ErrorPayload> {
    // The client's `--no-sandbox` only governs sessions it *creates*
    // (sandboxing part 2). On resume of an existing session id the session
    // keeps its own runtime state, so the flag is ignored there.
    let client_no_sandbox = no_sandbox && session_id.is_none();
    let project_root = project_root.map(PathBuf::from);

    let cfg_root = match (session_id, &project_root) {
        (Some(id), _) => match ctx.db.get_session(id) {
            Ok(Some(row)) => Some(PathBuf::from(row.project_root)),
            Ok(None) => {
                return Err(ErrorPayload {
                    code: ErrorCode::UnknownSession,
                    message: format!("unknown session {id}"),
                });
            }
            Err(e) => return Err(internal(e)),
        },
        (None, Some(root)) => Some(root.clone()),
        (None, None) => {
            return Err(ErrorPayload {
                code: ErrorCode::BadRequest,
                message: "attach requires session_id or project_root".into(),
            });
        }
    };

    let cfg_root = cfg_root.expect("resolved above");
    let (providers_cfg, extended_cfg) = load_configs(&cfg_root).map_err(internal)?;

    let handle = ctx
        .registry
        .attach(
            session_id,
            project_root,
            &providers_cfg,
            &extended_cfg,
            client_no_sandbox,
        )
        .map_err(internal)?;

    // Replace any prior attachment. Register this client with the worker's
    // interactive-client counter when it can answer interrupts (the loop
    // guard reads that count for headless detection). Building the guard
    // before the old `state.attached` is replaced means a re-attach by the
    // same client transiently holds two guards, never zero — the count
    // can't briefly read headless mid-swap.
    let event_rx = handle.subscribe();
    let interactive_guard = if interactive {
        Some(handle.register_interactive_client())
    } else {
        None
    };
    let session_id = handle.session_id;

    // Read/unread marker (GOALS §17f): the session just became active for
    // this client, so everything the agent produced up to now is "seen."
    // Best-effort — a marker write failure must not block the attach.
    if let Err(e) = ctx.db.mark_session_viewed(session_id) {
        tracing::warn!(error = %e, %session_id, "mark_session_viewed failed");
    }

    let project_root = handle.project_root.to_string_lossy().into_owned();
    let active_agent = handle.active_agent_name.clone();
    // Source identity from the live session, not a DB read: a freshly
    // created session is deferred-persistence (session-id-display-and-lazy-
    // persist) and has no `sessions` row yet, so `get_session` would miss.
    let project_id = handle.project_id();
    let short_id = handle.short_id();

    state.attached = Some(AttachedSession {
        handle,
        event_rx,
        _interactive_guard: interactive_guard,
    });

    // History snapshot of past tool calls / assistant turns for the
    // attached session, projected into the wire `HistoryEntry` shape.
    let history = match ctx.db.list_tool_calls_for_session(session_id) {
        Ok(rows) => rows
            .into_iter()
            .map(|ev| proto::HistoryEntry::ToolCall {
                agent: ev.agent,
                call_id: ev.call_id,
                tool: ev.tool,
                original_input: ev.original_input_json,
                wire_input: ev.wire_input_json,
                recovery_kind: None, // §14: filled once we read recovery back
                recovery_stage: None,
                output: ev.output,
                hard_fail: ev.hard_fail,
                truncated: ev.truncated,
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    Ok(Response::Attached {
        session_id,
        short_id,
        project_root,
        project_id,
        active_agent,
        history,
    })
}

fn list_sessions(
    ctx: &DaemonContext,
    project_id: Option<String>,
    parent_session_id: Option<Uuid>,
) -> std::result::Result<Response, ErrorPayload> {
    // The row assembly (level selection, fork counts, read/unread inputs)
    // lives in one place — `Db::list_session_summaries` — so the daemon
    // and the TUI's daemonless direct-DB fallback produce the same shape
    // (ordering / scoping / fork-grouping). Live status is layered on by
    // the client via `SessionLiveStatus`, not here.
    let sessions = ctx
        .db
        .list_session_summaries(project_id.as_deref(), parent_session_id, 100)
        .map_err(internal)?;
    Ok(Response::Sessions { sessions })
}

/// List every plan (active first, newest within a group) for the
/// read-only `/plans` browser. Plans are global — no project scope.
fn list_plans(ctx: &DaemonContext) -> std::result::Result<Response, ErrorPayload> {
    let summaries = ctx.db.list_all_plan_summaries().map_err(internal)?;
    let plans = summaries.into_iter().map(plan_summary_wire).collect();
    Ok(Response::Plans { plans })
}

/// Full detail of one plan: its steps with dependency prerequisites
/// (resolved to titles), per-step status, and each step's tests. Reads the
/// edge list once and indexes it so each step's `depends_on` is a lookup.
fn plan_detail(ctx: &DaemonContext, plan_id: Uuid) -> std::result::Result<Response, ErrorPayload> {
    let plan = match ctx.db.plan_by_id(plan_id).map_err(internal)? {
        Some(p) => p,
        None => {
            return Err(ErrorPayload {
                code: ErrorCode::BadRequest,
                message: format!("unknown plan {plan_id}"),
            });
        }
    };
    let step_count = ctx.db.list_steps(plan_id).map_err(internal)?.len() as i64;
    let summary = plan_summary_wire(crate::db::plans::PlanSummary { plan, step_count });

    let steps = ctx.db.list_steps(plan_id).map_err(internal)?;
    let edges = ctx.db.list_dependencies(plan_id).map_err(internal)?;
    // `id → title` so dependency targets render as titles, not uuids.
    let title_by_id: std::collections::HashMap<Uuid, String> =
        steps.iter().map(|s| (s.id, s.title.clone())).collect();

    let mut wire_steps = Vec::with_capacity(steps.len());
    for step in &steps {
        // `from depends on to`: this step's prerequisites are the `to`
        // endpoints of edges whose `from` is this step.
        let depends_on = edges
            .iter()
            .filter(|(from, _)| *from == step.id)
            .filter_map(|(_, to)| title_by_id.get(to).cloned())
            .collect();
        let tests = ctx
            .db
            .list_step_tests(step.id)
            .map_err(internal)?
            .into_iter()
            .map(|t| proto::PlanTestWire {
                command: t.command,
                phase: t.phase.as_str().to_string(),
                concurrency: match t.concurrency {
                    crate::db::plans::TestConcurrency::Parallel => "parallel".to_string(),
                    crate::db::plans::TestConcurrency::Exclusive { resource_key } => {
                        format!("exclusive: {resource_key}")
                    }
                },
            })
            .collect();
        wire_steps.push(proto::PlanStepWire {
            step_id: step.id,
            title: step.title.clone(),
            status: step.status.as_str().to_string(),
            depends_on,
            tests,
        });
    }
    Ok(Response::PlanDetail {
        plan: summary,
        steps: wire_steps,
    })
}

/// Flatten a [`crate::db::plans::PlanSummary`] onto the wire shape.
fn plan_summary_wire(s: crate::db::plans::PlanSummary) -> proto::PlanSummaryWire {
    proto::PlanSummaryWire {
        plan_id: s.plan.id,
        slug: s.plan.slug,
        title: s.plan.title,
        description: s.plan.description,
        status: s.plan.status.as_str().to_string(),
        base_branch: s.plan.base_branch,
        target_branch: s.plan.target_branch,
        step_count: s.step_count,
        created_at: s.plan.created_at,
    }
}

fn fork_session(
    ctx: &DaemonContext,
    parent_session_id: Uuid,
    fork_point_turn_id: Option<String>,
) -> std::result::Result<Response, ErrorPayload> {
    // Guard rail: refuse forks of unknown parents with the typed
    // `UnknownSession` code so the TUI can surface a friendlier error
    // than a generic internal failure.
    match ctx.db.get_session(parent_session_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(ErrorPayload {
                code: ErrorCode::UnknownSession,
                message: format!("unknown parent session {parent_session_id}"),
            });
        }
        Err(e) => return Err(internal(e)),
    }
    let row = ctx
        .db
        .create_fork(parent_session_id, fork_point_turn_id.clone())
        .map_err(internal)?;
    Ok(Response::Forked {
        session_id: row.session_id,
        short_id: row.short_id.unwrap_or_default(),
        parent_session_id,
        fork_point_turn_id,
    })
}

fn rename_session(
    ctx: &DaemonContext,
    session_id: Uuid,
    title: &str,
) -> std::result::Result<Response, ErrorPayload> {
    match ctx.db.get_session(session_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(ErrorPayload {
                code: ErrorCode::UnknownSession,
                message: format!("unknown session {session_id}"),
            });
        }
        Err(e) => return Err(internal(e)),
    }
    ctx.db.rename_session(session_id, title).map_err(internal)?;
    Ok(Response::Ack)
}

async fn delete_session(
    ctx: &DaemonContext,
    session_id: Uuid,
    cascade: bool,
) -> std::result::Result<Response, ErrorPayload> {
    match ctx.db.get_session(session_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(ErrorPayload {
                code: ErrorCode::UnknownSession,
                message: format!("unknown session {session_id}"),
            });
        }
        Err(e) => return Err(internal(e)),
    }
    // Don't delete out from under a running worker (GOALS §17h): stop any
    // live workers in the affected subtree first — that cancels their
    // async jobs and ends the current turn cleanly.
    interrupt_subtree(ctx, session_id, cascade).await;
    ctx.db
        .delete_session(session_id, cascade)
        .map_err(internal)?;
    Ok(Response::Ack)
}

async fn archive_session(
    ctx: &DaemonContext,
    session_id: Uuid,
    cascade: bool,
) -> std::result::Result<Response, ErrorPayload> {
    match ctx.db.get_session(session_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(ErrorPayload {
                code: ErrorCode::UnknownSession,
                message: format!("unknown session {session_id}"),
            });
        }
        Err(e) => return Err(internal(e)),
    }
    // Same interrupt-first rule as delete: don't archive a session while
    // its worker is live.
    interrupt_subtree(ctx, session_id, cascade).await;
    ctx.db
        .archive_session(session_id, cascade)
        .map_err(internal)?;
    Ok(Response::Ack)
}

/// Stop any live worker for `root` (and, when `cascade`, its whole fork
/// subtree) before an archive/delete. Best-effort over the candidate ids
/// the daemon currently has active workers for — there is no DB walk
/// here because only sessions with a live worker need interrupting, and
/// the registry already knows those.
async fn interrupt_subtree(ctx: &DaemonContext, root: Uuid, cascade: bool) {
    if !cascade {
        ctx.registry.interrupt_and_stop(root).await;
        return;
    }
    // Cascade: interrupt every active session whose row sits in the
    // subtree rooted at `root`. We intersect the daemon's live worker set
    // with the DB subtree so we only walk what's actually running.
    let active = ctx.registry.active_session_ids();
    for id in active {
        if ctx.db.is_in_subtree(root, id).unwrap_or(false) {
            ctx.registry.interrupt_and_stop(id).await;
        }
    }
}

fn unarchive_session(
    ctx: &DaemonContext,
    session_id: Uuid,
) -> std::result::Result<Response, ErrorPayload> {
    match ctx.db.get_session(session_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(ErrorPayload {
                code: ErrorCode::UnknownSession,
                message: format!("unknown session {session_id}"),
            });
        }
        Err(e) => return Err(internal(e)),
    }
    ctx.db.unarchive_session(session_id).map_err(internal)?;
    Ok(Response::Ack)
}

fn require_attached(state: &ClientState) -> std::result::Result<&AttachedSession, ErrorPayload> {
    state.attached.as_ref().ok_or_else(|| ErrorPayload {
        code: ErrorCode::NotAttached,
        message: "client has not attached to a session".into(),
    })
}

fn internal<E: std::fmt::Display>(err: E) -> ErrorPayload {
    ErrorPayload {
        code: ErrorCode::Internal,
        // `{:#}` walks the full anyhow context chain (e.g. `resolving
        // model: provider ...: ...`) rather than printing only the
        // outermost context, so daemon-surfaced errors are legible
        // instead of an opaque `internal: resolving model`.
        message: format!("{err:#}"),
    }
}

fn not_implemented(what: &str) -> ErrorPayload {
    ErrorPayload {
        code: ErrorCode::Internal,
        // `{:#}` for consistency with `internal()`; `what` is a plain
        // slug here, so the alternate form is identical, but keeping the
        // same form means a future error-typed arg would print its chain.
        message: format!("{what:#} not yet implemented in v1"),
    }
}

/// Walk the layered-config discovery from `cwd` and merge the first
/// `config.json` + `extended-config.json` found into the typed
/// configs. This mirrors `tui::agent_runner::load_providers` /
/// `load_extended` so the in-process and daemon-mediated paths see
/// identical config behavior during the transition.
fn load_configs(cwd: &Path) -> Result<(ProvidersConfig, ExtendedConfig)> {
    let dirs = discover_config_dirs(cwd);
    let mut providers = ProvidersConfig::default();
    let mut extended = ExtendedConfig::default();

    if let Some(dir) = dirs.first() {
        let providers_path = dir.path.join("config.json");
        if providers_path.exists() {
            providers = ConfigDoc::load(&providers_path)
                .context("loading providers config")?
                .providers();
        }
    }
    for dir in &dirs {
        let extended_path = dir.path.join("extended-config.json");
        if let Ok(bytes) = std::fs::read(&extended_path) {
            if let Ok(cfg) = serde_json::from_slice::<ExtendedConfig>(&bytes) {
                extended = cfg;
                break;
            }
        }
    }
    Ok((providers, extended))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::shutdown::ShutdownPhase;

    fn test_ctx() -> Arc<DaemonContext> {
        let db = Db::open_in_memory().expect("in-memory db");
        let locks = Arc::new(LockManager::from_db(db.clone()).expect("locks"));
        let paths = DaemonPaths {
            socket: std::path::PathBuf::from("/tmp/cockpit-test.sock"),
            pid_file: std::path::PathBuf::from("/tmp/cockpit-test.pid"),
            ephemeral: true,
        };
        Arc::new(DaemonContext::new(db, locks, paths))
    }

    /// The single graceful-shutdown path
    /// (`daemon-graceful-drain-shutdown.md`): the first `request_shutdown`
    /// begins the drain and broadcasts the (non-forced) notice; a **second**
    /// one while still draining **shortens** to force and broadcasts the
    /// forced notice — never a second drain or a reset deadline.
    #[tokio::test]
    async fn second_stop_request_shortens_to_force() {
        let ctx = test_ctx();
        let mut events = ctx.subscribe_global();
        assert_eq!(ctx.shutdown.phase(), ShutdownPhase::Running);

        // First request: begin drain + non-forced notice.
        request_shutdown(&ctx);
        assert_eq!(ctx.shutdown.phase(), ShutdownPhase::Draining);
        match events.recv().await.expect("drain notice") {
            proto::Event::DaemonDraining { forced } => assert!(!forced),
            other => panic!("expected DaemonDraining, got {other:?}"),
        }

        // Second request mid-drain: shorten to force + forced notice.
        request_shutdown(&ctx);
        assert_eq!(ctx.shutdown.phase(), ShutdownPhase::Forced);
        match events.recv().await.expect("forced notice") {
            proto::Event::DaemonDraining { forced } => assert!(forced),
            other => panic!("expected forced DaemonDraining, got {other:?}"),
        }

        // A third request is a no-op — already forced, no further events.
        request_shutdown(&ctx);
        assert_eq!(ctx.shutdown.phase(), ShutdownPhase::Forced);
    }

    /// New-user-work gate: once draining, `SendUserMessage` is refused with
    /// the `Shutdown` error code rather than dropped or queued.
    #[tokio::test]
    async fn send_user_message_refused_while_draining() {
        let ctx = test_ctx();
        let mut state = ClientState { attached: None };

        ctx.shutdown.begin_drain();

        let err = handle_request(
            Request::SendUserMessage {
                text: "hi".into(),
                images: vec![],
            },
            &mut state,
            &ctx,
        )
        .await
        .expect_err("draining daemon must refuse new user messages");
        assert_eq!(err.code, ErrorCode::Shutdown);
    }

    /// Refresh-on-daemon-connect (daemon side): the `GuidanceEstimate`
    /// request the TUI fires once a daemon comes up must resolve the project
    /// guidance file at `project_root` and return its basename plus non-zero
    /// sizes. This is the calibrated answer the indicator adopts in place of
    /// the launch-time local fallback, so it must never come back empty when
    /// a guidance file is present. `AGENTS.md` is in the shipped default
    /// list, so this holds independent of any host config override.
    #[tokio::test]
    async fn guidance_estimate_resolves_file_at_project_root() {
        let ctx = test_ctx();
        let mut state = ClientState { attached: None };
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "PROJECT RULES\n").unwrap();

        let resp = handle_request(
            Request::GuidanceEstimate {
                project_root: tmp.path().to_string_lossy().into_owned(),
                provider: None,
                model: None,
            },
            &mut state,
            &ctx,
        )
        .await
        .expect("guidance estimate must answer without an attached session");

        match resp {
            Response::GuidanceEstimate {
                file,
                tokens,
                system_tokens,
            } => {
                assert_eq!(file.as_deref(), Some("AGENTS.md"));
                assert!(tokens > 0, "non-empty guidance body sizes to > 0 tokens");
                assert!(system_tokens > 0, "system prompt baseline is non-zero");
            }
            other => panic!("expected GuidanceEstimate, got {other:?}"),
        }
    }
}
