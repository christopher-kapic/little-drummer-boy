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
}

impl DaemonContext {
    pub fn new(db: Db, locks: Arc<LockManager>, paths: DaemonPaths) -> Self {
        let registry = SessionRegistry::new(db.clone(), locks.clone());
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
        }
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

/// Bind the Unix socket and run the accept loop until `shutdown`
/// fires. Each accepted connection spawns a detached client task.
pub async fn run_accept_loop(
    ctx: Arc<DaemonContext>,
    listener: UnixListener,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    set_socket_perms(&ctx.paths.socket);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("daemon: shutdown signal received, closing accept loop");
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

        Request::ListSkills => Err(not_implemented("ListSkills")),
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
            // The shutdown watcher in `run_accept_loop` is signalled
            // by the daemon's external SIGTERM path; we honor that
            // path by sending SIGTERM to ourselves, which the existing
            // signal handler picks up cleanly.
            #[cfg(unix)]
            unsafe {
                libc::kill(std::process::id() as libc::pid_t, libc::SIGTERM);
            }
            Ok(Response::Ack)
        }
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
    let rows = match (project_id.as_deref(), parent_session_id) {
        (_, Some(parent)) => ctx.db.list_forks(parent).map_err(internal)?,
        (Some(pid), None) => ctx.db.list_root_sessions(pid, 100).map_err(internal)?,
        (None, None) => ctx.db.list_sessions(true, 100).map_err(internal)?,
    };
    let mut sessions = Vec::with_capacity(rows.len());
    for row in rows {
        let fork_count = ctx
            .db
            .count_forks_for(row.session_id)
            .map_err(internal)
            .unwrap_or(0);
        // Full subtree descendant count for the archive/delete cascade
        // statement (GOALS §17h) — direct forks plus their descendants.
        let descendant_count = ctx
            .db
            .count_descendants(row.session_id)
            .map_err(internal)
            .unwrap_or(0);
        // Read/unread + pending-question inputs for the browser's tiers
        // 3-4 (GOALS §17f). Best-effort: a query miss degrades to "no
        // activity / no open question" rather than failing the list.
        let latest_activity_at = ctx
            .db
            .latest_agent_activity_at(row.session_id)
            .ok()
            .flatten();
        let open_interrupts = ctx
            .db
            .list_open_interrupts(row.session_id)
            .map(|v| v.len() as u32)
            .unwrap_or(0);
        sessions.push(proto::SessionSummary {
            session_id: row.session_id,
            short_id: row.short_id,
            project_root: row.project_root,
            project_id: row.project_id,
            started_at: row.started_at,
            last_active_at: row.last_active_at,
            turns: 0, // wire up when we track turn count
            active_agent: row.active_agent,
            title: row.title,
            parent_session_id: row.parent_session_id,
            fork_count,
            descendant_count,
            last_viewed_at: row.last_viewed_at,
            latest_activity_at,
            open_interrupts,
            archived_at: row.archived_at,
        });
    }
    Ok(Response::Sessions { sessions })
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
