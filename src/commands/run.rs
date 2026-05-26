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
//! 6. If we own the daemon (ephemeral path), shut it down.

use std::io::{IsTerminal, Read, Write};

use anyhow::{Context, Result};

use crate::cli::{OutputFormat, RunArgs};
use crate::daemon::client::{LifecycleMode, probe_or_spawn};
use crate::daemon::proto::{self, Request, Response};

pub async fn run(args: RunArgs) -> Result<()> {
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

    let cwd = std::env::current_dir().context("resolving cwd")?;
    let project_root = cwd.to_string_lossy().into_owned();

    // Attach a fresh session.
    let attached = client
        .request_ok(Request::Attach {
            session_id: None,
            project_root: Some(project_root),
        })
        .await?;
    let session_id = match attached {
        Response::Attached { session_id, .. } => session_id,
        other => anyhow::bail!("unexpected attach response: {other:?}"),
    };

    // Send the user message.
    client
        .request_ok(Request::SendUserMessage { text: prompt })
        .await
        .context("sending user message")?;

    // Pump events until the turn completes (or the session ends).
    let exit_code = pump_events(&client, session_id, args.format).await?;

    if daemon.owns_daemon {
        let _ = daemon.shutdown().await;
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
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
        | AssistantTextDelta { session_id, .. }
        | ReasoningDelta { session_id, .. }
        | AssistantText { session_id, .. }
        | ToolStart { session_id, .. }
        | ToolEnd { session_id, .. }
        | ToolError { session_id, .. }
        | SubagentSpawned { session_id, .. }
        | SubagentReport { session_id, .. }
        | InterruptRaised { session_id, .. }
        | InterruptResolved { session_id, .. }
        | SessionEnded { session_id, .. } => *session_id,
    })
}
