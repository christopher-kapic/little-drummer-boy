//! `cockpit run` — one-shot non-interactive prompt through the daemon.
//!
//! Lifecycle (GOALS §8b + the user's refinement on the `--ephemeral`
//! flag):
//!
//! - **Default:** attach to a long-running daemon if one is up;
//!   otherwise spawn an ephemeral daemon that exits when the run
//!   completes.
//! - **`--ephemeral`:** always spawn a fresh daemon for this run. The
//!   daemon ends when the run does.
//!
//! Behavior:
//!
//! 1. Resolve project root (cwd or `--project`).
//! 2. Build the prompt (argv + stdin).
//! 3. probe_or_spawn the daemon, attach a new session.
//! 4. Send the prompt and pump events until `TurnComplete`.
//! 5. In `default` format we stream assistant text to stdout; in
//!    `json` format we emit one envelope per line.
//! 6. If we own the daemon (ephemeral path), shut it down. An
//!    [`EphemeralDaemonGuard`] (Layer A) guarantees this fires on every
//!    exit — happy path, early `?` error, panic/unwind, or
//!    SIGINT/SIGTERM — never on a run that attached to a pre-existing
//!    persistent daemon.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use crate::cli::{OutputFormat, RunArgs};
use crate::daemon::client::{LifecycleMode, probe_or_spawn};
use crate::daemon::proto::{self, Envelope, Request, Response};

/// RAII backstop that shuts down an ephemeral daemon this `run` process
/// owns, on **every** exit path — early `?` returns, panics/unwinds, and
/// the normal end of the turn (Layer A). A run that *attached* to a
/// pre-existing persistent daemon (`owns_daemon = false`) builds no
/// guard, so it never shuts anything down.
///
/// The drop performs a best-effort *synchronous* `StopDaemon` so it
/// works from inside `Drop` without juggling the async runtime: it
/// connects to the daemon's Unix socket with the std (blocking)
/// `UnixStream` and writes one NDJSON `StopDaemon` request. The daemon
/// SIGTERMs itself on receipt (see `server::handle_request`).
struct EphemeralDaemonGuard {
    socket: PathBuf,
    /// Cleared once shutdown has been requested (happy path) so the
    /// drop doesn't fire a redundant second request.
    armed: Arc<AtomicBool>,
}

impl EphemeralDaemonGuard {
    fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            armed: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Disarm and synchronously request shutdown. Idempotent: the first
    /// caller wins, later calls (including the drop) are no-ops.
    fn shutdown(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            stop_daemon_blocking(&self.socket);
        }
    }
}

impl Drop for EphemeralDaemonGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Best-effort synchronous `StopDaemon`. Connects to the daemon socket
/// with the blocking std `UnixStream`, writes one NDJSON request, and
/// returns — usable from `Drop`. Any failure (daemon already gone,
/// socket removed) is swallowed; the watchdog (Layer C) is the final
/// backstop.
fn stop_daemon_blocking(socket: &Path) {
    let Ok(envelope) = serde_json::to_string(&Envelope::request(
        uuid::Uuid::new_v4(),
        Request::StopDaemon,
    )) else {
        return;
    };
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream as StdUnixStream;
        use std::time::Duration;
        if let Ok(mut stream) = StdUnixStream::connect(socket) {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
            let _ = stream.write_all(envelope.as_bytes());
            let _ = stream.write_all(b"\n");
            let _ = stream.flush();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, envelope);
    }
}

pub async fn run(args: RunArgs, no_sandbox: bool) -> Result<()> {
    let prompt = build_prompt(&args)?;
    if prompt.trim().is_empty() {
        anyhow::bail!("no prompt supplied (pass a message or pipe one on stdin)");
    }

    let mode = if args.ephemeral {
        LifecycleMode::AlwaysEphemeral
    } else {
        LifecycleMode::AttachOrEphemeral
    };

    let daemon = probe_or_spawn(mode).await?;
    let client = daemon.client.clone();

    // Layer A: arm the shutdown backstop *only* when we own the daemon.
    // Held across every `?` below so an error return still reaps it.
    let guard = daemon
        .owns_daemon
        .then(|| EphemeralDaemonGuard::new(daemon.socket.clone()));

    // A signal handler so Ctrl-C / SIGTERM during the run reaps the
    // daemon instead of orphaning it. Shares the guard's armed flag and
    // socket so it drives the identical synchronous shutdown.
    let signal_task = spawn_signal_shutdown(guard.as_ref());

    let result = run_turn(&client, &args, prompt, no_sandbox).await;

    // Stop the signal watcher and run the (now happy-path) shutdown
    // before deciding the exit code, so the daemon is gone whether the
    // turn succeeded or errored.
    if let Some(task) = signal_task {
        task.abort();
    }
    if let Some(guard) = &guard {
        guard.shutdown();
    }
    // Drop the guard explicitly here so its (now-disarmed) drop is a
    // no-op and we don't carry it past `process::exit`.
    drop(guard);

    let exit_code = result?;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// Attach, send the prompt, pump events. Split out so the `?` operators
/// unwind through [`run`]'s guard rather than skipping it.
async fn run_turn(
    client: &crate::daemon::client::DaemonClient,
    args: &RunArgs,
    prompt: String,
    no_sandbox: bool,
) -> Result<i32> {
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let project_root = cwd.to_string_lossy().into_owned();

    // Attach a fresh session. `no_sandbox` (sandboxing part 2) makes this
    // noninteractive session start unsandboxed unless the daemon was
    // launched `--no-sandbox` (which wins).
    let attached = client
        .request_ok(Request::Attach {
            session_id: None,
            project_root: Some(project_root),
            no_sandbox,
        })
        .await?;
    let session_id = match attached {
        Response::Attached { session_id, .. } => session_id,
        other => anyhow::bail!("unexpected attach response: {other:?}"),
    };

    // Send the user message.
    client
        .request_ok(Request::SendUserMessage {
            text: prompt,
            images: Vec::new(),
        })
        .await
        .context("sending user message")?;

    // Pump events until the turn completes (or the session ends).
    pump_events(client, session_id, args.format).await
}

/// Spawn a task that fires the guard's synchronous shutdown on
/// SIGINT/SIGTERM. Returns `None` when there's no guard (attached to a
/// persistent daemon) — there's nothing to reap.
fn spawn_signal_shutdown(
    guard: Option<&EphemeralDaemonGuard>,
) -> Option<tokio::task::JoinHandle<()>> {
    let guard = guard?;
    let armed = guard.armed.clone();
    let socket = guard.socket.clone();
    Some(tokio::spawn(async move {
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
        if armed.swap(false, Ordering::SeqCst) {
            stop_daemon_blocking(&socket);
        }
        // After reaping, exit the foreground promptly — the user asked
        // us to stop. The daemon is already (being) torn down.
        std::process::exit(130);
    }))
}

fn build_prompt(args: &RunArgs) -> Result<String> {
    let argv = args.message.join(" ");
    let mut prompt = argv;

    if !std::io::stdin().is_terminal() {
        let mut stdin_buf = String::new();
        std::io::stdin()
            .read_to_string(&mut stdin_buf)
            .context("reading stdin")?;
        if !stdin_buf.trim().is_empty() {
            if !prompt.is_empty() {
                prompt.push_str("\n\n");
            }
            prompt.push_str(stdin_buf.trim_end());
        }
    }

    Ok(prompt)
}

async fn pump_events(
    client: &crate::daemon::client::DaemonClient,
    session_id: uuid::Uuid,
    format: OutputFormat,
) -> Result<i32> {
    let mut stdout = std::io::stdout().lock();
    let mut error_seen = false;

    while let Some(event) = client.next_event().await {
        // Filter to this session's events.
        if event_session(&event) != Some(session_id) {
            continue;
        }

        match format {
            OutputFormat::Default => match &event {
                proto::Event::AssistantTextDelta { delta, .. } => {
                    let _ = stdout.write_all(delta.as_bytes());
                    let _ = stdout.flush();
                }
                proto::Event::ToolError { tool, error, .. } => {
                    error_seen = true;
                    let _ = writeln!(stdout, "\n[error: {tool}: {error}]");
                }
                proto::Event::SessionEnded { reason, .. } => {
                    let _ = writeln!(stdout, "\n[session ended: {reason}]");
                    break;
                }
                _ => {}
            },
            OutputFormat::Json => {
                let env = proto::Envelope::event(event.clone());
                if let Ok(line) = serde_json::to_string(&env) {
                    let _ = writeln!(stdout, "{line}");
                }
            }
        }

        if matches!(event, proto::Event::SessionEnded { .. }) {
            break;
        }

        // Heuristic end-of-turn for the default `cockpit run` path:
        // we treat a final-`AssistantText` from the root agent as the
        // terminal event. The engine emits one per turn after the
        // streaming deltas finish; for a no-tool-calls turn it's the
        // only assistant output and we exit immediately. For a
        // tool-using turn it's emitted *between* tool round-trips, so
        // we keep going until no more events arrive on a short timer.
        if matches!(event, proto::Event::AssistantText { .. }) {
            // Drain for up to 100ms; if nothing else arrives the turn
            // is complete and we exit.
            let drained = drain_until_quiet(client, std::time::Duration::from_millis(100)).await;
            if drained.is_empty() {
                break;
            }
            // Otherwise more events came in: re-render them and keep
            // pumping. We bail out of this loop and re-enter the outer
            // pump by sequence — easier to do with a goto-style flag,
            // but for v1 we just continue: the events we just drained
            // are lost unless we handled them inline. So we render
            // them inline now to avoid losing them.
            for ev in &drained {
                if let proto::Event::AssistantTextDelta { delta, .. } = ev {
                    let _ = stdout.write_all(delta.as_bytes());
                    let _ = stdout.flush();
                }
            }
        }
    }

    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();

    Ok(if error_seen { 3 } else { 0 })
}

/// Receive events until `quiet` elapses without any new event. Used by
/// the default path to detect "the turn is settled."
async fn drain_until_quiet(
    client: &crate::daemon::client::DaemonClient,
    quiet: std::time::Duration,
) -> Vec<proto::Event> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(quiet, client.next_event()).await {
            Ok(Some(ev)) => out.push(ev),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    out
}

fn event_session(event: &proto::Event) -> Option<uuid::Uuid> {
    use proto::Event::*;
    Some(match event {
        ThinkingStarted { session_id, .. }
        | Reconnecting { session_id, .. }
        | AssistantTextDelta { session_id, .. }
        | ReasoningDelta { session_id, .. }
        | AssistantText { session_id, .. }
        | ToolStart { session_id, .. }
        | ToolEnd { session_id, .. }
        | ToolError { session_id, .. }
        | SubagentSpawned { session_id, .. }
        | SubagentReport { session_id, .. }
        | Usage { session_id, .. }
        | InterruptRaised { session_id, .. }
        | InterruptResolved { session_id, .. }
        | AgentIdle { session_id, .. }
        | SessionEnded { session_id, .. }
        | JobStarted { session_id, .. }
        | JobProgress { session_id, .. }
        | JobNote { session_id, .. }
        | JobCompleted { session_id, .. }
        | ContextProjection { session_id, .. }
        | Pruned { session_id, .. }
        | CompactReady { session_id, .. }
        | SandboxState { session_id, .. } => *session_id,
        // Daemon-global event (no session_id) — irrelevant to a headless
        // one-shot run, so it's filtered out by the session check.
        CaffeinateState { .. } => return None,
    })
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::daemon::proto::Body;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::UnixListener;

    /// Accept one connection on `socket`, read the first NDJSON line, and
    /// return it. Models the daemon's read side closely enough to assert
    /// the guard's synchronous `StopDaemon` actually lands on the wire.
    async fn accept_one_line(listener: UnixListener) -> Option<String> {
        let (stream, _) = listener.accept().await.ok()?;
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(n) if n > 0 => Some(line),
            _ => None,
        }
    }

    fn parse_request(line: &str) -> Request {
        let env: Envelope = serde_json::from_str(line.trim_end()).expect("valid envelope");
        match env.body {
            Body::Request { request, .. } => request,
            other => panic!("expected a request envelope, got {other:?}"),
        }
    }

    /// Layer A: dropping the guard (the path taken on an early `?` return
    /// or an unwind) sends a `StopDaemon` request to the daemon socket.
    #[tokio::test]
    async fn guard_drop_sends_stop_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(accept_one_line(listener));

        // Build then immediately drop the guard, off the runtime thread
        // (the real drop fires from sync `Drop`).
        let socket_for_guard = socket.clone();
        tokio::task::spawn_blocking(move || {
            let guard = EphemeralDaemonGuard::new(socket_for_guard);
            drop(guard);
        })
        .await
        .unwrap();

        let line = tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("server timed out")
            .unwrap()
            .expect("a line arrived");
        assert!(matches!(parse_request(&line), Request::StopDaemon));
    }

    /// Layer A: an explicit `shutdown()` (the happy path) disarms the
    /// guard, so the subsequent drop is a no-op and only one `StopDaemon`
    /// is ever sent. The daemon socket receives exactly one line.
    #[tokio::test]
    async fn guard_shutdown_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(accept_one_line(listener));

        let socket_for_guard = socket.clone();
        tokio::task::spawn_blocking(move || {
            let guard = EphemeralDaemonGuard::new(socket_for_guard);
            assert!(guard.armed.load(Ordering::SeqCst));
            guard.shutdown();
            // Disarmed: the second call and the drop must both be no-ops.
            assert!(!guard.armed.load(Ordering::SeqCst));
            guard.shutdown();
            drop(guard);
        })
        .await
        .unwrap();

        // The one-and-only request landed.
        let line = tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("server timed out")
            .unwrap()
            .expect("a line arrived");
        assert!(matches!(parse_request(&line), Request::StopDaemon));
    }
}
