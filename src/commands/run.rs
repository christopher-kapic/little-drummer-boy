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

use anyhow::{Context, Result};

use crate::cli::{OutputFormat, RunArgs};
use crate::daemon::client::{LifecycleMode, probe_or_spawn};
use crate::daemon::ephemeral_guard::{EphemeralDaemonGuard, spawn_signal_shutdown};
use crate::daemon::proto::{self, Request, Response};

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
    let signal_task = spawn_signal_shutdown(guard.as_ref(), true);

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
    // Plan-run metric attribution (`plan-run-metrics`): when the plan executor
    // spawns this coder for a step, it passes `--plan-id`/`--step-id` (clap
    // requires them together); stamp the session so every inference call rolls
    // up per plan/step. Malformed ids degrade to no attribution rather than
    // failing the run — the executor always supplies well-formed uuids.
    let plan_context = match (args.plan_id.as_deref(), args.step_id.as_deref()) {
        (Some(p), Some(s)) => match (uuid::Uuid::parse_str(p), uuid::Uuid::parse_str(s)) {
            (Ok(p), Ok(s)) => Some((p, s)),
            _ => None,
        },
        _ => None,
    };
    attach_send_pump(
        client,
        prompt,
        no_sandbox,
        args.format,
        args.model.as_deref(),
        plan_context,
    )
    .await
}

/// Attach a fresh headless session, send `prompt`, and pump events to
/// completion, returning the run exit code. Shared by `cockpit run` and
/// `cockpit init` so both drive the identical non-interactive turn over
/// the daemon. The caller owns the daemon lifecycle (probe/spawn +
/// ephemeral guard).
pub(crate) async fn attach_send_pump(
    client: &crate::daemon::client::DaemonClient,
    prompt: String,
    no_sandbox: bool,
    format: OutputFormat,
    model_override: Option<&str>,
    plan_context: Option<(uuid::Uuid, uuid::Uuid)>,
) -> Result<i32> {
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let project_root = cwd.to_string_lossy().into_owned();

    // Attach a fresh session. `no_sandbox` (sandboxing part 2) makes this
    // noninteractive session start unsandboxed unless the daemon was
    // launched `--no-sandbox` (which wins). `model_override` (`--model`, the
    // plan executor passes the plan's pinned model) overrides every spawned
    // agent's frontmatter model for this session's run.
    let attached = client
        .request_ok(Request::Attach {
            session_id: None,
            project_root: Some(project_root),
            no_sandbox,
            // A streamed run has no UI to answer an interrupt — a
            // non-interactive attach. The loop guard treats the session as
            // headless and auto-rejects a back-to-back repeat (with the
            // guidance error) rather than blocking.
            interactive: false,
            model_override: model_override.map(str::to_string),
            plan_context,
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
    pump_events(client, session_id, format).await
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
        | PrimarySwapped { session_id, .. }
        | LlmModeChanged { session_id, .. }
        | SessionEnded { session_id, .. }
        | JobStarted { session_id, .. }
        | JobProgress { session_id, .. }
        | JobNote { session_id, .. }
        | JobCompleted { session_id, .. }
        | ContextProjection { session_id, .. }
        | Pruned { session_id, .. }
        | CompactReady { session_id, .. }
        | SandboxState { session_id, .. } => *session_id,
        // Daemon-global events (no session_id) — irrelevant to a headless
        // one-shot run, so they're filtered out by the session check.
        CaffeinateState { .. } | DaemonDraining { .. } => return None,
    })
}
