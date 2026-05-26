//! Minimal daemon process + client.
//!
//! v1 scope (per [[daemon_and_ipc]]): the daemon is a long-running
//! background process that the TUI connects to as a client. Real IPC —
//! session ownership, agent fan-out, the websocket relay — is the v2
//! follow-up. For now the daemon owns:
//!
//!   - A PID file at `$XDG_STATE_HOME/cockpit/daemon.pid`.
//!   - A Unix socket at `$XDG_RUNTIME_DIR/cockpit.sock` (fall back to
//!     `$XDG_STATE_HOME/cockpit/daemon.sock` when runtime dir isn't set).
//!   - A trivial accept loop that responds `ok\n` to any line. Enough
//!     for the TUI to verify the daemon is reachable.
//!
//! Lifecycle commands: `cockpit daemon {start, stop, status}`. The TUI
//! uses [`probe`] on launch.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

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

/// Cheap probe: try to connect to the socket and read a line. Returns
/// `Running` only when both the socket exists AND the daemon replies.
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
            let _ = stream.write_all(b"ping\n").await;
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
        Ok(mut s) => {
            let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
            let _ = s.write_all(b"ping\n");
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
/// SIGINT/SIGTERM or until the pid-file is removed externally.
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

    let cleanup = {
        let paths = paths.clone();
        async move {
            // Wait for either SIGINT or SIGTERM.
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
            let _ = std::fs::remove_file(&paths.socket);
            let _ = std::fs::remove_file(&paths.pid_file);
        }
    };

    tokio::select! {
        _ = cleanup => Ok(()),
        r = accept_loop(listener) => r,
    }
}

async fn accept_loop(listener: UnixListener) -> Result<()> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = stream.write_all(SOCKET_GREETING.as_bytes()).await;
            // Read & discard any single line, then close.
            let mut buf = [0u8; 256];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
        });
    }
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
