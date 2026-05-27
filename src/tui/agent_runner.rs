//! TUI ↔ daemon glue.
//!
//! Phase 4 of the daemon migration: the TUI no longer owns the
//! engine. Instead [`try_spawn`] probes (or auto-promotes) the daemon
//! via [`crate::daemon::client`], attaches a session at the cwd, and
//! pipes the per-tick event stream from the daemon's broadcast back
//! to the TUI in the same `Arc<Mutex<Vec<TurnEvent>>>` shape the rest
//! of `app.rs` already consumes. The wire-shape of events is
//! [`crate::daemon::proto::Event`]; we translate to [`TurnEvent`] at
//! the boundary so the TUI rendering paths don't need to know they
//! talk to a daemon.

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::daemon::client::{LifecycleMode, probe_or_spawn};
use crate::daemon::proto::{self, Request, Response};
use crate::engine::TurnEvent;

/// Handle the TUI keeps to talk to the engine (now via the daemon).
pub struct AgentRunner {
    /// Send user-typed messages here. Each line becomes one
    /// `SendUserMessage` request; the daemon's queue-folding (GOALS
    /// §1c) is performed inside the worker, not here.
    pub input_tx: mpsc::Sender<String>,
    /// Drained per tick into [`crate::tui::app::App::history`].
    pub events: Arc<Mutex<Vec<TurnEvent>>>,
    /// Name of whoever's currently on top of the agent stack. The
    /// chrome reads this for the active-agent slot (GOALS §1a).
    pub active_agent: Arc<Mutex<String>>,
}

/// Probe for the daemon (auto-promoting one if needed), attach a
/// fresh session at `cwd`, and return the runner handle.
///
/// Returns `Err(String)` instead of `anyhow::Error` so `app.rs` can
/// render the message in its fallback "input captured" stub without
/// having to format an anyhow chain.
pub fn try_spawn(cwd: &Path) -> Result<AgentRunner, String> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| "no tokio runtime — cockpit must be invoked from main".to_string())?;

    // probe_or_spawn is async; we block the (async) caller on it so
    // try_spawn returns a fully-attached handle to the TUI. We're
    // already in a tokio context (`main` is `#[tokio::main]`), so we
    // use `block_in_place` to run a `block_on` without panicking.
    let attached = tokio::task::block_in_place(|| {
        runtime.block_on(async {
            let daemon = probe_or_spawn(LifecycleMode::AttachOrAutoPromote)
                .await
                .map_err(|e| format!("daemon probe: {e}"))?;
            // Push our env into the daemon before attaching so any vars
            // the user added to their shell rc since the daemon was
            // spawned (API keys, COPILOT_API_URL, etc.) become visible
            // to the next inference call. Fire-and-forget — a daemon
            // that doesn't yet speak `RefreshEnv` just errors back, and
            // we proceed as before.
            let env: std::collections::HashMap<String, String> = std::env::vars().collect();
            let _ = daemon
                .client
                .request(Request::RefreshEnv { vars: env })
                .await;
            let project_root = cwd.to_string_lossy().into_owned();
            let attached = daemon
                .client
                .request_ok(Request::Attach {
                    session_id: None,
                    project_root: Some(project_root),
                })
                .await
                .map_err(|e| format!("attach: {e}"))?;
            let (session_id, active_agent_name) = match attached {
                Response::Attached {
                    session_id,
                    active_agent,
                    ..
                } => (session_id, active_agent),
                other => return Err(format!("unexpected attach response: {other:?}")),
            };
            Ok::<_, String>((daemon.client, session_id, active_agent_name))
        })
    })?;
    let (client, session_id, initial_active_agent) = attached;

    let (input_tx, mut input_rx) = mpsc::channel::<String>(32);
    let events = Arc::new(Mutex::new(Vec::new()));
    let active_agent = Arc::new(Mutex::new(initial_active_agent));

    // Outbound: TUI sends a line → forward to daemon as
    // SendUserMessage.
    {
        let client = client.clone();
        tokio::spawn(async move {
            while let Some(text) = input_rx.recv().await {
                if let Err(e) = client.request(Request::SendUserMessage { text }).await {
                    tracing::warn!(error = ?e, "send_user_message transport failed");
                    break;
                }
            }
        });
    }

    // Inbound: daemon events → translate → push into the shared
    // buffer and update active-agent tracker.
    {
        let events = events.clone();
        let active_agent = active_agent.clone();
        let client = client.clone();
        tokio::spawn(async move {
            while let Some(event) = client.next_event().await {
                if event_session(&event) != Some(session_id) {
                    continue;
                }
                update_active_agent(&event, &active_agent);
                if let Some(translated) = proto_event_to_turn_event(event) {
                    events.lock().unwrap().push(translated);
                }
            }
        });
    }

    Ok(AgentRunner {
        input_tx,
        events,
        active_agent,
    })
}

fn update_active_agent(event: &proto::Event, slot: &Arc<Mutex<String>>) {
    match event {
        proto::Event::SubagentSpawned { child, .. } => {
            *slot.lock().unwrap() = child.clone();
        }
        proto::Event::SubagentReport { .. } => {
            // Pop back to the root. v1 supports a depth-1 stack
            // (orchestrator-build → coder | explore); deeper trees
            // need a proper stack to track properly.
            *slot.lock().unwrap() = "orchestrator-build".to_string();
        }
        _ => {}
    }
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
        | Usage { session_id, .. }
        | InterruptRaised { session_id, .. }
        | InterruptResolved { session_id, .. }
        | SessionEnded { session_id, .. } => *session_id,
    })
}

fn proto_event_to_turn_event(event: proto::Event) -> Option<TurnEvent> {
    use proto::Event::*;
    Some(match event {
        ThinkingStarted { agent, .. } => TurnEvent::ThinkingStarted { agent },
        AssistantTextDelta { agent, delta, .. } => TurnEvent::AssistantTextDelta { agent, delta },
        ReasoningDelta { agent, delta, .. } => TurnEvent::ReasoningDelta { agent, delta },
        AssistantText { agent, text, .. } => TurnEvent::AssistantText { agent, text },
        ToolStart {
            agent,
            call_id,
            tool,
            args,
            ..
        } => TurnEvent::ToolStart {
            agent,
            call_id,
            tool,
            args,
        },
        ToolEnd {
            agent,
            call_id,
            tool,
            output,
            truncated,
            ..
        } => TurnEvent::ToolEnd {
            agent,
            call_id,
            tool,
            output,
            truncated,
        },
        ToolError {
            agent,
            call_id,
            tool,
            error,
            ..
        } => TurnEvent::ToolError {
            agent,
            call_id,
            tool,
            error,
        },
        SubagentSpawned {
            parent,
            child,
            prompt,
            ..
        } => TurnEvent::SubagentSpawned {
            parent,
            child,
            prompt,
        },
        SubagentReport { agent, report, .. } => TurnEvent::SubagentReport { agent, report },
        Usage {
            agent,
            input_tokens,
            output_tokens,
            cached_input_tokens,
            ..
        } => TurnEvent::Usage {
            agent,
            usage: crate::tokens::TokenUsage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
            },
        },
        // Interrupts and SessionEnded don't have TurnEvent analogues
        // yet — the TUI's needs-attention surface lands with the
        // approval router.
        InterruptRaised { .. } | InterruptResolved { .. } | SessionEnded { .. } => return None,
    })
}

/// One-line summary of a tool call's args for the `→ tool(...)`
/// affordance the TUI renders. Public so [`crate::tui::app`] can
/// reuse it when projecting [`TurnEvent::ToolStart`] into history.
pub fn short_args(v: &serde_json::Value) -> String {
    if let Some(map) = v.as_object() {
        let mut out = String::new();
        for (k, val) in map {
            if !out.is_empty() {
                out.push_str(", ");
            }
            let rendered = match val {
                serde_json::Value::String(s) if s.len() <= 40 => format!("{k}=\"{s}\""),
                serde_json::Value::String(s) => format!("{k}=<{}c>", s.len()),
                serde_json::Value::Bool(b) => format!("{k}={b}"),
                serde_json::Value::Number(n) => format!("{k}={n}"),
                other => format!(
                    "{k}={}",
                    other.to_string().chars().take(40).collect::<String>()
                ),
            };
            out.push_str(&rendered);
            if out.chars().count() > 80 {
                out.push('…');
                break;
            }
        }
        out
    } else {
        v.to_string()
    }
}

/// First non-empty trimmed line of `s`, capped at `max_chars`. Used
/// for tool-output snippets and subagent prompt previews.
pub fn first_line(s: &str, max_chars: usize) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.chars().count() > max_chars {
        let truncated: String = first.chars().take(max_chars).collect();
        format!("{truncated}…")
    } else {
        first.to_string()
    }
}
