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

/// Daemon-wide singletons. Held in an `Arc` so per-client tasks can
/// share without copying.
pub struct DaemonContext {
    pub db: Db,
    pub locks: Arc<LockManager>,
    pub registry: SessionRegistry,
    pub paths: DaemonPaths,
    pub started_at: Instant,
}

impl DaemonContext {
    pub fn new(db: Db, locks: Arc<LockManager>, paths: DaemonPaths) -> Self {
        let registry = SessionRegistry::new(db.clone(), locks.clone());
        Self {
            db,
            locks,
            registry,
            paths,
            started_at: Instant::now(),
        }
    }
}

/// Bootstrap the daemon: open the DB, build the lock manager, return
/// a ready-to-use context. Called from `daemon::run_foreground`.
pub fn boot(paths: DaemonPaths) -> Result<DaemonContext> {
    let db = Db::open_default().context("opening session DB")?;
    let locks = Arc::new(LockManager::from_db(db.clone()).context("loading lock state")?);
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
}

async fn handle_client(stream: UnixStream, ctx: Arc<DaemonContext>) -> Result<()> {
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
    ctx: &DaemonContext,
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
    ctx: &DaemonContext,
) -> std::result::Result<Response, ErrorPayload> {
    match request {
        Request::Attach {
            session_id,
            project_root,
        } => attach(state, ctx, session_id, project_root),

        Request::SendUserMessage { text } => {
            let att = require_attached(state)?;
            att.handle
                .send_work(SessionWork::UserMessage(text))
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

        Request::ForkSession {
            parent_session_id,
            fork_point_turn_id,
        } => fork_session(ctx, parent_session_id, fork_point_turn_id),

        Request::RenameSession { session_id, title } => rename_session(ctx, session_id, &title),

        Request::DeleteSession {
            session_id,
            cascade,
        } => delete_session(ctx, session_id, cascade),

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

fn attach(
    state: &mut ClientState,
    ctx: &DaemonContext,
    session_id: Option<Uuid>,
    project_root: Option<String>,
) -> std::result::Result<Response, ErrorPayload> {
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
        .attach(session_id, project_root, &providers_cfg, &extended_cfg)
        .map_err(internal)?;

    // Replace any prior attachment.
    let event_rx = handle.subscribe();
    let session_id = handle.session_id;
    let project_root = handle.project_root.to_string_lossy().into_owned();
    let active_agent = handle.active_agent_name.clone();

    state.attached = Some(AttachedSession { handle, event_rx });

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

    let project_id = ctx
        .db
        .get_session(session_id)
        .ok()
        .flatten()
        .map(|s| s.project_id)
        .unwrap_or_default();

    Ok(Response::Attached {
        session_id,
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

fn delete_session(
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
    ctx.db
        .delete_session(session_id, cascade)
        .map_err(internal)?;
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
        message: err.to_string(),
    }
}

fn not_implemented(what: &str) -> ErrorPayload {
    ErrorPayload {
        code: ErrorCode::Internal,
        message: format!("{what} not yet implemented in v1"),
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
