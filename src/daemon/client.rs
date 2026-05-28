//! Typed client over the daemon's NDJSON protocol.
//!
//! Spawns one background "reader/writer" task that owns the
//! [`ProtoStream`]; callers interact through:
//!
//! - [`DaemonClient::request`] — send one [`proto::Request`], wait for
//!   the matching [`proto::Response`] (or [`proto::ErrorPayload`]).
//! - [`DaemonClient::event_stream`] — clone-able subscriber to
//!   server-pushed events.
//!
//! The split lets the TUI driver fan multiple in-flight requests
//! through one socket while also reading the event stream, without
//! any locking ceremony in user code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::daemon::proto::{self, Body, Envelope, ErrorPayload, ProtoStream, Request, Response};

/// Outbound queue depth. Generous — request payloads are tiny.
const REQUEST_QUEUE: usize = 64;

/// Inbound event queue depth. Lagging consumers drop the oldest
/// events; the TUI is expected to drain fast enough to keep up. If it
/// can't, the right answer is "reattach" (the server re-sends the
/// current session state on `Attach`).
const EVENT_QUEUE: usize = 1024;

/// Default request timeout. Most requests are < 50ms; we set a
/// generous ceiling so a hung daemon causes a loud error rather than
/// a stalled TUI.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Public handle. Cheap to clone: every clone shares the same
/// background reader/writer task; only the event-stream subscription
/// differs.
#[derive(Clone)]
pub struct DaemonClient {
    request_tx: mpsc::Sender<Pending>,
    /// One channel per `DaemonClient` clone, hydrated by the reader
    /// task. We use `Arc<Mutex<_>>` because `mpsc::Receiver` isn't
    /// `Clone` — clones of `DaemonClient` share access to the
    /// receiver they were spawned with.
    events: Arc<tokio::sync::Mutex<mpsc::Receiver<proto::Event>>>,
}

struct Pending {
    request: Request,
    reply: oneshot::Sender<std::result::Result<Response, ErrorPayload>>,
}

impl DaemonClient {
    /// Connect to the daemon at `socket`. Spawns the background task
    /// before returning.
    pub async fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to {}", socket.display()))?;
        let proto = ProtoStream::new(stream);
        Ok(Self::from_proto(proto))
    }

    fn from_proto(proto: ProtoStream<UnixStream>) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<Pending>(REQUEST_QUEUE);
        let (event_tx, event_rx) = mpsc::channel::<proto::Event>(EVENT_QUEUE);
        tokio::spawn(run_io(proto, request_rx, event_tx));
        Self {
            request_tx,
            events: Arc::new(tokio::sync::Mutex::new(event_rx)),
        }
    }

    /// Send a request and wait for the matching response. Returns the
    /// daemon's typed [`proto::ErrorPayload`] when the request was
    /// rejected, distinct from transport / timeout errors which come
    /// back as `Err(anyhow)`.
    pub async fn request(
        &self,
        request: Request,
    ) -> Result<std::result::Result<Response, ErrorPayload>> {
        let (tx, rx) = oneshot::channel();
        self.request_tx
            .send(Pending { request, reply: tx })
            .await
            .map_err(|_| anyhow!("daemon client task has stopped"))?;
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(anyhow!("daemon client dropped reply channel")),
            Err(_) => Err(anyhow!("request timed out after {:?}", REQUEST_TIMEOUT)),
        }
    }

    /// Convenience: send a request, unwrap typed errors as `Err`.
    pub async fn request_ok(&self, request: Request) -> Result<Response> {
        match self.request(request).await? {
            Ok(r) => Ok(r),
            Err(e) => Err(anyhow!("daemon error: {e}")),
        }
    }

    /// Pull the next server-pushed event. Returns `None` when the
    /// connection has closed. Multi-call from multiple cloned
    /// clients is fine; each event is delivered to exactly one
    /// caller (we don't use broadcast on the client side because
    /// the TUI is the single consumer; the broadcast lives on the
    /// daemon side where multi-client is the design point).
    pub async fn next_event(&self) -> Option<proto::Event> {
        let mut events = self.events.lock().await;
        events.recv().await
    }
}

async fn run_io(
    mut proto: ProtoStream<UnixStream>,
    mut request_rx: mpsc::Receiver<Pending>,
    event_tx: mpsc::Sender<proto::Event>,
) {
    let mut pending: HashMap<Uuid, oneshot::Sender<std::result::Result<Response, ErrorPayload>>> =
        HashMap::new();

    loop {
        tokio::select! {
            biased;

            // Inbound envelope from the daemon.
            recv = proto.recv() => {
                match recv {
                    Ok(None) => {
                        tracing::debug!("daemon closed the connection");
                        break;
                    }
                    Ok(Some(env)) => {
                        match env.body {
                            Body::Response { id, response } => {
                                if let Some(tx) = pending.remove(&id) {
                                    let _ = tx.send(Ok(response));
                                } else {
                                    tracing::warn!(id = %id, "daemon responded with unknown id");
                                }
                            }
                            Body::Error { id, error } => {
                                match id {
                                    Some(id) => {
                                        if let Some(tx) = pending.remove(&id) {
                                            let _ = tx.send(Err(error));
                                        } else {
                                            tracing::warn!(id = %id, ?error, "daemon error for unknown id");
                                        }
                                    }
                                    None => {
                                        tracing::warn!(?error, "out-of-band daemon error");
                                    }
                                }
                            }
                            Body::Event { event } => {
                                if event_tx.send(event).await.is_err() {
                                    // The consumer dropped — we're
                                    // closing soon. Keep reading so
                                    // we don't fill OS buffers.
                                }
                            }
                            Body::Request { id, request } => {
                                tracing::warn!(id = %id, ?request, "daemon sent a request to a client; ignoring");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = ?e, "daemon read failed; closing");
                        break;
                    }
                }
            }

            // Outbound request from the user.
            req = request_rx.recv() => {
                match req {
                    None => {
                        // All DaemonClient handles dropped; exit cleanly.
                        break;
                    }
                    Some(p) => {
                        let id = Uuid::new_v4();
                        pending.insert(id, p.reply);
                        let envelope = Envelope::request(id, p.request);
                        if let Err(e) = proto.send(&envelope).await {
                            tracing::warn!(error = ?e, "daemon write failed");
                            if let Some(tx) = pending.remove(&id) {
                                let _ = tx.send(Err(ErrorPayload {
                                    code: proto::ErrorCode::Internal,
                                    message: format!("write to daemon failed: {e}"),
                                }));
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    // Drain any pending requests with an explicit "connection closed."
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(ErrorPayload {
            code: proto::ErrorCode::Internal,
            message: "daemon connection closed".into(),
        }));
    }
}

// ---- lifecycle helpers ----------------------------------------------------

/// Strategy for getting a daemon to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleMode {
    /// "Attach if running, otherwise auto-promote a long-running
    /// background daemon." The TUI's default.
    AttachOrAutoPromote,
    /// "Attach if running, otherwise spawn a temporary daemon I'll
    /// stop on exit." Default for `cockpit run`. The flag name on
    /// the CLI is `--ephemeral`.
    AttachOrEphemeral,
    /// "Always spawn a fresh ephemeral daemon, even if one is
    /// running." Used by `cockpit run --ephemeral`.
    AlwaysEphemeral,
}

/// Connect-or-spawn result: a ready-to-use client plus a flag the
/// caller honors when it's time to shut down — `owns_daemon = true`
/// means "you spawned this daemon, so stop it on your way out."
pub struct ConnectedDaemon {
    pub client: DaemonClient,
    pub owns_daemon: bool,
    pub socket: PathBuf,
}

/// Find the daemon socket, optionally spawn the daemon, return a
/// connected client. Honors [`LifecycleMode`].
pub async fn probe_or_spawn(mode: LifecycleMode) -> Result<ConnectedDaemon> {
    use crate::daemon::{
        DaemonPaths, DaemonStatus, probe, spawn_detached, spawn_detached_ephemeral,
    };

    let canonical = DaemonPaths::resolve_canonical()?;

    match mode {
        LifecycleMode::AttachOrAutoPromote | LifecycleMode::AttachOrEphemeral => {
            if matches!(probe(&canonical).await, DaemonStatus::Running) {
                let client = DaemonClient::connect(&canonical.socket).await?;
                return Ok(ConnectedDaemon {
                    client,
                    owns_daemon: false,
                    socket: canonical.socket,
                });
            }
        }
        LifecycleMode::AlwaysEphemeral => {
            // Always spawn fresh on a unique per-pid ephemeral path
            // (Layer B). It never touches the canonical socket, so it
            // coexists with a persistent daemon — no "already running"
            // bail needed.
        }
    }

    // No reachable daemon to attach to — spawn one.
    //
    // `AttachOrAutoPromote` (the TUI) promotes a *persistent* daemon at
    // the canonical path. The two ephemeral modes spawn a per-pid
    // ephemeral daemon (Layer B): unique socket/pid the canonical
    // `daemon stop`/`status` never sees, with the self-reaping watchdog
    // armed (Layer C) so an uncatchable foreground death can't orphan it.
    let ephemeral = matches!(
        mode,
        LifecycleMode::AttachOrEphemeral | LifecycleMode::AlwaysEphemeral
    );

    let (paths, pid) = if ephemeral {
        // Derive the ephemeral path set from *our* pid so it's unique
        // per run, then hand it to the spawned daemon to bind.
        let paths = DaemonPaths::resolve_ephemeral(std::process::id())?;
        let pid = spawn_detached_ephemeral(&paths)?;
        (paths, pid)
    } else {
        let pid = spawn_detached()?;
        (canonical, pid)
    };
    tracing::info!(pid = pid, ephemeral = ephemeral, "daemon spawned");

    // Wait for the socket + a successful handshake.
    let client = wait_for_daemon(&paths.socket).await?;

    Ok(ConnectedDaemon {
        client,
        owns_daemon: ephemeral,
        socket: paths.socket,
    })
}

/// Poll for the daemon socket and an actual DaemonStatus response.
/// 50ms initial backoff, doubling up to 250ms; total cap 5s.
async fn wait_for_daemon(socket: &Path) -> Result<DaemonClient> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut backoff = Duration::from_millis(50);

    loop {
        if socket.exists() {
            match DaemonClient::connect(socket).await {
                Ok(client) => {
                    // Sanity check — first request after connect.
                    if client.request_ok(Request::DaemonStatus).await.is_ok() {
                        return Ok(client);
                    }
                }
                Err(_) => {} // socket exists but accept hasn't started yet
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for daemon at {}", socket.display());
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_millis(250));
    }
}
