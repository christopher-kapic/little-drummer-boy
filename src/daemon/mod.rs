//! Daemon process + client. cockpit's daemon owns the session DB, the
//! lock manager, the redaction table, the provider clients, and the
//! configuration resolver (GOALS §8). The TUI is a *client* of the
//! daemon, not the process that does the work.
//!
//! Process layout:
//!
//! - [`proto`] — NDJSON wire schema. Same envelope shape for in-process
//!   channels, the Unix-socket transport, and (later) the WebSocket
//!   relay (`cockpit connect`, GOALS §8d).
//! - `server` (P2) — accept loop + per-client task + per-session worker.
//! - `client` (P3) — typed client over the proto.
//!
//! Lifecycle:
//!
//! - PID file at `$XDG_STATE_HOME/cockpit/daemon.pid`.
//! - Unix socket at `$XDG_RUNTIME_DIR/cockpit/cockpit.sock`, fallback
//!   to `$XDG_STATE_HOME/cockpit/daemon.sock`. Socket file mode is
//!   0600.
//! - First `cockpit` invocation auto-promotes via setsid + double-fork
//!   (GOALS §8b); the foreground terminal becomes a TUI client attached
//!   to the freshly spawned daemon. `cockpit daemon {start, stop,
//!   status}` lets the user manage the lifecycle explicitly.

pub mod client;
pub mod proto;
pub mod registry;
pub mod server;
pub mod session_worker;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Legacy line greeting sent by the v0 daemon when it had no real
/// protocol. Preserved through the P2 server cut-over so that
/// [`probe_blocking`] and any tooling that issued the early `"ok\n"`
/// probe still treats the daemon as reachable while the proto-based
/// handshake is being wired in. This constant goes away once the
/// proto handshake is the only path.
const SOCKET_GREETING: &str = "ok\n";

#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub pid_file: PathBuf,
    pub socket: PathBuf,
}

impl DaemonPaths {
    pub fn resolve() -> Result<Self> {
        let state = state_dir().context("could not locate state dir")?;
        std::fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
        let pid_file = state.join("daemon.pid");
        let socket = if let Some(rt) = runtime_dir() {
            std::fs::create_dir_all(&rt).with_context(|| format!("creating {}", rt.display()))?;
            rt.join("cockpit.sock")
        } else {
            state.join("daemon.sock")
        };
        Ok(Self { pid_file, socket })
    }
}

fn state_dir() -> Option<PathBuf> {
    if let Ok(s) = std::env::var("XDG_STATE_HOME") {
        if !s.trim().is_empty() {
            return Some(PathBuf::from(s).join("cockpit"));
        }
    }
    let home = dirs::home_dir()?;
    Some(home.join(".local/state/cockpit"))
}

fn runtime_dir() -> Option<PathBuf> {
    if let Ok(s) = std::env::var("XDG_RUNTIME_DIR") {
        if !s.trim().is_empty() {
            return Some(PathBuf::from(s).join("cockpit"));
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStatus {
    /// Daemon is running and the socket accepts a connection.
    Running,
    /// PID file exists but the process is dead or the socket is gone.
    Stale,
    /// No PID file.
    NotRunning,
}

/// Cheap probe: try to connect and read the daemon's "hello"
/// envelope. The server emits one immediately on accept (see
/// [`server::handle_client`]), so any successful read of a non-empty
/// line confirms the daemon is alive — no client-side write needed.
pub async fn probe(paths: &DaemonPaths) -> DaemonStatus {
    if !paths.socket.exists() {
        return if paths.pid_file.exists() {
            DaemonStatus::Stale
        } else {
            DaemonStatus::NotRunning
        };
    }
    match tokio::time::timeout(
        Duration::from_millis(500),
        UnixStream::connect(&paths.socket),
    )
    .await
    {
        Ok(Ok(mut stream)) => {
            let mut reader = tokio::io::BufReader::new(&mut stream);
            let mut line = String::new();
            match tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line))
                .await
            {
                Ok(Ok(_)) if !line.is_empty() => DaemonStatus::Running,
                _ => DaemonStatus::Stale,
            }
        }
        _ => DaemonStatus::Stale,
    }
}

/// Sync version of `probe`. Useful before the tokio runtime is up.
pub fn probe_blocking(paths: &DaemonPaths) -> DaemonStatus {
    use std::os::unix::net::UnixStream as StdUnixStream;
    if !paths.socket.exists() {
        return if paths.pid_file.exists() {
            DaemonStatus::Stale
        } else {
            DaemonStatus::NotRunning
        };
    }
    match StdUnixStream::connect(&paths.socket) {
        Ok(s) => {
            let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
            let mut buf = String::new();
            let mut r = BufReader::new(&s);
            match r.read_line(&mut buf) {
                Ok(_) if !buf.is_empty() => DaemonStatus::Running,
                _ => DaemonStatus::Stale,
            }
        }
        Err(_) => DaemonStatus::Stale,
    }
}

/// Spawn a detached daemon process. Returns the child PID. The current
/// process should *not* wait on the child — it's intended to outlive us.
pub fn spawn_detached() -> Result<u32> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().context("locating own binary")?;
    let child = Command::new(exe)
        .arg("daemon")
        .arg("start")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning daemon child")?;
    Ok(child.id())
}

/// Run the daemon's accept loop in the current process. Blocks until
/// SIGINT/SIGTERM. Boots the DB + lock manager, registers a shutdown
/// watcher, and runs the [`server::run_accept_loop`].
pub async fn run_foreground(paths: DaemonPaths) -> Result<()> {
    if matches!(probe(&paths).await, DaemonStatus::Running) {
        anyhow::bail!(
            "another daemon is already running (socket: {})",
            paths.socket.display()
        );
    }
    // Clear any stale leftover.
    let _ = std::fs::remove_file(&paths.socket);
    std::fs::write(&paths.pid_file, std::process::id().to_string())
        .with_context(|| format!("writing pid file {}", paths.pid_file.display()))?;

    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("binding {}", paths.socket.display()))?;

    let ctx = std::sync::Arc::new(server::boot(paths.clone())?);

    // `shutdown` is a watch channel: every long-running task (accept
    // loop, per-client tasks via the registry's broadcast) can observe
    // and stop cleanly.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel::<bool>(false);

    let signal_task = {
        let paths = paths.clone();
        let shutdown_tx = shutdown_tx.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut int = signal(SignalKind::interrupt()).ok();
                let mut term = signal(SignalKind::terminate()).ok();
                tokio::select! {
                    _ = async { if let Some(s) = int.as_mut() { s.recv().await; } } => {}
                    _ = async { if let Some(s) = term.as_mut() { s.recv().await; } } => {}
                }
            }
            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c().await.ok();
            }
            let _ = shutdown_tx.send(true);
            ctx.registry.shutdown_all().await;
            let _ = std::fs::remove_file(&paths.socket);
            let _ = std::fs::remove_file(&paths.pid_file);
        })
    };

    let accept = server::run_accept_loop(ctx, listener, shutdown_rx);
    let result = accept.await;
    let _ = signal_task.await;
    result
}

/// Kill the running daemon (if any) and clean up its pid + socket files.
pub fn stop(paths: &DaemonPaths) -> Result<bool> {
    let Some(pid) = read_pid(paths) else {
        // No pid file — best-effort socket cleanup.
        let _ = std::fs::remove_file(&paths.socket);
        return Ok(false);
    };
    #[cfg(unix)]
    {
        // SIGTERM is graceful — daemon's signal handler removes its
        // pid/socket files. Fall back to outright file cleanup if it
        // doesn't respond.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        // Wait briefly for the daemon to clean up.
        for _ in 0..20 {
            if !paths.pid_file.exists() {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Force cleanup if the daemon never disappeared (could've been
        // already dead from a crash).
        let _ = std::fs::remove_file(&paths.pid_file);
        let _ = std::fs::remove_file(&paths.socket);
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        let _ = std::fs::remove_file(&paths.pid_file);
        let _ = std::fs::remove_file(&paths.socket);
        Ok(true)
    }
}

fn read_pid(paths: &DaemonPaths) -> Option<u32> {
    let s = std::fs::read_to_string(&paths.pid_file).ok()?;
    s.trim().parse().ok()
}
