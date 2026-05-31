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
    /// The active LLM-strength mode this agent was spawned under
    /// (`prompts/llm-modes-defensive-normal.md`). Drives tool-description
    /// verbosity at [`ToolBox::definitions`] time — the one rendering seam.
    pub llm_mode: crate::config::extended::LlmMode,
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
    /// A non-blocking system notice for the transcript (warn chip). Used
    /// by the prompt-injection guard (GOALS §4i) to surface a flagged-but-
    /// below-threshold prompt and the fail-open "scan could not run"
    /// case. Rendered as a muted/yellow plain line; never enters the
    /// model's context (it's UI-only — the user message itself proceeds
    /// unchanged).
    Notice { text: String },

    /// The driver loop unwound to the root and drained its queue: the
    /// agent is idle, waiting for the next user message. Emitted by the
    /// driver (not by [`turn`]) as the falling edge that stops the
    /// TUI's span-long working indicator. No agent name — it's a
    /// whole-stack signal, not a per-agent one.
    AgentIdle,

    /// The primary (root-frame) agent was swapped in place (`/plan` →
    /// `Plan`, `/build` → `Build`, `plan.md §4.6.d`). Emitted by the driver
    /// so the client chrome's active-agent slot tracks the new primary.
    PrimarySwapped { name: String },
    /// The active `llm_mode` was switched live (`/llm-mode`,
    /// `prompts/llm-modes-defensive-normal.md`). The client tracks `mode` so
    /// its `/llm-mode` toggle + cache-break warning resolve against the
    /// authoritative current value.
    LlmModeChanged {
        mode: crate::config::extended::LlmMode,
    },

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

    /// The daemon began (or escalated) a graceful shutdown
    /// (`daemon-graceful-drain-shutdown.md`). Daemon-global. The TUI shows
    /// the drain notice and refuses new input; `forced` distinguishes the
    /// initial drain (in-flight work finishing) from the force-deadline
    /// case (work was aborted — a truncated turn isn't a clean finish).
    DaemonDraining { forced: bool },

    /// Project-scoped plan-status snapshot for the additive chrome slot
    /// (`plan-status-chrome-and-resolver.md`). Daemon-global (carries
    /// `project_id`); the TUI applies it only when `project_id` matches its
    /// own session's project, then renders the ready / in-progress /
    /// interruptions segments (each omitted when zero, slot absent when all
    /// zero). Driven by daemon broadcast, not TUI-local bookkeeping, so a
    /// reconnecting / late-opened TUI shows the correct state.
    PlanStatusState {
        project_id: String,
        ready: i64,
        in_progress: i64,
        interruptions: i64,
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
    /// Agent invoked the `handoff` tool (the `Auto` front door). Like
    /// `task`/`jobs` this is intercepted by the engine and routed to the
    /// driver, which swaps the root-frame primary in place at the idle
    /// boundary (the same machinery `/plan`/`/build` use) and delivers a
    /// confirmation as this call's tool_result. The swapped-in primary
    /// then takes over the conversation.
    Handoff {
        /// The target primary agent name (`Plan` or `Build`).
        target: String,
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
    is_root: bool,
    deferred_log: crate::engine::deferred::DeferredLog,
    tx: &mpsc::Sender<TurnEvent>,
) -> Result<TurnOutcome> {
    let tools = agent.tools.definitions(agent.llm_mode);

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

    // Live instructions-file diff injection (prompt
    // `instructions-file-live-diff.md`). The session's guidance file
    // (`AGENTS.md`/`CLAUDE.md`) was baked into the frozen system block at
    // session start; an in-place edit since then is invisible to the model
    // because the cached prefix is held byte-stable. Detect that here and
    // append the change as a synthetic system-role message at the END of
    // history — append only, so the cached system prefix is untouched
    // (cache-safe). Gated to the session root: subagents recompose a fresh
    // system prompt on spawn and already carry the latest file, so they
    // need no injection. The baseline advances on inject, so each distinct
    // change is injected exactly once (idempotent across turns).
    //
    // The message goes through the normal outbound redaction chokepoint
    // (`redact.scrub`) like any other content — not routed around it — and
    // is appended *before* `prompt`, so it lands at the end of the prior
    // conversation, immediately ahead of the current user message.
    if is_root && let Some(message) = session.guidance_change_injection(&cwd) {
        history.push(Message::System {
            content: redact.scrub(&message),
        });
    }

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

    let calls: Vec<ToolCall> = collect_tool_calls(&choice);

    // Even with streaming, emit a final AssistantText so the TUI knows
    // to freeze the live-streaming entry into a static history row.
    // Non-streaming paths land here directly. `text` was extracted above.
    if !text.trim().is_empty() {
        // Outbound translation (`prompts/utility-translation.md`): when this
        // is the foreground primary's *final* user-facing answer (root frame,
        // no tool calls this turn), translate the COMPLETE assembled text from
        // the model's language back into the user's. The translated form is
        // shown to the user only — the model-language `text` already went into
        // `history` (the wire/transcript split is preserved: the model sees
        // its own output, the user reads the translation) and the timeline
        // `AssistantMessage` event below records the original. When
        // translation is inactive (languages unset/equal, or the utility
        // model is unset/erroring) the text is emitted unchanged — identical
        // to the pre-feature behavior. No streaming translation: the
        // translated answer lands once, here, after the response completes.
        let shown = if is_root && calls.is_empty() {
            translate_final_response(&text, &cwd).await
        } else {
            text.clone()
        };
        let _ = tx
            .send(TurnEvent::AssistantText {
                agent: agent.name.clone(),
                text: shown,
            })
            .await;
        // Timeline event (Part B). Tagged with the same `call_id` as the
        // request that produced it so the export can group a turn. Records the
        // model's *original* output — never the translated user-facing form.
        if let Err(e) = session.record_event(
            crate::db::session_log::SessionEventKind::AssistantMessage,
            Some(&agent.name),
            Some(&call_id.to_string()),
            &serde_json::json!({ "text": text }),
        ) {
            tracing::warn!(error = %e, "record assistant_message event failed");
        }
    }

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
        deferred_log,
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
            // Interactivity: an explicit `mode` override wins; otherwise the
            // agent's own default (`coder`/`plan-author` interactive, the
            // rest noninteractive). The explicit `mode` is the seam the
            // future LLM-strategy axis switches on
            // (`design-need-to-discuss-or-test.md`).
            let noninteractive = match tc.function.arguments.get("mode").and_then(Value::as_str) {
                Some("subagent_interactive") => false,
                Some("subagent") => true,
                _ => crate::engine::builtin::is_noninteractive(&child),
            };
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

        // `handoff` is structural: the driver owns the single primary-swap
        // authority (same idle-boundary mechanism as `/plan`/`/build`), so
        // the `Auto` front door routes the chosen target there via
        // [`TurnOutcome::Handoff`] rather than dispatching a tool here.
        if tc.function.name == "handoff" {
            let mut args = tc.function.arguments.clone();
            let schema = agent
                .tools
                .get("handoff")
                .map(|t| t.parameters())
                .unwrap_or(Value::Null);
            let _ = repair(&mut args, &schema, "handoff");
            let target = args
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("Build")
                .to_string();
            return Ok(TurnOutcome::Handoff {
                target,
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

        // Command-safety gate (`prompts/utility-command-safety-gate.md`):
        // in `auto` approval mode each gated call (`bash`/`webfetch`/
        // `mcp_invoke`) is judged by the utility model — with NO history —
        // before it runs. `safe` → run; `unsafe` (or utility model
        // unavailable → fail CLOSED) → escalate to the user; a denial skips
        // dispatch. The verdict also says whether the result needs a
        // post-run injection re-check (handled after dispatch). Only
        // evaluated for schema-valid, non-loop-rejected gated calls.
        let mut recheck_result = false;
        let gate_block: Option<String> = if repair_outcome.valid && !loop_guard_reject {
            match safety_gate_decision(&tc.function.name, &args, &ctx, tx).await {
                GateOutcome::Run { recheck } => {
                    recheck_result = recheck;
                    None
                }
                GateOutcome::Block(msg) => Some(msg),
            }
        } else {
            None
        };

        // Dispatch only when validate-then-repair produced a schema-valid
        // call AND the loop guard didn't reject it AND the safety gate
        // didn't block it. Otherwise skip dispatch and treat the
        // model-readable diagnostic as an invocation failure — same
        // downstream audit/telemetry/history path a tool's own
        // `invalid_input` takes.
        let result = if loop_guard_reject {
            Err(invalid_input(loop_guard_message(&tc.function.name)))
        } else if let Some(msg) = gate_block {
            Err(invalid_input(msg))
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
        let mut output_str = redact.scrub(&raw_output);

        // Result injection re-check (`prompts/utility-command-safety-gate.md`):
        // when the safety gate flagged this call's result as pulling in
        // external/untrusted content, route the (scrubbed) output through
        // the shared injection-check mechanism. A `high` rating BLOCKS and
        // asks the user (allow through / drop / edit — same override UX as
        // the inbound prompt-injection block); `medium` delivers with a warn
        // chip; `low` (or unavailable → can't-recheck warn) delivers. The
        // recorded transcript keeps the post-recheck `output_str` (wire =
        // user, GOALS §14). Only fires on a successful, flagged call.
        if recheck_result && !hard_fail {
            output_str = result_recheck(&output_str, &ctx, tx).await;
        }

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

/// Translate the foreground primary's complete final response from the
/// model's language back into the user's (`prompts/utility-translation.md`).
/// Loads the layered config for `cwd`; when translation is inactive or the
/// utility model is unset/unavailable the input is returned unchanged
/// (degrade, never block). The `<think>…</think>` reasoning that some
/// models inline in their text is stripped before translation so the
/// translated answer matches what the streamed path already shows (the
/// reasoning rides the separate reasoning channel).
async fn translate_final_response(text: &str, cwd: &std::path::Path) -> String {
    let Some((extended, providers)) = crate::engine::translate::load_if_active(cwd) else {
        return text.to_string();
    };
    let stripped = crate::engine::translate::strip_think_blocks(text);
    crate::engine::translate::outbound(&stripped, &extended, &providers).await
}

/// The tools the command-safety gate (`auto` approval mode) covers: `bash`
/// plus the two network tools (`webfetch`, `mcp_invoke`). Anything else is
/// out of scope and runs ungated. Matched by name in the dispatch loop.
fn is_gated_tool(name: &str) -> bool {
    matches!(name, "bash" | "webfetch" | "mcp_invoke")
}

/// What the command-safety gate decided for one call.
enum GateOutcome {
    /// Proceed to dispatch. `recheck` is whether the call's result must be
    /// injection-re-checked afterward.
    Run { recheck: bool },
    /// Skip dispatch; the string is the model-readable tool result
    /// (`invalid_input`) explaining why the call was withheld.
    Block(String),
}

/// Decide a single gated call under the session's approval mode
/// (`prompts/utility-command-safety-gate.md`). Non-gated tools, and the
/// `manual`/`yolo` modes, never reach the utility-model gate:
///
/// - `manual` → the user approves everything elsewhere; the gate is not
///   this mode's engine. Run (no per-call gate here).
/// - `yolo` → run everything unprompted.
/// - `auto` → judge the single call (no history) via the utility model:
///   `safe` runs; `unsafe` escalates to the user; utility-model unavailable
///   fails CLOSED (escalates). A user denial blocks dispatch.
///
/// The evaluator also reports whether the result needs an injection
/// re-check; that flag is threaded back on [`GateOutcome::Run`].
async fn safety_gate_decision(
    tool: &str,
    args: &Value,
    ctx: &ToolCtx,
    tx: &mpsc::Sender<TurnEvent>,
) -> GateOutcome {
    use crate::config::extended::ApprovalMode;
    use crate::engine::safety_gate::{SafetyOutcome, evaluate};

    if !is_gated_tool(tool) {
        return GateOutcome::Run { recheck: false };
    }
    match ctx.session.approval_mode() {
        // `manual`: the gate is not invoked (the user is the gate elsewhere).
        // `yolo`: everything runs, gate bypassed. Either way, run ungated.
        ApprovalMode::Manual | ApprovalMode::Yolo => return GateOutcome::Run { recheck: false },
        ApprovalMode::Auto => {}
    }

    // `auto` mode. The utility model judges this single call with no
    // conversation history. The guard's own model override falls back to the
    // utility model (same chain the injection guard uses).
    tracing::debug!(
        mode = crate::config::extended::ApprovalMode::Auto.as_str(),
        tool,
        "safety gate: evaluating gated call"
    );
    let (extended, providers) = crate::auto_title::load_configs_for(&ctx.cwd);
    let model_ref = extended
        .prompt_injection_guard
        .model
        .as_deref()
        .or(extended.utility_model.as_deref());

    let payload = gate_payload(tool, args);
    let outcome = evaluate(model_ref, &providers, tool, &payload).await;

    match outcome {
        SafetyOutcome::Rated(verdict) if verdict.safe => {
            // Safe → run without prompting.
            GateOutcome::Run {
                recheck: verdict.recheck_result,
            }
        }
        SafetyOutcome::Rated(verdict) => {
            // Unsafe → escalate to the user. A denial blocks dispatch.
            // If the user approves, still honor the result re-check flag.
            match escalate_gated_call(tool, args, ctx, false, tx).await {
                true => GateOutcome::Run {
                    recheck: verdict.recheck_result,
                },
                false => GateOutcome::Block(gate_block_message(tool, false)),
            }
        }
        SafetyOutcome::Unavailable => {
            // Fail CLOSED: the gate couldn't vet the call, so treat it as
            // requiring user approval rather than silently running it.
            match escalate_gated_call(tool, args, ctx, true, tx).await {
                // Approved → run, and (conservatively) re-check the result:
                // the eval that would have set the flag never completed, so a
                // call the user only let through under an unavailable gate
                // still gets its result vetted if it's a network tool.
                true => GateOutcome::Run {
                    recheck: tool != "bash",
                },
                false => GateOutcome::Block(gate_block_message(tool, true)),
            }
        }
    }
}

/// The single command/call text the safety evaluator judges. For `bash`
/// it's the raw command line; for the network tools it's the call's
/// arguments serialized compactly (the URL / server+tool+args).
fn gate_payload(tool: &str, args: &Value) -> String {
    if tool == "bash" {
        return args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
    }
    serde_json::to_string(args).unwrap_or_else(|_| args.to_string())
}

/// Escalate a gated call to the user through the existing approval prompt.
/// `bash` reuses [`Approver::approve_command`] (classify + command-detail
/// UX); the network tools use the once-only [`Approver::approve_tool_call`].
/// `unavailable` tailors the surfaced reason (gate down vs. rated unsafe).
/// With no approver wired (seed re-exec, tests) there is no client to ask —
/// fail closed by treating it as denied.
async fn escalate_gated_call(
    tool: &str,
    args: &Value,
    ctx: &ToolCtx,
    unavailable: bool,
    tx: &mpsc::Sender<TurnEvent>,
) -> bool {
    let Some(approver) = ctx.approver.as_ref() else {
        // No human to ask → fail closed (do not silently run).
        return false;
    };

    // Surface why we're asking (the safety gate, not an ordinary approval).
    let reason = if unavailable {
        format!(
            "safety gate unavailable (utility model unset or unreachable) — asking before running `{tool}`"
        )
    } else {
        format!("safety gate flagged this `{tool}` call as unsafe — asking before running it")
    };
    let _ = tx.send(TurnEvent::Notice { text: reason }).await;

    let decision = if tool == "bash" {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        approver.approve_command(command).await
    } else {
        let label = format!("{tool} {}", gate_payload(tool, args));
        approver.approve_tool_call(&label).await
    };
    matches!(decision, Ok(d) if d.is_allowed())
}

/// The model-readable tool result when a gated call is withheld (denied at
/// the safety-gate escalation). Reads as an invocation error so the model
/// changes course rather than treating it as a hard abort.
fn gate_block_message(tool: &str, unavailable: bool) -> String {
    if unavailable {
        format!(
            "`{tool}` was not run: the command-safety gate could not reach the utility model and \
             the user declined to run it unverified. Try a different approach or ask the user."
        )
    } else {
        format!(
            "`{tool}` was not run: the command-safety gate flagged it as unsafe and the user \
             declined. Do not retry the same call — choose a safer approach."
        )
    }
}

/// What to do with a flagged tool result given its injection-check
/// outcome. Pure routing decision, split out so it's unit-testable without
/// a live utility model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecheckAction {
    /// Deliver the result unchanged (`low` rating).
    Pass,
    /// Deliver with a warn chip (`medium` rating).
    Warn,
    /// Block and ask the user — allow / drop / edit (`high` rating).
    Block,
    /// Re-check could not run; deliver with a "could not re-check" chip.
    /// Never silently asserts the high-risk content is clean — surfaces it.
    Unavailable,
}

/// Map an injection-check outcome to the result-recheck action
/// (`prompts/utility-command-safety-gate.md`). `high` blocks, `medium`
/// warns, `low` (and the never-rated `off`) pass, and an unavailable check
/// surfaces a "could not re-check" chip.
fn result_recheck_action(outcome: crate::engine::injection_check::CheckOutcome) -> RecheckAction {
    use crate::config::extended::InjectionThreshold;
    use crate::engine::injection_check::CheckOutcome;
    match outcome {
        CheckOutcome::Rated(InjectionThreshold::High) => RecheckAction::Block,
        CheckOutcome::Rated(InjectionThreshold::Medium) => RecheckAction::Warn,
        CheckOutcome::Rated(_) => RecheckAction::Pass,
        CheckOutcome::Unavailable => RecheckAction::Unavailable,
    }
}

/// Route a flagged tool result through the shared injection-check mechanism
/// (`prompts/utility-command-safety-gate.md`). Returns the text that should
/// enter history (and the audit row — wire = user, GOALS §14):
///
/// - `high` → BLOCK and ask the user (allow through / drop / edit), same
///   override UX as the inbound prompt-injection block.
/// - `medium` → deliver with a warn chip.
/// - `low` → deliver unchanged.
/// - unavailable → deliver with a "could not re-check" warn chip (the call
///   already passed the gate; mirror fail-safe by flagging it rather than
///   silently asserting it's clean).
async fn result_recheck(output: &str, ctx: &ToolCtx, tx: &mpsc::Sender<TurnEvent>) -> String {
    use crate::config::extended::resolve_injection_guard;
    use crate::engine::injection_check::check;

    let (extended, providers) = crate::auto_title::load_configs_for(&ctx.cwd);
    let guard = resolve_injection_guard(&ctx.cwd);
    let model_ref = extended
        .prompt_injection_guard
        .model
        .as_deref()
        .or(extended.utility_model.as_deref());

    let outcome = check(model_ref, &providers, &guard.check_prompt, output).await;
    match result_recheck_action(outcome) {
        RecheckAction::Block => result_injection_override(output, ctx, tx).await,
        RecheckAction::Warn => {
            let _ = tx
                .send(TurnEvent::Notice {
                    text:
                        "tool result rated `medium` for prompt injection — delivering with caution"
                            .to_string(),
                })
                .await;
            output.to_string()
        }
        RecheckAction::Pass => output.to_string(),
        RecheckAction::Unavailable => {
            let _ = tx
                .send(TurnEvent::Notice {
                    text: "tool result could not be re-checked for prompt injection (utility \
                           model unset or unavailable) — delivering unverified"
                        .to_string(),
                })
                .await;
            output.to_string()
        }
    }
}

/// Option ids for the high-risk tool-result override prompt
/// (`prompts/utility-command-safety-gate.md`). Mirrors the inbound
/// prompt-injection override's stable-id pattern in the driver.
const ID_RESULT_ALLOW: &str = "res_allow";
const ID_RESULT_DROP: &str = "res_drop";
const ID_RESULT_EDIT: &str = "res_edit";

/// The placeholder that replaces a dropped/withheld high-risk result in the
/// transcript. Recorded as the result (wire = user, GOALS §14) so both the
/// model and the user see the same withheld marker.
const RESULT_WITHHELD: &str =
    "[tool result withheld: rated high-risk for prompt injection and dropped by the user]";

/// A high-risk tool result was flagged by the re-check: block it and ask
/// the user how to proceed — allow through / drop / edit — the same
/// override UX as the inbound prompt-injection block. Returns the text that
/// should be delivered to the model and recorded.
///
/// Headless (no interactive client to answer) → the block stands: the
/// result is withheld (fail safe — never silently deliver unvetted
/// high-risk content). A dismissal reads the same.
async fn result_injection_override(
    output: &str,
    ctx: &ToolCtx,
    tx: &mpsc::Sender<TurnEvent>,
) -> String {
    use crate::daemon::proto::{InterruptOption, InterruptQuestion, InterruptQuestionSet};

    if !ctx.interrupts.is_interactive_attached() {
        let _ = tx
            .send(TurnEvent::Notice {
                text: "tool result rated `high` for prompt injection; no interactive client to \
                       confirm — withheld"
                    .to_string(),
            })
            .await;
        return RESULT_WITHHELD.to_string();
    }

    let description =
        "A tool result was rated high-risk for prompt injection. It may try to hijack the agent. \
         How do you want to proceed?"
            .to_string();
    let question = InterruptQuestion::Single {
        prompt: "Deliver this high-risk tool result?".to_string(),
        options: vec![
            InterruptOption {
                id: ID_RESULT_ALLOW.to_string(),
                label: "Allow it through unchanged".to_string(),
                description: Some("the agent sees the full result".to_string()),
            },
            InterruptOption {
                id: ID_RESULT_DROP.to_string(),
                label: "Drop it".to_string(),
                description: Some("the agent sees a withheld marker".to_string()),
            },
            InterruptOption {
                id: ID_RESULT_EDIT.to_string(),
                label: "Edit what the agent sees".to_string(),
                description: Some("you'll type the replacement next".to_string()),
            },
        ],
        allow_freetext: false,
        command_detail: None,
    };
    let set = InterruptQuestionSet {
        questions: vec![question],
    };

    let response = raise_and_wait_in_turn(ctx, &description, set).await;
    match selected_id_of(&response).as_deref() {
        Some(ID_RESULT_ALLOW) => {
            let _ = tx
                .send(TurnEvent::Notice {
                    text: "high-risk tool result allowed through".to_string(),
                })
                .await;
            output.to_string()
        }
        Some(ID_RESULT_EDIT) => {
            let edit_set = InterruptQuestionSet {
                questions: vec![InterruptQuestion::Freetext {
                    prompt: "Enter the replacement result the agent should see (blank drops it)"
                        .to_string(),
                }],
            };
            let resp = raise_and_wait_in_turn(ctx, "Edit the tool result", edit_set).await;
            match freetext_of(&resp) {
                Some(text) if !text.trim().is_empty() => {
                    let _ = tx
                        .send(TurnEvent::Notice {
                            text: "high-risk tool result replaced with your edit".to_string(),
                        })
                        .await;
                    text
                }
                _ => {
                    let _ = tx
                        .send(TurnEvent::Notice {
                            text: "high-risk tool result dropped (no replacement entered)"
                                .to_string(),
                        })
                        .await;
                    RESULT_WITHHELD.to_string()
                }
            }
        }
        // Drop, or a dismissal → withhold (fail safe).
        _ => {
            let _ = tx
                .send(TurnEvent::Notice {
                    text: "high-risk tool result dropped".to_string(),
                })
                .await;
            RESULT_WITHHELD.to_string()
        }
    }
}

/// Raise an interrupt from inside a turn and block until the user answers,
/// reusing the persist → register → emit → wait ordering the `question`
/// tool and `Approver` rely on. On a DB failure returns `Cancel` (treated
/// as a dismissal) rather than hanging. Mirrors `Driver::raise_and_wait`
/// but using the turn's `ToolCtx` (no `Driver` handle here).
async fn raise_and_wait_in_turn(
    ctx: &ToolCtx,
    description: &str,
    set: crate::daemon::proto::InterruptQuestionSet,
) -> crate::daemon::proto::ResolveResponse {
    let interrupt_id = match ctx.session.db.raise_interrupt_questions(
        ctx.session.id,
        &ctx.agent_id,
        description,
        &set,
    ) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "result injection override: raising interrupt failed");
            return crate::daemon::proto::ResolveResponse::Cancel;
        }
    };
    let pending = ctx.interrupts.register(interrupt_id);
    ctx.interrupts.emit_raised(
        ctx.session.id,
        interrupt_id,
        &ctx.agent_id,
        description,
        set,
    );
    pending.wait().await
}

/// The selected option id from a resolved single-select interrupt
/// (unwrapping a one-question `Batch`); `Cancel` / other shapes → `None`.
fn selected_id_of(resp: &crate::daemon::proto::ResolveResponse) -> Option<String> {
    use crate::daemon::proto::ResolveResponse;
    match resp {
        ResolveResponse::Single { selected_id } => Some(selected_id.clone()),
        ResolveResponse::Batch { responses } => match responses.first() {
            Some(ResolveResponse::Single { selected_id }) => Some(selected_id.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// The free-text answer from a resolved free-text interrupt (unwrapping a
/// one-question `Batch`); `Cancel` / other shapes → `None`.
fn freetext_of(resp: &crate::daemon::proto::ResolveResponse) -> Option<String> {
    use crate::daemon::proto::ResolveResponse;
    match resp {
        ResolveResponse::Freetext { text } => Some(text.clone()),
        ResolveResponse::Batch { responses } => match responses.first() {
            Some(ResolveResponse::Freetext { text }) => Some(text.clone()),
            _ => None,
        },
        _ => None,
    }
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

#[cfg(test)]
mod safety_gate_tests {
    use super::*;
    use std::sync::Arc;

    use crate::approval::Approver;
    use crate::approval::store::GrantStore;
    use crate::config::extended::ApprovalMode;
    use crate::engine::injection_check::CheckOutcome;
    use crate::engine::tool::ToolCtx;

    /// Build a ToolCtx for the gate tests: a real session (so we can set the
    /// approval mode) plus an `Approver` wired to a detached interrupt hub.
    /// The hub is detached → not interactive, so an escalation prompt would
    /// never resolve; the tests only exercise paths that don't actually wait
    /// (no approver, or modes that skip the gate).
    fn gate_ctx(root: &std::path::Path, mode: ApprovalMode, with_approver: bool) -> ToolCtx {
        let db = crate::db::Db::open_in_memory().unwrap();
        let session =
            crate::session::Session::create(db.clone(), root.to_path_buf(), "coder").unwrap();
        session.set_sandbox_enabled(false);
        session.set_approval_mode(mode);
        let sid = session.id;
        let locks = Arc::new(crate::locks::LockManager::from_db(db.clone()).unwrap());
        let cfg = crate::config::extended::RedactConfig::default();
        let redact = Arc::new(crate::redact::RedactionTable::build(&cfg, root).unwrap());
        let hub = Arc::new(crate::engine::interrupt::InterruptHub::detached());
        let approver = if with_approver {
            let store = GrantStore::new(db.clone(), sid, root.to_path_buf());
            Some(Arc::new(Approver::new(
                store,
                db,
                sid,
                "coder",
                hub.clone(),
            )))
        } else {
            None
        };
        ToolCtx {
            agent_id: "coder".to_string(),
            locks,
            session: Arc::new(session),
            cwd: root.to_path_buf(),
            redact,
            interrupts: hub,
            cancel: tokio_util::sync::CancellationToken::new(),
            approver,
            deferred_log: crate::engine::deferred::DeferredLog::new(),
        }
    }

    #[test]
    fn gate_scope_covers_only_bash_and_network_tools() {
        // bash + the two network tools are gated; everything else is out of
        // scope (read/edit/intel/etc. never reach the utility-model gate).
        assert!(is_gated_tool("bash"));
        assert!(is_gated_tool("webfetch"));
        assert!(is_gated_tool("mcp_invoke"));
        assert!(!is_gated_tool("read"));
        assert!(!is_gated_tool("editunlock"));
        assert!(!is_gated_tool("search"));
        assert!(!is_gated_tool("task"));
    }

    #[tokio::test]
    async fn manual_mode_runs_without_gating() {
        // `manual`: the per-call utility gate is not this mode's engine — the
        // gate decision is `Run` immediately, with no model call and no
        // result re-check requested.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = gate_ctx(tmp.path(), ApprovalMode::Manual, true);
        let (tx, _rx) = mpsc::channel(8);
        let args = serde_json::json!({ "command": "rm -rf /" });
        let outcome = safety_gate_decision("bash", &args, &ctx, &tx).await;
        assert!(matches!(outcome, GateOutcome::Run { recheck: false }));
    }

    #[tokio::test]
    async fn yolo_mode_bypasses_the_gate() {
        // `yolo`: everything runs unprompted; the gate is bypassed even for a
        // destructive command, with no model call.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = gate_ctx(tmp.path(), ApprovalMode::Yolo, true);
        let (tx, _rx) = mpsc::channel(8);
        let args = serde_json::json!({ "command": "rm -rf /" });
        let outcome = safety_gate_decision("bash", &args, &ctx, &tx).await;
        assert!(matches!(outcome, GateOutcome::Run { recheck: false }));
    }

    #[tokio::test]
    async fn non_gated_tool_is_never_gated_even_in_auto() {
        // A non-scoped tool runs ungated in `auto` mode — no model call.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = gate_ctx(tmp.path(), ApprovalMode::Auto, true);
        let (tx, _rx) = mpsc::channel(8);
        let args = serde_json::json!({ "path": "src/main.rs" });
        let outcome = safety_gate_decision("read", &args, &ctx, &tx).await;
        assert!(matches!(outcome, GateOutcome::Run { recheck: false }));
    }

    #[tokio::test]
    async fn auto_mode_fails_closed_when_utility_model_unset_and_no_client() {
        // `auto` + no utility model configured → safety eval is Unavailable →
        // fail CLOSED: escalate to the user. With no approver/interactive
        // client to ask, the call is BLOCKED (not silently run) — the
        // opposite of the inbound scan's fail-open.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = gate_ctx(tmp.path(), ApprovalMode::Auto, false);
        let (tx, _rx) = mpsc::channel(8);
        let args = serde_json::json!({ "command": "ls" });
        let outcome = safety_gate_decision("bash", &args, &ctx, &tx).await;
        match outcome {
            GateOutcome::Block(msg) => {
                assert!(msg.contains("safety gate"), "got: {msg}");
            }
            GateOutcome::Run { .. } => {
                panic!("auto mode must NOT silently run when the gate is unavailable")
            }
        }
    }

    #[test]
    fn gate_payload_uses_command_for_bash_and_args_for_network() {
        let bash = serde_json::json!({ "command": "curl https://x", "cwd": "/tmp" });
        assert_eq!(gate_payload("bash", &bash), "curl https://x");
        let fetch = serde_json::json!({ "url": "https://x.com/foo" });
        let p = gate_payload("webfetch", &fetch);
        assert!(p.contains("https://x.com/foo"), "got: {p}");
    }

    #[test]
    fn result_recheck_routing_maps_rating_to_action() {
        use crate::config::extended::InjectionThreshold;
        // Only a flagged result is ever re-checked; given the outcome, the
        // routing is high→block, medium→warn, low→pass, unavailable→surface.
        assert_eq!(
            result_recheck_action(CheckOutcome::Rated(InjectionThreshold::High)),
            RecheckAction::Block
        );
        assert_eq!(
            result_recheck_action(CheckOutcome::Rated(InjectionThreshold::Medium)),
            RecheckAction::Warn
        );
        assert_eq!(
            result_recheck_action(CheckOutcome::Rated(InjectionThreshold::Low)),
            RecheckAction::Pass
        );
        assert_eq!(
            result_recheck_action(CheckOutcome::Unavailable),
            RecheckAction::Unavailable
        );
    }
}
