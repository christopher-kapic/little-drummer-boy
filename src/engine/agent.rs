//! [`Agent`] — one role-specialized conversational actor.
//!
//! An `Agent` bundles:
//!   - `name`        — `Build`, `coder`, etc. Shown in the
//!     TUI active-agent slot (GOALS §1a).
//!   - `system`      — the role-specific system prompt.
//!   - `tools`       — a [`ToolBox`] of tools this agent is allowed to
//!     invoke. The primary agent and the coder share an engine but have
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
use crate::engine::tool::invalid_input;
use crate::engine::tool::{ToolBox, ToolCtx, ToolOutput};
use crate::redact::RedactionTable;
use crate::session::{Session, ToolCallRow};

/// One built-in or user-defined agent.
pub struct Agent {
    pub name: String,
    pub system: String,
    pub tools: ToolBox,
    pub model: Arc<Model>,
    pub params: ModelParams,
}

/// Events the agent emits during a turn. The driver forwards these to
/// the TUI for display; the persistence layer can subscribe to the
/// same channel.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// Model inference started; nothing has been emitted yet. The TUI
    /// shows a "Thinking…" placeholder until the first text delta
    /// arrives. Fires once per round-trip; also fires before reasoning-
    /// mode models start emitting their reasoning chunks (which we
    /// currently drop — see [`crate::engine::model::Model::complete`]).
    ThinkingStarted { agent: String },
    /// An inference call failed with a network/transient error and is
    /// being auto-retried (GOALS network-retry). `attempt` is the 1-based
    /// retry number. The TUI shows a non-blocking `reconnecting… attempt
    /// N` status (no per-attempt toast spam); cleared by the next
    /// `ThinkingStarted` / `AssistantTextDelta` / `AgentIdle`.
    Reconnecting { agent: String, attempt: u32 },
    /// One streaming chunk of the assistant's text response. The TUI
    /// accumulates these in a live-rendered line.
    AssistantTextDelta { agent: String, delta: String },
    /// One streaming chunk of the model's *reasoning* (thinking-mode
    /// models only). The TUI hides this by default — the
    /// "Thinking…" placeholder is the visible affordance — but
    /// captures it so the user can expand a thinking block later to
    /// inspect the chain of thought.
    ReasoningDelta { agent: String, delta: String },
    /// Assistant turn's text is complete. Emitted right after the
    /// stream finishes (or, in non-streaming mode, after the response
    /// returns). `text` is the full accumulated text; the TUI uses
    /// this as a "finalize the streaming entry" signal.
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
    /// result; the TUI renders it red. `kind` tells the TUI whether the
    /// model built the call badly (bold red) or the tool failed for
    /// another reason (red).
    ToolError {
        agent: String,
        call_id: String,
        tool: String,
        error: String,
        kind: crate::engine::tool::ToolFailKind,
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
    /// Provider-reported token usage for the round-trip that just
    /// completed. Absent when the provider didn't include a usage
    /// chunk in the response stream.
    Usage {
        agent: String,
        usage: crate::tokens::TokenUsage,
    },
    /// The driver loop unwound to the root and drained its queue: the
    /// agent is idle, waiting for the next user message. Emitted by the
    /// driver (not by [`turn`]) as the falling edge that stops the
    /// TUI's span-long working indicator. No agent name — it's a
    /// whole-stack signal, not a per-agent one.
    AgentIdle,

    /// A `question` tool raised an interrupt (GOALS §3b): the agent is
    /// blocked until the user answers. The TUI opens the answering
    /// dialog from this; the answer round-trips back to the daemon as
    /// `ResolveInterrupt`. Carries the batch of questions to render.
    InterruptRaised {
        interrupt_id: uuid::Uuid,
        /// Interrupt-level context (from `raise_interrupt(description, …)`),
        /// rendered as a muted context header above the question prompt.
        /// Empty when the agent supplied none.
        description: String,
        questions: crate::daemon::proto::InterruptQuestionSet,
    },

    /// An async job (loop / timer / background, GOALS §22) started. UI
    /// only — drives the transient jobs strip. `kind` is `loop` /
    /// `timer` / `background`. `session_id` lets a multi-session client
    /// scope per-session views (`/ps`, `/stop`) without reaching across
    /// sessions.
    JobStarted {
        session_id: uuid::Uuid,
        job_id: String,
        label: String,
        kind: String,
    },
    /// A background job produced an output line (it's in the ring buffer
    /// now). UI-only progress tick so the strip can show liveness; the
    /// output itself reaches the model only via `background.tail` or the
    /// budget-capped completion.
    JobProgress { job_id: String },
    /// A note from an ephemeral-fork loop iteration. Shown live in the
    /// UI; enters main context only at loop termination (bundled with the
    /// terminal result) — token economy (§22).
    JobNote { job_id: String, text: String },
    /// An async job reached a terminal state. UI-only marker; the
    /// model-facing result is injected separately as a late-arriving turn
    /// by the driver. `failed` drives the red treatment + needs_attention
    /// wording.
    JobCompleted {
        job_id: String,
        label: String,
        kind: String,
        failed: bool,
    },

    /// How many wire tokens `/prune` would drop from the **foreground**
    /// agent's context right now (GOALS §1a / §10). Recomputed by the
    /// driver from the same `dedup_plan` `/prune` executes, so the
    /// status-line `ctx X% → Y% prunable` figure equals what `/prune`
    /// then removes. Emitted after every turn settles and after a prune.
    /// `cache_cold` carries the cache-cold predicate's verdict so the
    /// `/prune` confirm copy reports hot-vs-cold without guessing.
    ContextProjection {
        prunable_tokens: u64,
        cache_cold: bool,
    },

    /// A `/prune` (manual or auto) completed on the foreground agent.
    /// `auto` distinguishes the cache-aware auto-fire from a user
    /// invocation. `bodies` is how many snapshot bodies were elided this
    /// prune; `tokens_saved` is the wire-token drop. `elided` is the
    /// **current** full set of `original_event_id`s whose tool-result body
    /// is now an elision marker in the wire history (cumulative across
    /// prunes, not just this one). The TUI dims the matching scrollback
    /// tool-result bodies by their `call_id`; full text stays visible
    /// (GOALS §14 wire-vs-user split). UI marker for the transcript.
    Pruned {
        auto: bool,
        bodies: usize,
        tokens_saved: u64,
        elided: Vec<String>,
    },

    /// `/compact` assembled a fresh-thread handoff. Carries the
    /// review-ready handoff text (brief + deterministic appendix +
    /// seed-tool plan) for the TUI to drop into the composer, plus the
    /// new session id the daemon created and the seed-tool count. The
    /// old session stays recoverable in SQLite.
    CompactReady {
        new_session_id: uuid::Uuid,
        handoff: String,
        seed_tool_count: usize,
        seed_tool_tokens: u64,
    },

    /// Filesystem sandboxing was toggled for the session (`/sandbox`,
    /// sandboxing part 2). UI-only: the TUI surfaces the resulting state
    /// as a toast. Emitted by the daemon's `SetSandbox` handler.
    SandboxState { enabled: bool },

    /// Caffeination (`/caffeinate`) state changed — daemon-global,
    /// broadcast to every client (incl. until-idle auto-off). Drives the
    /// `☕` chrome glyph on all clients + a toast on the originator.
    /// `message` is `Some` only for the client that issued the request.
    CaffeinateState {
        active: bool,
        lid_close_guaranteed: bool,
        message: Option<String>,
    },
}

/// Outcome of one [`turn`] call. The driver loops on the result.
#[derive(Debug)]
pub enum TurnOutcome {
    /// Agent produced text and no tool calls — its turn is done.
    Done,
    /// Agent produced one or more tool calls; the loop must run another
    /// turn so the model can react to the results.
    Continue,
    /// Agent invoked `task` for an *interactive* subagent (e.g.
    /// `coder` from `Build`). The driver pushes a fresh
    /// session onto the stack and the subagent takes over the
    /// conversation until it produces final text.
    SpawnSubagent {
        child_agent: String,
        prompt: String,
        /// Outstanding tool-call id the driver must answer when the
        /// subagent finishes. `ToolCall.id` is `String`; `ToolCall.call_id`
        /// is `Option<String>` because some providers don't surface a
        /// distinct id and rig's `tool_result_with_call_id` accepts the
        /// pair shape.
        task_call_id: String,
        task_function_call_id: Option<String>,
    },
    /// Agent invoked `task` for a *noninteractive* subagent (e.g.
    /// `explore` from `Build`). The driver runs the
    /// child's full conversation loop to completion synchronously
    /// and delivers its final text back as the parent's tool result —
    /// the user sees the spawn rendered like a single tool call,
    /// not a primary handoff.
    SpawnNoninteractive {
        child_agent: String,
        prompt: String,
        task_call_id: String,
        task_function_call_id: Option<String>,
    },
    /// Agent invoked the `jobs` meta-tool (GOALS §22). Like `task`, this
    /// is intercepted by the engine and routed to the driver, which owns
    /// the single async-job authority. The driver dispatches the action,
    /// builds the tool result, and delivers it back as this call's
    /// tool_result — same shape as a noninteractive tool call.
    JobAction {
        /// Repaired `{action, args}` payload.
        args: Value,
        task_call_id: String,
        task_function_call_id: Option<String>,
    },
}

/// Drive one round-trip with the model + dispatch any tool calls. The
/// `history` buffer is mutated in place: the user message (if any) was
/// pushed by the caller; this function appends the assistant turn and
/// every tool-result message in order.
///
/// `redact` is the §7 chokepoint — tool outputs are scrubbed before
/// they enter history so a leaked secret from bash / read / edit never
/// becomes part of the next inference call. The model also never sees
/// the raw form via the user transcript: `tool_call_events.output` is
/// the scrubbed text.
#[allow(clippy::too_many_arguments)]
pub async fn turn(
    agent: &Agent,
    history: &mut Vec<Message>,
    prompt: Message,
    session: Arc<Session>,
    locks: Arc<crate::locks::LockManager>,
    redact: Arc<RedactionTable>,
    cwd: std::path::PathBuf,
    interrupts: Arc<crate::engine::interrupt::InterruptHub>,
    cancel: tokio_util::sync::CancellationToken,
    approver: Option<Arc<crate::approval::Approver>>,
    loop_guard_threshold: u32,
    tx: &mpsc::Sender<TurnEvent>,
) -> Result<TurnOutcome> {
    let tools = agent.tools.definitions();

    // Tell the TUI we've called the model — `Thinking…` shows until the
    // first AssistantTextDelta arrives.
    let _ = tx
        .send(TurnEvent::ThinkingStarted {
            agent: agent.name.clone(),
        })
        .await;

    // Stamp the send time for the cache-cold predicate's TTL arm
    // (GOALS §10). Done right before the round-trip so "time since last
    // send" measures from when the provider last saw (and cached) the
    // prefix.
    session.note_send();

    // One id per round-trip, shared by the captured request body
    // (`inference_requests`), the metadata row (`inference_calls`), and
    // the `inference_request` timeline event — so the export joins them
    // (session-log-export Parts A/B).
    let call_id = Uuid::new_v4();

    let ((msg_id, choice, usage), captured_request) = agent
        .model
        .complete_captured(
            &agent.system,
            history,
            prompt.clone(),
            &tools,
            agent.params.clone(),
            &agent.name,
            tx,
            &cancel,
        )
        .await
        .with_context(|| format!("completion call for agent `{}`", agent.name))?;

    // Persist the full as-sent (post-redaction) request body (Part A).
    // Best-effort: auditing must never break a live turn.
    if let Err(e) = session.record_inference_request(call_id, &captured_request) {
        tracing::warn!(error = %e, "record_inference_request failed");
    }
    // Timeline event for the request (Part B). Token usage is folded in
    // when the provider reported it; the export resolves the `file` name
    // deterministically from this event's seq + short_id + call_id.
    let usage_json = usage.map(|u| {
        serde_json::json!({
            "input_tokens": u.input_tokens,
            "output_tokens": u.output_tokens,
            "cached_input_tokens": u.cached_input_tokens,
        })
    });
    if let Err(e) = session.record_event(
        crate::db::session_log::SessionEventKind::InferenceRequest,
        Some(&agent.name),
        Some(&call_id.to_string()),
        &serde_json::json!({ "usage": usage_json }),
    ) {
        tracing::warn!(error = %e, "record inference_request event failed");
    }

    // Assistant output text, extracted once: used both for the
    // calibration text basis below and the AssistantText emit further
    // down.
    let text = extract_text(&choice);

    if let Some(u) = usage {
        if let Err(e) = session.record_usage(call_id, u) {
            tracing::warn!(error = %e, "session.record_usage failed");
        }
        // Feed the round into tokenizer calibration. The basis is a
        // consistent text proxy for what was sent + produced (the
        // messages already in history + this prompt + the assistant
        // output); the scale factor absorbs system/tool/serialization
        // overhead, so we deliberately don't reconstruct rig's exact
        // request wire format.
        let mut basis = String::new();
        for m in history.iter() {
            if let Ok(s) = serde_json::to_string(m) {
                basis.push_str(&s);
            }
        }
        if let Ok(s) = serde_json::to_string(&prompt) {
            basis.push_str(&s);
        }
        basis.push_str(&text);
        session.note_calibration_sample(&basis, u);

        let _ = tx
            .send(TurnEvent::Usage {
                agent: agent.name.clone(),
                usage: u,
            })
            .await;
    }

    // Persist the assistant turn.
    history.push(prompt);
    history.push(Message::Assistant {
        id: msg_id.clone(),
        content: choice.clone(),
    });

    // Even with streaming, emit a final AssistantText so the TUI knows
    // to freeze the live-streaming entry into a static history row.
    // Non-streaming paths land here directly. `text` was extracted above.
    if !text.trim().is_empty() {
        let _ = tx
            .send(TurnEvent::AssistantText {
                agent: agent.name.clone(),
                text: text.clone(),
            })
            .await;
        // Timeline event (Part B). Tagged with the same `call_id` as the
        // request that produced it so the export can group a turn.
        if let Err(e) = session.record_event(
            crate::db::session_log::SessionEventKind::AssistantMessage,
            Some(&agent.name),
            Some(&call_id.to_string()),
            &serde_json::json!({ "text": text }),
        ) {
            tracing::warn!(error = %e, "record assistant_message event failed");
        }
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
        redact: redact.clone(),
        interrupts,
        cancel,
        approver,
    };

    for tc in &calls {
        // `task` is special — it's a structural tool the driver
        // handles. For interactive subagents (coder) the driver
        // performs a primary handoff via [`TurnOutcome::SpawnSubagent`];
        // for noninteractive ones (explore) it runs the child inline
        // and returns the result as this task call's tool_result via
        // [`TurnOutcome::SpawnNoninteractive`]. Other tool calls in
        // the same assistant turn are dropped — the model will re-
        // emit them on the next turn once it has the task result.
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
            let noninteractive = crate::engine::builtin::is_noninteractive(&child);
            // Timeline event (Part B): a `task` delegation spawned a
            // child. Carries the child agent, the triggering task call id,
            // and the brief. (Interactive subagents share this session's
            // id with a distinct agent name; user-/loop-forks are separate
            // sessions the export follows via the fork tree.)
            if let Err(e) = session.record_event(
                crate::db::session_log::SessionEventKind::SubagentSpawned,
                Some(&agent.name),
                Some(&tc.id),
                &serde_json::json!({
                    "child_agent": child,
                    "task_call_id": tc.id,
                    "noninteractive": noninteractive,
                    "prompt": prompt,
                }),
            ) {
                tracing::warn!(error = %e, "record subagent_spawned event failed");
            }
            if !noninteractive {
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
            return Ok(TurnOutcome::SpawnNoninteractive {
                child_agent: child,
                prompt,
                task_call_id: tc.id.clone(),
                task_function_call_id: tc.call_id.clone(),
            });
        }

        // `jobs` is structural in the **main** conversation: the driver
        // owns the single async-job authority (GOALS §22), so the action
        // is routed there via [`TurnOutcome::JobAction`]. Inside an
        // ephemeral-fork loop iteration the toolbox instead carries the
        // in-process `ForkJobTool` (alongside `note`) — there, `jobs` is
        // dispatched normally and re-routes create-actions to requests
        // (forks cannot spawn jobs). We tell the two apart by the
        // fork-only `note` tool: present only inside a loop fork.
        if tc.function.name == "jobs" && agent.tools.get("note").is_none() {
            let mut args = tc.function.arguments.clone();
            // Validate + repair the loose outer object against the `jobs`
            // tool's own minimal `{action, args}` schema; per-action
            // validation runs in the driver through the same repair
            // contract (§12). The outer schema is permissive (`args` is a
            // free-form object), so this only catches a malformed `action`.
            let jobs_schema = agent
                .tools
                .get("jobs")
                .map(|t| t.parameters())
                .unwrap_or(Value::Null);
            let _ = repair(&mut args, &jobs_schema, "jobs");
            return Ok(TurnOutcome::JobAction {
                args,
                task_call_id: tc.id.clone(),
                task_function_call_id: tc.call_id.clone(),
            });
        }

        let start = Instant::now();
        let mut args = tc.function.arguments.clone();
        let original = args.clone();

        // Validate-then-repair against the tool's own JSON Schema (§12).
        // Clean input is returned untouched; a repairable malformation is
        // fixed at the disagreeing path and re-validated; an unrecoverable
        // call short-circuits to a model-readable hard-fail *without*
        // dispatching the tool. An unknown tool name has no schema, so it
        // validates trivially and surfaces its "unknown tool" error in
        // `dispatch_one` as before.
        let schema = agent
            .tools
            .get(&tc.function.name)
            .map(|t| t.parameters())
            .unwrap_or(Value::Null);
        let repair_outcome = repair(&mut args, &schema, &tc.function.name);
        let recovery = repair_outcome.recovery;

        let _ = tx
            .send(TurnEvent::ToolStart {
                agent: agent.name.clone(),
                call_id: tc.id.clone(),
                tool: tc.function.name.clone(),
                args: args.clone(),
            })
            .await;

        // Loop guard (GOALS §1/§12): block a back-to-back identical tool
        // call (same name + canonical post-repair `wire_input`) pending
        // approval. Only schema-valid calls are guarded — a malformed call
        // already short-circuits below, and isn't a "loop" worth
        // prompting on. The chain is maintained on `session` so it spans
        // turns; an intervening different call resets the count. When the
        // guard rejects (one-off, an always-reject rule, or headless), the
        // call is *not* dispatched and a guidance error stands in as the
        // tool result so the model changes course. With no approver wired
        // (seed-tool re-exec, tool tests) the guard is skipped — never
        // silently denied, matching the command/path approval contract.
        let loop_guard_reject = if repair_outcome.valid
            && let Some(approver) = ctx.approver.as_ref()
        {
            let signature =
                crate::approval::store::GrantStore::loop_signature(&tc.function.name, &args);
            let consecutive = session.bump_consecutive_call(&signature);
            if consecutive >= loop_guard_threshold.max(1) {
                let interactive = ctx.interrupts.is_interactive_attached();
                let decision = approver
                    .approve_repeat(&tc.function.name, &args, interactive)
                    .await?;
                !decision.is_accept()
            } else {
                false
            }
        } else {
            false
        };

        // Dispatch only when validate-then-repair produced a schema-valid
        // call AND the loop guard didn't reject it. Otherwise skip dispatch
        // and treat the model-readable diagnostic as an invocation failure
        // — same downstream audit/telemetry/history path a tool's own
        // `invalid_input` takes.
        let result = if loop_guard_reject {
            Err(invalid_input(loop_guard_message(&tc.function.name)))
        } else if repair_outcome.valid {
            dispatch_one(&agent.tools, &tc.function.name, args.clone(), &ctx).await
        } else {
            let msg = repair_outcome.error.unwrap_or_else(|| {
                format!("`{}` arguments failed schema validation", tc.function.name)
            });
            Err(invalid_input(msg))
        };

        // Per §13c: if the tool returned a recovery + canonical args
        // (today only `editunlock` does), prefer the tool's recovery
        // over the shape-repair one for this row, and use the canonical
        // form as the row's `wire_input_json` AND the in-history
        // assistant message's tool-call args. That last bit makes the
        // model's next inference see the form that would have matched
        // at stage 1.
        let (tool_recovery, wire_args) = match &result {
            Ok(out) => (out.recovery.clone(), out.canonical_args.clone()),
            Err(_) => (None, None),
        };
        if let Some(canonical) = &wire_args {
            args = canonical.clone();
            rewrite_assistant_tool_call(history, &tc.id, canonical);
        }
        let recovery = tool_recovery.unwrap_or(recovery);

        let (raw_output, hard_fail, fail_kind) = match &result {
            Ok(ToolOutput { content, .. }) => (content.clone(), false, None),
            Err(e) => {
                let msg = format!("Error: {e}");
                (msg, true, Some(crate::engine::tool::classify_failure(e)))
            }
        };

        // Scrub tool output through the §7 chokepoint before it enters
        // history or the audit row. The model only ever sees the
        // redacted form; the user transcript shows the same (audit
        // expansion of `original_input` does not apply to tool *outputs*,
        // only to tool *inputs* — see §14e).
        let output_str = redact.scrub(&raw_output);

        if hard_fail {
            let _ = tx
                .send(TurnEvent::ToolError {
                    agent: agent.name.clone(),
                    call_id: tc.id.clone(),
                    tool: tc.function.name.clone(),
                    error: output_str.clone(),
                    kind: fail_kind.unwrap_or(crate::engine::tool::ToolFailKind::Execution),
                })
                .await;
        } else {
            let truncated = matches!(
                &result,
                Ok(ToolOutput {
                    truncated: true,
                    ..
                })
            );
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

        let truncated = matches!(
            &result,
            Ok(ToolOutput {
                truncated: true,
                ..
            })
        );

        let duration_ms = start.elapsed().as_millis() as u64;

        // Surface the recovery split for the timeline event (Part B):
        // the wire-vs-user inputs + recovery kind/stage make tool-input
        // corrections auditable in the export.
        let (recovery_kind, recovery_stage) = recovery.db_fields();
        let tool_path = args.get("path").and_then(Value::as_str).map(str::to_string);

        // Persist the audit row. v0 stores the original AND a wire form
        // that's equal to the original; §13c canonical-form rewrite
        // will diverge them when implemented.
        if let Err(e) = session.record_tool_call(ToolCallRow {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            agent: agent.name.clone(),
            call_id: tc.id.clone(),
            tool: tc.function.name.clone(),
            path: tool_path,
            original_input_json: original.clone(),
            wire_input_json: args.clone(),
            recovery,
            hard_fail,
            output: output_str.clone(),
            truncated,
            duration_ms,
        }) {
            // Auditing must not break the live conversation. Log and
            // continue — the model still sees the tool result.
            tracing::warn!(error = %e, tool = %tc.function.name, "persisting tool_call_event failed");
        }

        // Timeline event (Part B), sourced from / consistent with the
        // `tool_call_events` audit row above. The `call_id` here is the
        // model's per-tool-call id (`tc.id`), which is distinct from the
        // round-trip `call_id` (above) — both correlations matter.
        if let Err(e) = session.record_event(
            crate::db::session_log::SessionEventKind::ToolCall,
            Some(&agent.name),
            Some(&tc.id),
            &serde_json::json!({
                "tool": tc.function.name,
                "original_input": original,
                "wire_input": args,
                "recovery_kind": recovery_kind,
                "recovery_stage": recovery_stage,
                "hard_fail": hard_fail,
                "output": output_str,
                "truncated": truncated,
                "duration_ms": duration_ms,
            }),
        ) {
            tracing::warn!(error = %e, tool = %tc.function.name, "record tool_call event failed");
        }

        history.push(tool_result_message(tc, output_str));
    }

    Ok(TurnOutcome::Continue)
}

/// The guidance error returned as a *tool result* when the loop guard
/// blocks a back-to-back identical call (GOALS §1/§12). It reads as a
/// normal tool-result error so the model changes course rather than
/// treating it as a hard abort. Built with [`invalid_input`] so it
/// classifies as an [`crate::engine::tool::ToolFailKind::Invocation`]
/// failure (the model's repeat is the cause). The dispatcher prefixes
/// `Error:` per the wire-vs-user transcript conventions, the same as any
/// other invocation failure.
fn loop_guard_message(tool: &str) -> String {
    format!(
        "`{tool}` was blocked: it repeats the immediately-preceding tool call exactly \
         (same arguments), which is a likely loop. Do not re-issue the same call — try a \
         different approach: change the arguments, use a different tool, or reconsider \
         whether the previous result already answered the question."
    )
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

/// Mutate the most recent assistant message in `history` so the tool
/// call identified by `call_id` carries `canonical_args` instead of the
/// model's original arguments. Used by the §13c edit-cascade rewrite so
/// the next inference's attention pass over its own outputs sees the
/// form that would have matched at stage 1.
///
/// Walks backwards because the assistant turn we just pushed is the
/// last element. Silent no-op if the message or the matching tool-call
/// isn't found — the audit row still has the canonical form.
///
/// Tripwire for native Anthropic: this mutates the *most recent*
/// assistant turn in place. If that turn carries a signed thinking
/// block, mutating any sibling block risks a "latest assistant message
/// cannot be modified" 400. See `miscellaneous.md` §10b.
fn rewrite_assistant_tool_call(history: &mut [Message], call_id: &str, canonical_args: &Value) {
    use rig::message::AssistantContent;
    for msg in history.iter_mut().rev() {
        if let Message::Assistant { content, .. } = msg {
            for c in content.iter_mut() {
                if let AssistantContent::ToolCall(tc) = c
                    && tc.id == call_id
                {
                    tc.function.arguments = canonical_args.clone();
                    return;
                }
            }
            return;
        }
    }
}

/// Allow `Recovery` to flow into telemetry plumbing without exposing the
/// enum's name to every caller.
pub fn recovery_db_fields(r: &Recovery) -> (Option<&'static str>, Option<&'static str>) {
    r.db_fields()
}
