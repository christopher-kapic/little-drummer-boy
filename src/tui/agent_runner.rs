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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::daemon::client::{LifecycleMode, probe_or_spawn};
use crate::daemon::proto::{self, Request, Response};
use crate::engine::TurnEvent;

/// The three 30-day autocomplete count maps fetched at session start.
/// `models` and `slash` are global; `tags` is scoped to this session's
/// project. Empty when the daemon predates `GetUsageCounts`.
#[derive(Default, Clone)]
pub struct UsageCounts {
    pub models: HashMap<String, u64>,
    pub slash: HashMap<String, u64>,
    pub tags: HashMap<String, u64>,
}

/// Handle the TUI keeps to talk to the engine (now via the daemon).
pub struct AgentRunner {
    /// Send user submissions here (text + any pasted image parts). Each
    /// becomes one `SendUserMessage` request; the daemon's queue-folding
    /// (GOALS §1c) is performed inside the worker, not here.
    pub input_tx: mpsc::Sender<crate::engine::message::UserSubmission>,
    /// Fire-and-forget `RecordUsage` requests (autocomplete tally).
    pub record_tx: mpsc::Sender<Request>,
    /// Drained per tick into [`crate::tui::app::App::history`].
    pub events: Arc<Mutex<Vec<TurnEvent>>>,
    /// Name of whoever's currently on top of the agent stack. The
    /// chrome reads this for the active-agent slot (GOALS §1a).
    pub active_agent: Arc<Mutex<String>>,
    /// This session's full id. Shown in the startup graphic and printed on
    /// exit (session-id-display-and-lazy-persist). Assigned by the daemon at
    /// attach, before the `sessions` row is persisted.
    pub session_id: uuid::Uuid,
    /// This session's 6-char display id (GOALS §17b). The TUI captures
    /// it as the predecessor short-id when this session spawns a
    /// `/compact` handoff, so the fresh session can draw a "compacted
    /// from <short-id>" boundary marker.
    pub short_id: String,
    /// This session's project id — the scope for `tag` usage records.
    pub project_id: String,
    /// Frequency counts fetched at attach; the TUI seeds its in-memory
    /// maps from these once.
    pub usage: UsageCounts,
    /// `true` when this TUI *spawned* the daemon it's attached to (the
    /// daemonless `AlwaysEphemeral` path) and therefore owns its teardown
    /// — the app builds an [`crate::daemon::ephemeral_guard::EphemeralDaemonGuard`]
    /// from this. `false` when it attached to a pre-existing (canonical or
    /// auto-promoted persistent) daemon, which it must never stop.
    pub owns_daemon: bool,
    /// The socket of the daemon this runner is attached to. Carried so an
    /// owned ephemeral daemon can be reaped on exit via the guard.
    pub socket: PathBuf,
}

/// Probe for the daemon (auto-promoting one if needed), attach a
/// fresh session at `cwd`, and return the runner handle.
///
/// Returns `Err(String)` instead of `anyhow::Error` so `app.rs` can
/// render the message in its fallback "input captured" stub without
/// having to format an anyhow chain.
pub fn try_spawn(cwd: &Path, no_sandbox: bool, mode: LifecycleMode) -> Result<AgentRunner, String> {
    try_spawn_inner(cwd, None, no_sandbox, mode)
}

/// Re-attach to an existing session by id (the `/compact` commit path,
/// T6.e). Same as [`try_spawn`] but resumes `session_id` instead of
/// creating a fresh one, so the TUI switches its event stream + input
/// channel onto the new compaction-handoff session. `no_sandbox` is
/// ignored by the daemon on resume (the session keeps its own state),
/// passed only to keep the attach shape uniform.
pub fn attach_to_session(
    cwd: &Path,
    session_id: uuid::Uuid,
    no_sandbox: bool,
    mode: LifecycleMode,
) -> Result<AgentRunner, String> {
    try_spawn_inner(cwd, Some(session_id), no_sandbox, mode)
}

fn try_spawn_inner(
    cwd: &Path,
    session_id: Option<uuid::Uuid>,
    no_sandbox: bool,
    mode: LifecycleMode,
) -> Result<AgentRunner, String> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| "no tokio runtime — cockpit must be invoked from main".to_string())?;

    // probe_or_spawn is async; we block the (async) caller on it so
    // try_spawn returns a fully-attached handle to the TUI. We're
    // already in a tokio context (`main` is `#[tokio::main]`), so we
    // use `block_in_place` to run a `block_on` without panicking.
    let attached = tokio::task::block_in_place(|| {
        runtime.block_on(async {
            let daemon = probe_or_spawn(mode)
                .await
                .map_err(|e| format!("daemon probe: {e}"))?;
            let owns_daemon = daemon.owns_daemon;
            let socket = daemon.socket.clone();
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
                    session_id,
                    project_root: Some(project_root),
                    no_sandbox,
                    // The TUI can answer interrupts (approval / loop-guard /
                    // `question` prompts) — mark this attach interactive so
                    // the loop guard prompts here instead of auto-rejecting.
                    interactive: true,
                })
                .await
                .map_err(|e| format!("attach: {e}"))?;
            let (session_id, short_id, active_agent_name, project_id) = match attached {
                Response::Attached {
                    session_id,
                    short_id,
                    active_agent,
                    project_id,
                    ..
                } => (session_id, short_id, active_agent, project_id),
                other => return Err(format!("unexpected attach response: {other:?}")),
            };
            // Fetch the autocomplete frequency maps for this session's
            // project. Best-effort: a daemon that doesn't speak
            // `GetUsageCounts` just leaves the maps empty (no ranking).
            let usage = match daemon
                .client
                .request_ok(Request::GetUsageCounts {
                    project_id: Some(project_id.clone()),
                })
                .await
            {
                Ok(Response::UsageCounts {
                    models,
                    slash,
                    tags,
                }) => UsageCounts {
                    models,
                    slash,
                    tags,
                },
                _ => UsageCounts::default(),
            };
            Ok::<_, String>((
                daemon.client,
                session_id,
                short_id,
                active_agent_name,
                project_id,
                usage,
                owns_daemon,
                socket,
            ))
        })
    })?;
    let (
        client,
        session_id,
        short_id,
        initial_active_agent,
        project_id,
        usage,
        owns_daemon,
        socket,
    ) = attached;

    let (input_tx, mut input_rx) = mpsc::channel::<crate::engine::message::UserSubmission>(32);
    let (record_tx, mut record_rx) = mpsc::channel::<Request>(32);
    let events = Arc::new(Mutex::new(Vec::new()));
    let active_agent = Arc::new(Mutex::new(initial_active_agent));

    // Outbound: TUI sends a submission (text + any image parts) → forward
    // to daemon as SendUserMessage.
    {
        let client = client.clone();
        tokio::spawn(async move {
            while let Some(sub) = input_rx.recv().await {
                if let Err(e) = client
                    .request(Request::SendUserMessage {
                        text: sub.text,
                        images: sub.images,
                    })
                    .await
                {
                    tracing::warn!(error = ?e, "send_user_message transport failed");
                    break;
                }
            }
        });
    }

    // Outbound: fire-and-forget autocomplete usage records.
    {
        let client = client.clone();
        tokio::spawn(async move {
            while let Some(req) = record_rx.recv().await {
                if let Err(e) = client.request(req).await {
                    tracing::warn!(error = ?e, "record_usage transport failed");
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
        // The current primary (root-frame) agent, tracked so a subagent pop
        // returns the active-agent slot to the right primary after a `/plan`
        // or `/build` swap (not a hardcoded `Build`). Seeded from the
        // attach-time active agent.
        let primary_agent = Arc::new(Mutex::new(active_agent.lock().unwrap().clone()));
        tokio::spawn(async move {
            while let Some(event) = client.next_event().await {
                // Daemon-global events (caffeinate) carry no session_id and
                // must reach this client regardless of which session it's
                // attached to — so they bypass the per-session filter.
                let is_global = matches!(event, proto::Event::CaffeinateState { .. });
                if !is_global && event_session(&event) != Some(session_id) {
                    continue;
                }
                update_active_agent(&event, &active_agent, &primary_agent);
                if let Some(translated) = proto_event_to_turn_event(event) {
                    events.lock().unwrap().push(translated);
                }
            }
        });
    }

    Ok(AgentRunner {
        input_tx,
        record_tx,
        events,
        active_agent,
        session_id,
        short_id,
        project_id,
        usage,
        owns_daemon,
        socket,
    })
}

/// Pre-flight sizing for the fresh-chat context indicator (Feature 1).
/// `file` is the basename of the matched guidance file (`None` when the
/// project has none); `guidance_tokens` is its body size (the `… in
/// <file>` label); `system_tokens` is the full composed system prompt
/// (role prompt + OS + session + guidance body), the baseline the
/// running context estimate folds in.
#[derive(Debug, Clone)]
pub struct GuidanceEstimate {
    pub file: Option<String>,
    pub guidance_tokens: u64,
    pub system_tokens: u64,
}

/// Resolve the fresh-chat sizing for `cwd` and the active model. Prefers
/// an already-running daemon's calibrated estimate (no attach, no spawn —
/// calling it at launch never creates a session); on any miss (no daemon,
/// connect/request error, or the daemon couldn't answer) it falls back to
/// a local raw-cl100k computation via [`crate::engine::builtin`]. The two
/// modes may differ by the calibration factor; each is the best available
/// for its mode. Best-effort and non-blocking for launch.
pub async fn fetch_guidance_estimate(
    cwd: &Path,
    provider: Option<String>,
    model: Option<String>,
) -> GuidanceEstimate {
    if let Some(est) = daemon_guidance_estimate(cwd, provider, model).await {
        return est;
    }
    local_guidance_estimate(cwd)
}

/// Ask an already-running daemon for the calibrated estimate. Returns
/// `None` on any failure (no daemon, transport error, or a malformed
/// response) so the caller can fall back to the local computation.
async fn daemon_guidance_estimate(
    cwd: &Path,
    provider: Option<String>,
    model: Option<String>,
) -> Option<GuidanceEstimate> {
    use crate::daemon::{DaemonPaths, DaemonStatus, probe};
    let paths = DaemonPaths::resolve().ok()?;
    if !matches!(probe(&paths).await, DaemonStatus::Running) {
        return None;
    }
    let client = crate::daemon::client::DaemonClient::connect(&paths.socket)
        .await
        .ok()?;
    let resp = client
        .request_ok(Request::GuidanceEstimate {
            project_root: cwd.to_string_lossy().into_owned(),
            provider,
            model,
        })
        .await
        .ok()?;
    match resp {
        Response::GuidanceEstimate {
            file,
            tokens,
            system_tokens,
        } => Some(GuidanceEstimate {
            file,
            guidance_tokens: tokens,
            system_tokens,
        }),
        _ => None,
    }
}

/// Daemonless fallback: size the guidance file body and the full composed
/// system prompt in-process with raw cl100k (`crate::tokens::count`).
/// Cheap and synchronous — `load_agent_guidance` only stats/reads one
/// small file along the cwd→git-root walk — so it never blocks launch.
fn local_guidance_estimate(cwd: &Path) -> GuidanceEstimate {
    let file = crate::engine::builtin::load_agent_guidance(cwd).map(|(path, body)| {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        (name, crate::tokens::count(&body) as u64)
    });
    // No session exists yet at the fresh-chat indicator, so the system
    // prompt omits the `Session:` line — matching what the engine sends.
    let system_prompt = crate::engine::builtin::default_chat_system_prompt(cwd, "");
    let system_tokens = crate::tokens::count(&system_prompt) as u64;
    match file {
        Some((name, guidance_tokens)) => GuidanceEstimate {
            file: Some(name),
            guidance_tokens,
            system_tokens,
        },
        None => GuidanceEstimate {
            file: None,
            guidance_tokens: 0,
            system_tokens,
        },
    }
}

/// Run one blocking daemon request against an already-running daemon and
/// return the typed response. Connects only — never spawns — so the
/// `/sessions` browser degrades gracefully (no live data, no DB writes,
/// no crash) when the daemon isn't up. Mirrors `try_spawn_inner`'s
/// `block_in_place` pattern so it's callable from the synchronous TUI
/// key handlers. `Err(String)` for any transport/typed failure.
pub fn daemon_request_blocking(req: Request) -> Result<Response, String> {
    use crate::daemon::{DaemonPaths, DaemonStatus, probe};
    let runtime =
        tokio::runtime::Handle::try_current().map_err(|_| "no tokio runtime".to_string())?;
    tokio::task::block_in_place(|| {
        runtime.block_on(async {
            let paths = DaemonPaths::resolve().map_err(|e| format!("daemon paths: {e}"))?;
            if !matches!(probe(&paths).await, DaemonStatus::Running) {
                return Err("daemon not running".to_string());
            }
            let client = crate::daemon::client::DaemonClient::connect(&paths.socket)
                .await
                .map_err(|e| format!("daemon connect: {e}"))?;
            client
                .request_ok(req)
                .await
                .map_err(|e| format!("daemon request: {e}"))
        })
    })
}

/// List sessions for the `/sessions` browser. `project_id = Some(p)` +
/// `parent = None` → root sessions in `p`; `parent = Some(s)` → direct
/// forks of `s`; both `None` → every open session (all-projects scope).
pub fn list_sessions_blocking(
    project_id: Option<String>,
    parent_session_id: Option<uuid::Uuid>,
) -> Result<Vec<proto::SessionSummary>, String> {
    match daemon_request_blocking(Request::ListSessions {
        project_id,
        parent_session_id,
    })? {
        Response::Sessions { sessions } => Ok(sessions),
        other => Err(format!("unexpected list_sessions response: {other:?}")),
    }
}

/// List every plan (active first) for the `/plans` browser. Daemon down →
/// `Err(String)`; the pane renders it inline rather than refusing to open.
pub fn list_plans_blocking() -> Result<Vec<proto::PlanSummaryWire>, String> {
    match daemon_request_blocking(Request::ListPlans)? {
        Response::Plans { plans } => Ok(plans),
        other => Err(format!("unexpected list_plans response: {other:?}")),
    }
}

/// Fetch one plan's full detail (steps + dependency prerequisites + tests)
/// for the `/plans` drill-in. `Err(String)` on a daemon/transport failure
/// or an unknown plan id.
pub fn plan_detail_blocking(
    plan_id: uuid::Uuid,
) -> Result<(proto::PlanSummaryWire, Vec<proto::PlanStepWire>), String> {
    match daemon_request_blocking(Request::PlanDetail { plan_id })? {
        Response::PlanDetail { plan, steps } => Ok((plan, steps)),
        other => Err(format!("unexpected plan_detail response: {other:?}")),
    }
}

/// Fetch live `(has_active_jobs, processing)` status for the candidate
/// session ids. Daemon down / no live worker → empty map; callers treat
/// absent ids as not-processing / no-jobs.
pub fn session_live_status_blocking(
    session_ids: Vec<uuid::Uuid>,
) -> std::collections::HashMap<uuid::Uuid, (bool, bool)> {
    match daemon_request_blocking(Request::SessionLiveStatus { session_ids }) {
        Ok(Response::SessionLiveStatus { statuses }) => statuses
            .into_iter()
            .map(|s| (s.session_id, (s.has_active_jobs, s.processing)))
            .collect(),
        _ => std::collections::HashMap::new(),
    }
}

fn update_active_agent(
    event: &proto::Event,
    slot: &Arc<Mutex<String>>,
    primary: &Arc<Mutex<String>>,
) {
    match event {
        proto::Event::PrimarySwapped { name, .. } => {
            // The root-frame primary changed (`/plan` ↔ `/build`). Track it
            // and, since a swap only happens at idle (no subagent on top),
            // reflect it in the live slot immediately.
            *primary.lock().unwrap() = name.clone();
            *slot.lock().unwrap() = name.clone();
        }
        proto::Event::SubagentSpawned { child, .. } => {
            *slot.lock().unwrap() = child.clone();
        }
        proto::Event::SubagentReport { .. } => {
            // Pop back to the current primary. v1 supports a depth-1 stack
            // (`Build`/`Plan` → one subagent); deeper trees need a proper
            // stack to track properly.
            *slot.lock().unwrap() = primary.lock().unwrap().clone();
        }
        _ => {}
    }
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
        | SessionEnded { session_id, .. }
        | JobStarted { session_id, .. }
        | JobProgress { session_id, .. }
        | JobNote { session_id, .. }
        | JobCompleted { session_id, .. }
        | ContextProjection { session_id, .. }
        | Pruned { session_id, .. }
        | CompactReady { session_id, .. }
        | SandboxState { session_id, .. } => *session_id,
        // Daemon-global events carry no session_id: they reach every
        // client regardless of attachment.
        CaffeinateState { .. } | DaemonDraining { .. } => return None,
    })
}

fn proto_event_to_turn_event(event: proto::Event) -> Option<TurnEvent> {
    use proto::Event::*;
    Some(match event {
        ThinkingStarted { agent, .. } => TurnEvent::ThinkingStarted { agent },
        Reconnecting { agent, attempt, .. } => TurnEvent::Reconnecting { agent, attempt },
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
            kind,
            ..
        } => TurnEvent::ToolError {
            agent,
            call_id,
            tool,
            error,
            kind,
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
        AgentIdle { .. } => TurnEvent::AgentIdle,
        JobStarted {
            session_id,
            job_id,
            label,
            kind,
        } => TurnEvent::JobStarted {
            session_id,
            job_id,
            label,
            kind,
        },
        JobProgress { job_id, .. } => TurnEvent::JobProgress { job_id },
        JobNote { job_id, text, .. } => TurnEvent::JobNote { job_id, text },
        JobCompleted {
            job_id,
            label,
            kind,
            failed,
            ..
        } => TurnEvent::JobCompleted {
            job_id,
            label,
            kind,
            failed,
        },
        ContextProjection {
            prunable_tokens,
            cache_cold,
            ..
        } => TurnEvent::ContextProjection {
            prunable_tokens,
            cache_cold,
        },
        Pruned {
            auto,
            bodies,
            tokens_saved,
            elided,
            ..
        } => TurnEvent::Pruned {
            auto,
            bodies,
            tokens_saved,
            elided,
        },
        CompactReady {
            new_session_id,
            handoff,
            seed_tool_count,
            seed_tool_tokens,
            ..
        } => TurnEvent::CompactReady {
            new_session_id,
            handoff,
            seed_tool_count,
            seed_tool_tokens,
        },
        // A question-tool interrupt (GOALS §3b) carries a question batch;
        // surface it so the TUI opens the answering dialog. A bare
        // `InterruptRaised` with no batch (the `jobs` needs-attention
        // nudge) has no dialog and stays a no-op here. `InterruptResolved`
        // / `SessionEnded` have no TurnEvent analogue.
        InterruptRaised {
            interrupt_id,
            description,
            questions: Some(questions),
            ..
        } => TurnEvent::InterruptRaised {
            interrupt_id,
            description,
            questions,
        },
        SandboxState { enabled, .. } => TurnEvent::SandboxState { enabled },
        CaffeinateState {
            active,
            lid_close_guaranteed,
            message,
        } => TurnEvent::CaffeinateState {
            active,
            lid_close_guaranteed,
            message,
        },
        DaemonDraining { forced } => TurnEvent::DaemonDraining { forced },
        InterruptRaised { .. } | InterruptResolved { .. } | SessionEnded { .. } => return None,
        // The chrome's active-agent slot is updated directly in
        // `update_active_agent`; the swap needs no history-stream entry.
        PrimarySwapped { .. } => return None,
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
