//! [`Agent`] — one role-specialized conversational actor.
//!
//! An `Agent` bundles:
//!   - `name`        — `orchestrator-build`, `coder`, etc. Shown in the
//!     TUI active-agent slot (GOALS §1a).
//!   - `system`      — the role-specific system prompt.
//!   - `tools`       — a [`ToolBox`] of tools this agent is allowed to
//!     invoke. The orchestrator and the coder share an engine but have
//!     completely different tool surfaces.
//!   - `model`       — provider-side completion model. May be shared
//!     across agents via `Arc`.
//!
//! The agent loop ([`turn`]) is *one* model call plus the dispatch of
//! any tool calls it requested. The outer multi-turn orchestration
//! (loop until no more tool calls, switch agents on `task`, etc.) lives
//! in [`crate::engine::driver`].

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::engine::message::{
    Message, ToolCall, collect_tool_calls, extract_text, tool_result_message,
};
use crate::engine::model::{Model, ModelParams};
use crate::engine::repair::{Recovery, repair};
use crate::engine::tool::{ToolBox, ToolCtx, ToolOutput};
use crate::session::{Session, ToolCallRow};

/// One built-in or user-defined agent.
pub struct Agent {
    pub name: String,
    pub system: String,
    pub tools: ToolBox,
    pub model: Arc<Model>,
    pub params: ModelParams,
    /// Which argument fields the repair catalog should consider as
    /// `array<string>` for the `wrap_bare_string` repair. v0 tools all
    /// take object args with no array fields; this is a forward-looking
    /// knob.
    pub array_fields: Vec<&'static str>,
}

/// Events the agent emits during a turn. The driver forwards these to
/// the TUI for display; the persistence layer can subscribe to the
/// same channel.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// Agent emitted prose. v0 sends one event per turn (non-streaming);
    /// the field will become per-token when streaming lands.
    AssistantText { agent: String, text: String },
    /// A tool call started. `args` are post-repair.
    ToolStart {
        agent: String,
        call_id: String,
        tool: String,
        args: Value,
    },
    /// Tool finished. `output` is what the model will see next turn.
    ToolEnd {
        agent: String,
        call_id: String,
        tool: String,
        output: String,
        truncated: bool,
    },
    /// A tool errored. The model will see this string as the tool
    /// result; the TUI renders it red.
    ToolError {
        agent: String,
        call_id: String,
        tool: String,
        error: String,
    },
    /// `task` invoked a subagent; primary handoff (GOALS §3b) starts.
    /// Driver handles the actual stack push.
    SubagentSpawned {
        parent: String,
        child: String,
        prompt: String,
    },
    /// A subagent's final text. Delivered back to the parent as the
    /// tool result for its outstanding `task` call.
    SubagentReport { agent: String, report: String },
}

/// Outcome of one [`turn`] call. The driver loops on the result.
#[derive(Debug)]
pub enum TurnOutcome {
    /// Agent produced text and no tool calls — its turn is done.
    Done,
    /// Agent produced one or more tool calls; the loop must run another
    /// turn so the model can react to the results.
    Continue,
    /// Agent invoked `task`; the driver must push a subagent.
    SpawnSubagent {
        /// Which built-in agent name to spawn.
        child_agent: String,
        /// The brief to give it.
        prompt: String,
        /// Outstanding tool-call id the driver must answer when the
        /// subagent finishes. `ToolCall.id` is `String`; `ToolCall.call_id`
        /// is `Option<String>` because some providers don't surface a
        /// distinct id and rig's `tool_result_with_call_id` accepts the
        /// pair shape.
        task_call_id: String,
        task_function_call_id: Option<String>,
    },
}

/// Drive one round-trip with the model + dispatch any tool calls. The
/// `history` buffer is mutated in place: the user message (if any) was
/// pushed by the caller; this function appends the assistant turn and
/// every tool-result message in order.
pub async fn turn(
    agent: &Agent,
    history: &mut Vec<Message>,
    prompt: Message,
    session: Arc<Session>,
    locks: Arc<crate::locks::LockManager>,
    cwd: std::path::PathBuf,
    tx: &mpsc::Sender<TurnEvent>,
) -> Result<TurnOutcome> {
    let tools = agent.tools.definitions();
    let (msg_id, choice) = agent
        .model
        .complete(&agent.system, history, prompt.clone(), &tools, agent.params.clone())
        .await
        .with_context(|| format!("completion call for agent `{}`", agent.name))?;

    // Persist the assistant turn.
    history.push(prompt);
    history.push(Message::Assistant {
        id: msg_id.clone(),
        content: choice.clone(),
    });

    let text = extract_text(&choice);
    if !text.trim().is_empty() {
        let _ = tx
            .send(TurnEvent::AssistantText {
                agent: agent.name.clone(),
                text: text.clone(),
            })
            .await;
    }

    let calls: Vec<ToolCall> = collect_tool_calls(&choice);
    if calls.is_empty() {
        return Ok(TurnOutcome::Done);
    }

    // Tool dispatch.
    let ctx = ToolCtx {
        agent_id: agent.name.clone(),
        locks,
        session: session.clone(),
        cwd,
    };

    for tc in &calls {
        // `task` is special — it's a structural tool the driver, not
        // this loop, has to handle. We return early so the driver can
        // push a subagent before the rest of the calls fire. Any tool
        // calls the model emitted after `task` in the same turn are
        // dropped — the model will re-emit them after the subagent
        // returns, which keeps the conversation cleaner than queuing
        // them across a subagent boundary.
        if tc.function.name == "task" {
            let prompt = tc
                .function
                .arguments
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let child = tc
                .function
                .arguments
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or("coder")
                .to_string();
            let _ = tx
                .send(TurnEvent::SubagentSpawned {
                    parent: agent.name.clone(),
                    child: child.clone(),
                    prompt: prompt.clone(),
                })
                .await;
            return Ok(TurnOutcome::SpawnSubagent {
                child_agent: child,
                prompt,
                task_call_id: tc.id.clone(),
                task_function_call_id: tc.call_id.clone(),
            });
        }

        let start = Instant::now();
        let mut args = tc.function.arguments.clone();
        let original = args.clone();
        let recovery = repair(&mut args, &agent.array_fields);

        let _ = tx
            .send(TurnEvent::ToolStart {
                agent: agent.name.clone(),
                call_id: tc.id.clone(),
                tool: tc.function.name.clone(),
                args: args.clone(),
            })
            .await;

        let result = dispatch_one(&agent.tools, &tc.function.name, args.clone(), &ctx).await;

        let (output_str, hard_fail) = match &result {
            Ok(ToolOutput { content, .. }) => (content.clone(), false),
            Err(e) => {
                let msg = format!("Error: {e}");
                let _ = tx
                    .send(TurnEvent::ToolError {
                        agent: agent.name.clone(),
                        call_id: tc.id.clone(),
                        tool: tc.function.name.clone(),
                        error: msg.clone(),
                    })
                    .await;
                (msg, true)
            }
        };

        let truncated = matches!(&result, Ok(ToolOutput { truncated: true, .. }));
        if !hard_fail {
            let _ = tx
                .send(TurnEvent::ToolEnd {
                    agent: agent.name.clone(),
                    call_id: tc.id.clone(),
                    tool: tc.function.name.clone(),
                    output: output_str.clone(),
                    truncated,
                })
                .await;
        }

        // Persist the audit row. v0 stores the original AND a wire form
        // that's equal to the original; §13c canonical-form rewrite
        // will diverge them when implemented.
        session.record_tool_call(ToolCallRow {
            event_id: Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            agent: agent.name.clone(),
            tool: tc.function.name.clone(),
            path: args
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string),
            original_input_json: original,
            wire_input_json: args,
            recovery,
            hard_fail,
            duration_ms: start.elapsed().as_millis() as u64,
        });

        history.push(tool_result_message(tc, output_str));
    }

    Ok(TurnOutcome::Continue)
}

async fn dispatch_one(
    tools: &ToolBox,
    name: &str,
    args: Value,
    ctx: &ToolCtx,
) -> Result<ToolOutput> {
    let tool = tools
        .get(name)
        .with_context(|| format!("unknown tool `{name}`"))?;
    tool.call(args, ctx).await
}

/// Allow `Recovery` to flow into telemetry plumbing without exposing the
/// enum's name to every caller.
pub fn recovery_db_fields(r: &Recovery) -> (Option<&'static str>, Option<&'static str>) {
    r.db_fields()
}
