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

pub mod caffeinate;
pub mod client;
pub mod proto;
pub mod registry;
pub mod server;
pub mod session_worker;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::net::{UnixListener, UnixStream};

/// Legacy line greeting sent by the v0 daemon when it had no real
/// protocol. Preserved through the P2 server cut-over so that
/// [`probe_blocking`] and any tooling that issued the early `"ok\n"`
/// probe still treats the daemon as reachable while the proto-based
/// handshake is being wired in. This constant goes away once the
/// proto handshake is the only path.
const SOCKET_GREETING: &str = "ok\n";

/// Env var carrying the ephemeral daemon's socket path from the parent
/// `run` process to the daemon child it spawns. Internal wiring only —
/// never exposed on the user-facing CLI surface (Layer B). Its presence
/// is also what flips the child into ephemeral mode (enabling the
/// self-reaping watchdog, Layer C).
const EPHEMERAL_SOCKET_ENV: &str = "COCKPIT_EPHEMERAL_SOCKET";
/// Companion to [`EPHEMERAL_SOCKET_ENV`]: the ephemeral pid-file path.
const EPHEMERAL_PID_ENV: &str = "COCKPIT_EPHEMERAL_PID_FILE";

#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub pid_file: PathBuf,
    pub socket: PathBuf,
    /// True for a per-run ephemeral daemon (unique `cockpit-eph-<pid>`
    /// paths); false for the canonical persistent daemon. Gates the
    /// idle-reaping watchdog (Layer C) — the persistent daemon must
    /// never self-exit on idle.
    pub ephemeral: bool,
}

impl DaemonPaths {
    /// Resolve the daemon paths. A daemon child spawned for an
    /// ephemeral run inherits its unique path set from
    /// [`EPHEMERAL_SOCKET_ENV`] / [`EPHEMERAL_PID_ENV`] (set by the
    /// parent via [`spawn_detached_ephemeral`]); everyone else gets the
    /// canonical persistent path set.
    pub fn resolve() -> Result<Self> {
        if let Some(paths) = Self::from_ephemeral_env()? {
            return Ok(paths);
        }
        Self::resolve_canonical()
    }

    /// The canonical persistent daemon's path set. `cockpit daemon
    /// {start,stop,status}` operate exclusively on these.
    pub fn resolve_canonical() -> Result<Self> {
        let state = state_dir().context("could not locate state dir")?;
        std::fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
        let pid_file = state.join("daemon.pid");
        let socket = if let Some(rt) = runtime_dir() {
            std::fs::create_dir_all(&rt).with_context(|| format!("creating {}", rt.display()))?;
            rt.join("cockpit.sock")
        } else {
            state.join("daemon.sock")
        };
        Ok(Self {
            pid_file,
            socket,
            ephemeral: false,
        })
    }

    /// Derive a unique ephemeral path set keyed on `pid`:
    /// `cockpit-eph-<pid>.sock` + `cockpit-eph-<pid>.pid`, in the same
    /// directory the canonical socket/pid would live in. Deterministic
    /// in `pid`, so the parent can compute the path it will hand to the
    /// child it spawns (Layer B).
    pub fn resolve_ephemeral(pid: u32) -> Result<Self> {
        let state = state_dir().context("could not locate state dir")?;
        std::fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
        let pid_file = state.join(format!("cockpit-eph-{pid}.pid"));
        let socket = if let Some(rt) = runtime_dir() {
            std::fs::create_dir_all(&rt).with_context(|| format!("creating {}", rt.display()))?;
            rt.join(format!("cockpit-eph-{pid}.sock"))
        } else {
            state.join(format!("cockpit-eph-{pid}.sock"))
        };
        Ok(Self {
            pid_file,
            socket,
            ephemeral: true,
        })
    }

    /// Reconstruct the ephemeral path set the parent chose, from the
    /// internal env vars. Returns `Ok(None)` when not running as an
    /// ephemeral child (the common case).
    fn from_ephemeral_env() -> Result<Option<Self>> {
        let socket = std::env::var_os(EPHEMERAL_SOCKET_ENV);
        let pid_file = std::env::var_os(EPHEMERAL_PID_ENV);
        match (socket, pid_file) {
            (Some(socket), Some(pid_file)) => {
                let socket = PathBuf::from(socket);
                if let Some(parent) = socket.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                Ok(Some(Self {
                    pid_file: PathBuf::from(pid_file),
                    socket,
                    ephemeral: true,
                }))
            }
            _ => Ok(None),
        }
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

/// Spawn a detached *canonical* daemon process. Returns the child PID.
/// The current process should *not* wait on the child — it's intended
/// to outlive us. `no_sandbox` forwards the daemon-level `--no-sandbox`
/// (sandboxing part 2): the child disables filesystem sandboxing for all
/// its sessions.
pub fn spawn_detached(no_sandbox: bool) -> Result<u32> {
    spawn_detached_inner(None, no_sandbox)
}

/// Spawn a detached *ephemeral* daemon bound to `paths` (a unique
/// `cockpit-eph-<pid>` path set from [`DaemonPaths::resolve_ephemeral`]).
/// The child binds the exact path the parent chose by reading the
/// internal env vars (Layer B); never via the user-facing CLI surface.
/// Returns the child PID.
///
/// An auto-promoted ephemeral daemon is never launched `--no-sandbox`:
/// the client's `--no-sandbox` is a *per-session* default passed at
/// attach time, not a daemon-level one (sandboxing part 2 precedence).
pub fn spawn_detached_ephemeral(paths: &DaemonPaths) -> Result<u32> {
    spawn_detached_inner(Some(paths), false)
}

fn spawn_detached_inner(ephemeral: Option<&DaemonPaths>, no_sandbox: bool) -> Result<u32> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().context("locating own binary")?;
    let mut command = Command::new(exe);
    command
        .arg("daemon")
        .arg("start")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if no_sandbox {
        command.arg("--no-sandbox");
    }
    if let Some(paths) = ephemeral {
        command
            .env(EPHEMERAL_SOCKET_ENV, &paths.socket)
            .env(EPHEMERAL_PID_ENV, &paths.pid_file);
    }
    let child = command.spawn().context("spawning daemon child")?;
    Ok(child.id())
}

/// Idle grace period for the ephemeral self-reaping watchdog (Layer C).
/// When the last client of an *ephemeral* daemon disconnects, the
/// daemon waits this long before exiting on its own; a reconnect within
/// the window cancels the countdown. Bounds the lifetime of an orphan
/// left by an uncatchable foreground death (SIGKILL, power loss) to
/// roughly this value. The persistent daemon never arms this timer.
pub const EPHEMERAL_IDLE_GRACE: Duration = Duration::from_secs(30);

/// Run the daemon's accept loop in the current process. Blocks until
/// SIGINT/SIGTERM. Boots the DB + lock manager, registers a shutdown
/// watcher, and runs the [`server::run_accept_loop`].
pub async fn run_foreground(paths: DaemonPaths) -> Result<()> {
    run_foreground_inner(paths, EPHEMERAL_IDLE_GRACE).await
}

/// Like [`run_foreground`] but with an injectable idle-grace duration so
/// tests can exercise the ephemeral watchdog (Layer C) without sleeping
/// the full 30s of wall-clock.
pub async fn run_foreground_inner(paths: DaemonPaths, idle_grace: Duration) -> Result<()> {
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
        let shutdown_tx = shutdown_tx.clone();
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
        })
    };

    // Layer C: ephemeral-only self-reaping watchdog. The persistent
    // daemon must never self-exit on idle, so the watchdog is armed only
    // when this daemon owns an ephemeral path set (Layer B's flag). It
    // shares the same `shutdown_tx` as the signal handler, so a fired
    // timer drives the identical clean-shutdown path.
    let watchdog_task = if paths.ephemeral {
        let shutdown_tx = shutdown_tx.clone();
        let client_presence = ctx.client_presence();
        Some(tokio::spawn(async move {
            idle_watchdog(client_presence, idle_grace, shutdown_tx).await;
        }))
    } else {
        None
    };

    let accept = server::run_accept_loop(ctx.clone(), listener, shutdown_rx);
    let result = accept.await;

    // The accept loop has stopped (shutdown fired). Drain the workers,
    // then remove our own socket + pid files. For an ephemeral daemon
    // these are the unique `cockpit-eph-<pid>` files; for the canonical
    // daemon they're the shared persistent files — either way, the
    // process that bound them is the one cleaning them up.
    ctx.registry.shutdown_all().await;
    let _ = std::fs::remove_file(&paths.socket);
    let _ = std::fs::remove_file(&paths.pid_file);

    signal_task.abort();
    if let Some(watchdog) = watchdog_task {
        watchdog.abort();
    }
    result
}

/// Ephemeral self-reaping watchdog (Layer C). Watches `presence` (a
/// live count of connected clients). Whenever the count drops to zero,
/// it starts an `idle_grace` countdown; if a client reconnects before
/// the timer fires, the countdown is cancelled and the daemon keeps
/// running; if the timer fires with still no client, it signals
/// shutdown via `shutdown_tx`. Idempotent: re-entry just re-reads the
/// latest count.
async fn idle_watchdog(
    mut presence: tokio::sync::watch::Receiver<usize>,
    idle_grace: Duration,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    loop {
        // Block until there are no connected clients.
        if *presence.borrow() != 0 {
            if presence.changed().await.is_err() {
                // Sender dropped — daemon is tearing down anyway.
                return;
            }
            continue;
        }

        // No clients: race the grace timer against a reconnect.
        tokio::select! {
            _ = tokio::time::sleep(idle_grace) => {
                // Re-check under the borrow: a client may have connected
                // in the same tick the timer fired.
                if *presence.borrow() == 0 {
                    tracing::info!("ephemeral daemon idle past grace; self-reaping");
                    let _ = shutdown_tx.send(true);
                    return;
                }
            }
            changed = presence.changed() => {
                if changed.is_err() {
                    return;
                }
                // Loop re-evaluates the (possibly non-zero) count.
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Layer B: ephemeral paths are keyed on pid, live in the same
    /// directory as the canonical paths, use the `cockpit-eph-<pid>`
    /// scheme, and are flagged ephemeral. The canonical paths are
    /// distinct and never flagged ephemeral.
    #[test]
    fn ephemeral_paths_are_unique_and_distinct_from_canonical() {
        let eph_a = DaemonPaths::resolve_ephemeral(111).expect("resolve eph a");
        let eph_b = DaemonPaths::resolve_ephemeral(222).expect("resolve eph b");
        let canonical = DaemonPaths::resolve_canonical().expect("resolve canonical");

        // Unique per pid.
        assert_ne!(eph_a.socket, eph_b.socket);
        assert_ne!(eph_a.pid_file, eph_b.pid_file);

        // `cockpit-eph-<pid>` scheme.
        assert_eq!(
            eph_a.socket.file_name().unwrap().to_string_lossy(),
            "cockpit-eph-111.sock"
        );
        assert_eq!(
            eph_a.pid_file.file_name().unwrap().to_string_lossy(),
            "cockpit-eph-111.pid"
        );

        // Same parent directory as the canonical socket/pid.
        assert_eq!(eph_a.socket.parent(), canonical.socket.parent());
        assert_eq!(eph_a.pid_file.parent(), canonical.pid_file.parent());

        // Never collides with the canonical files.
        assert_ne!(eph_a.socket, canonical.socket);
        assert_ne!(eph_a.pid_file, canonical.pid_file);

        // Flags.
        assert!(eph_a.ephemeral);
        assert!(eph_b.ephemeral);
        assert!(!canonical.ephemeral);
    }

    /// Layer B wiring: a daemon child started for an ephemeral run binds
    /// the exact path set the parent chose, transmitted via the internal
    /// env vars. `resolve()` honors those env vars (flagging ephemeral);
    /// absent them, it falls back to the canonical path set.
    #[test]
    fn resolve_honors_ephemeral_env() {
        // This test mutates process-global env; keep all of it in one
        // test to bound the race surface against other env-reading code.
        let prev_socket = std::env::var_os(EPHEMERAL_SOCKET_ENV);
        let prev_pid = std::env::var_os(EPHEMERAL_PID_ENV);

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("chosen.sock");
        let pid_file = dir.path().join("chosen.pid");

        // SAFETY: single-threaded test body; we restore on the way out.
        unsafe {
            std::env::set_var(EPHEMERAL_SOCKET_ENV, &socket);
            std::env::set_var(EPHEMERAL_PID_ENV, &pid_file);
        }
        let resolved = DaemonPaths::resolve().expect("resolve with env");
        assert_eq!(resolved.socket, socket);
        assert_eq!(resolved.pid_file, pid_file);
        assert!(resolved.ephemeral);

        // SAFETY: same as above.
        unsafe {
            std::env::remove_var(EPHEMERAL_SOCKET_ENV);
            std::env::remove_var(EPHEMERAL_PID_ENV);
        }
        let canonical = DaemonPaths::resolve().expect("resolve without env");
        assert!(!canonical.ephemeral);

        // Restore whatever was there before so we don't disturb siblings.
        unsafe {
            match prev_socket {
                Some(v) => std::env::set_var(EPHEMERAL_SOCKET_ENV, v),
                None => std::env::remove_var(EPHEMERAL_SOCKET_ENV),
            }
            match prev_pid {
                Some(v) => std::env::set_var(EPHEMERAL_PID_ENV, v),
                None => std::env::remove_var(EPHEMERAL_PID_ENV),
            }
        }
    }

    /// Layer C: with no connected client, the watchdog signals shutdown
    /// once the (injected, short) grace elapses.
    #[tokio::test(start_paused = true)]
    async fn watchdog_reaps_after_idle_grace() {
        let (presence_tx, presence_rx) = tokio::sync::watch::channel(0usize);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let grace = Duration::from_secs(30);

        let task = tokio::spawn(idle_watchdog(presence_rx, grace, shutdown_tx));

        // Advance past the grace window. With paused time this is
        // deterministic and instant — no wall-clock sleep.
        tokio::time::advance(grace + Duration::from_secs(1)).await;

        shutdown_rx.changed().await.expect("shutdown signalled");
        assert!(*shutdown_rx.borrow());
        drop(presence_tx);
        let _ = task.await;
    }

    /// Layer C: a client reconnecting inside the grace window cancels the
    /// countdown; the daemon does not self-exit while a client is present.
    #[tokio::test(start_paused = true)]
    async fn watchdog_reconnect_cancels_countdown() {
        let (presence_tx, presence_rx) = tokio::sync::watch::channel(0usize);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let grace = Duration::from_secs(30);

        let task = tokio::spawn(idle_watchdog(presence_rx, grace, shutdown_tx));

        // A client connects partway through the grace window.
        tokio::time::advance(grace / 2).await;
        presence_tx.send(1).unwrap();

        // Even well past the original deadline, no shutdown fires while a
        // client is connected.
        tokio::time::advance(grace * 2).await;
        tokio::task::yield_now().await;
        assert!(!*shutdown_rx.borrow(), "watchdog reaped despite a client");

        drop(presence_tx);
        let _ = task.await;
    }

    /// End-to-end gating (Layers B + C): a real *ephemeral* daemon with
    /// no client self-reaps within the injected grace and removes its
    /// own socket + pid files; a real *persistent* daemon with the same
    /// idle conditions stays up. Uses a short real-time grace (not the
    /// 30s production value) so the test stays fast.
    #[tokio::test]
    async fn ephemeral_self_reaps_persistent_does_not() {
        // `server::boot` opens the canonical DB via `cockpit_data_dir()`;
        // point it at a throwaway dir so the test never touches real
        // user state. Save/restore to avoid disturbing sibling tests.
        let data = tempfile::tempdir().expect("data tempdir");
        let prev_data = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: single-threaded test setup; restored at the end.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", data.path());
        }

        let sock_dir = tempfile::tempdir().expect("sock tempdir");
        let grace = Duration::from_millis(300);

        // --- Ephemeral: must self-reap. ---
        let eph = DaemonPaths {
            socket: sock_dir.path().join("eph.sock"),
            pid_file: sock_dir.path().join("eph.pid"),
            ephemeral: true,
        };
        let eph_clone = eph.clone();
        let eph_task = tokio::spawn(async move { run_foreground_inner(eph_clone, grace).await });

        // Wait for it to come up.
        wait_until(|| eph.socket.exists(), Duration::from_secs(2)).await;
        assert!(eph.pid_file.exists(), "ephemeral pid file written");

        // No client ever connects; it should self-reap and clean up.
        let reaped = tokio::time::timeout(Duration::from_secs(3), eph_task)
            .await
            .expect("ephemeral daemon did not self-reap in time");
        reaped.expect("join").expect("run_foreground_inner ok");
        assert!(!eph.socket.exists(), "ephemeral socket removed on reap");
        assert!(!eph.pid_file.exists(), "ephemeral pid removed on reap");

        // --- Persistent: must NOT self-reap. ---
        let persistent = DaemonPaths {
            socket: sock_dir.path().join("persist.sock"),
            pid_file: sock_dir.path().join("persist.pid"),
            ephemeral: false,
        };
        let persistent_clone = persistent.clone();
        let persist_task =
            tokio::spawn(async move { run_foreground_inner(persistent_clone, grace).await });
        wait_until(|| persistent.socket.exists(), Duration::from_secs(2)).await;

        // Past several grace windows with no client: still alive.
        tokio::time::sleep(grace * 4).await;
        assert!(
            persistent.socket.exists(),
            "persistent daemon must never self-reap on idle"
        );
        assert!(
            !persist_task.is_finished(),
            "persistent daemon exited on idle"
        );

        // Tear it down so the test leaves nothing behind.
        persist_task.abort();
        let _ = persist_task.await;
        let _ = std::fs::remove_file(&persistent.socket);
        let _ = std::fs::remove_file(&persistent.pid_file);

        // SAFETY: restore env.
        unsafe {
            match prev_data {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }

    async fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while !cond() {
            if std::time::Instant::now() >= deadline {
                panic!("condition not met within {timeout:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
