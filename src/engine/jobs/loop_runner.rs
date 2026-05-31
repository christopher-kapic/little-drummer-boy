//! Ephemeral-fork loop execution (`keep_in_context = false`, GOALS §22).
//!
//! The whole loop runs inside one spawned task. Each iteration is a turn
//! loop on an **ephemeral fork** branched from the main context as of loop
//! registration:
//!
//! - `independent = false` (default): iterations accumulate in the fork's
//!   own history (iteration 3 sees 1–2).
//! - `independent = true`: each iteration is a fresh fork from the
//!   snapshot, no prior-iteration history.
//!
//! Nothing crosses to main during the loop **except notes**. Notes are
//! shown live in the UI (a [`TurnEvent::JobNote`]) but enter main context
//! only at termination, bundled with the terminal result. Termination =
//! `limit` reached or the model called `loop.cancel` on its own loop. Only
//! the terminal iteration's full result is promoted to main.
//!
//! Forks **cannot** spawn async work: `loop.start`/`background.start`
//! called inside a fork do not execute — they record a
//! [`SpawnRequest`] that rides back to main with the terminal return.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::engine::agent::{Agent, TurnEvent, TurnOutcome, turn};
use crate::engine::jobs::authority::{JobContext, JobEvent};
use crate::engine::jobs::spec::{JobKind, LoopStartArgs};
use crate::engine::message::{Message, extract_text};
use crate::engine::tool::ToolBox;
use crate::intel::budget::BudgetedWriter;
use crate::tools::jobs::{ForkJobState, ForkJobTool, NoteTool};

use super::ASYNC_RESULT_TOKEN_CAP;

/// Everything the spawned ephemeral-loop task needs.
pub struct LoopRunCtx {
    pub job_id: String,
    pub label: String,
    pub args: LoopStartArgs,
    pub ctx: JobContext,
    /// Engine event channel — UI-only signals (notes, progress).
    pub turn_tx: mpsc::Sender<TurnEvent>,
    /// Authority→driver channel — the terminal completion.
    pub event_tx: mpsc::Sender<JobEvent>,
}

/// Max turns one fork iteration may take before we cut it off (bounds a
/// runaway iteration; same spirit as the noninteractive per-role turn
/// caps in `run_noninteractive`).
const MAX_ITERATION_TURNS: usize = 8;

/// Drive an ephemeral-fork loop to termination. Always sends exactly one
/// [`JobEvent::Completed`] at the end (limit reached, self-cancel, or
/// error) so the authority's registry entry is reconciled.
pub async fn run_forked_loop(run: LoopRunCtx) {
    let LoopRunCtx {
        job_id,
        label,
        args,
        ctx,
        turn_tx,
        event_tx,
    } = run;

    // Branch a fork from main as of registration (tail snapshot). The fork
    // shares the parent's project/agent/model/provider config.
    let fork_session =
        match crate::session::Session::create_fork(ctx.session.db.clone(), ctx.session.id, None) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                let _ = event_tx
                    .send(JobEvent::Completed {
                        job_id,
                        label,
                        kind: args.kind(),
                        result: format!("loop fork failed: {e:#}"),
                        failed: true,
                        requests: Vec::new(),
                    })
                    .await;
                return;
            }
        };

    // Shared state the fork's `note` / re-routed create-actions write into.
    let state = Arc::new(ForkJobState::new(job_id.clone()));

    // Build the fork agent: the main agent's tool surface, plus `note` and
    // a fork-scoped `jobs` meta-tool (cancel-own-loop + create→request).
    let fork_agent = Arc::new(build_fork_agent(&ctx.agent, state.clone(), turn_tx.clone()));

    let limit = args.limit.unwrap_or(u64::MAX);
    let mut delay = args.interval_secs;

    // Accumulated history for `independent = false`. Reset each iteration
    // for `independent = true`.
    let mut fork_history: Vec<Message> = Vec::new();
    let mut last_result = String::new();
    let mut iteration: u64 = 0;
    let mut errored = false;

    while iteration < limit {
        // Wait the interval before each iteration (a timer with limit=1
        // therefore fires after one interval — matching "one-shot delayed
        // prompt").
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;

        if state.is_cancelled() {
            break;
        }

        if args.independent {
            fork_history.clear();
        }

        match run_iteration(
            &fork_agent,
            &mut fork_history,
            &args.prompt,
            fork_session.clone(),
            &ctx,
            &turn_tx,
        )
        .await
        {
            Ok(text) => last_result = text,
            Err(e) => {
                last_result = format!("loop iteration error: {e:#}");
                errored = true;
                break;
            }
        }

        iteration += 1;

        // The fork may have asked to cancel its own loop mid-iteration.
        if state.is_cancelled() {
            break;
        }

        if args.backoff {
            delay = (delay.saturating_mul(2)).min(super::spec::BACKOFF_CEILING_SECS);
        }
    }

    // Promote the terminal iteration's result + accumulated notes to main.
    let notes = state.take_notes();
    let requests = state.take_requests();
    let result = bundle_terminal(&label, args.kind(), iteration, &last_result, &notes);

    let _ = event_tx
        .send(JobEvent::Completed {
            job_id,
            label,
            kind: args.kind(),
            result,
            failed: errored,
            requests,
        })
        .await;
}

/// Run one iteration's turn loop in the fork. Returns the iteration's
/// final assistant text.
async fn run_iteration(
    agent: &Arc<Agent>,
    history: &mut Vec<Message>,
    prompt: &str,
    session: Arc<crate::session::Session>,
    ctx: &JobContext,
    turn_tx: &mpsc::Sender<TurnEvent>,
) -> anyhow::Result<String> {
    let mut next_prompt = Message::user(ctx.redact.scrub(prompt));
    // A loop fork is a leaf with no human on the other end — it can't
    // raise an answerable interrupt (single async-job authority, GOALS
    // §22). A detached hub satisfies the shared `turn` signature. Same for
    // cancellation: a fork isn't tied to the foreground run's ctrl+c slot
    // (it's cancelled via `jobs(loop.cancel)`), so a fresh never-cancelled
    // token keeps the signature uniform.
    let interrupts = Arc::new(crate::engine::interrupt::InterruptHub::detached());
    let cancel = tokio_util::sync::CancellationToken::new();
    for _ in 0..MAX_ITERATION_TURNS {
        let outcome = turn(
            agent,
            history,
            next_prompt,
            session.clone(),
            ctx.locks.clone(),
            ctx.redact.clone(),
            ctx.cwd.clone(),
            interrupts.clone(),
            cancel.clone(),
            // A loop fork is a leaf with no human on the other end, so it
            // can't raise an answerable approval prompt either (same
            // reason it gets a detached interrupt hub). No approver →
            // native tools skip the boundary prompt (never deny) and the
            // sandboxed shell can't escalate. The fork still runs
            // confined when sandboxing is on. The loop guard is gated on
            // an approver, so it's inert here; the threshold is irrelevant.
            None,
            crate::config::extended::MIN_LOOP_GUARD_THRESHOLD,
            // A loop/job fork runs on the session-root agent's frozen
            // system block (GOALS §22), so it benefits from the live
            // instructions-file diff injection the same as the interactive
            // root conversation (`instructions-file-live-diff.md`).
            true,
            // A loop fork is a leaf with no parent to defer to; it carries a
            // fresh empty deferred-log that nobody reads (`plan.md §3d`).
            crate::engine::deferred::DeferredLog::new(),
            // A loop fork is a leaf that never seeds to a caller (GOALS §3c);
            // a fresh empty collector satisfies the signature, never drained.
            crate::engine::seed_collector::SeedCollector::new(),
            turn_tx,
        )
        .await?;
        match outcome {
            TurnOutcome::Continue => {
                next_prompt = history
                    .pop()
                    .expect("Continue with empty history is unreachable");
            }
            TurnOutcome::Done => return Ok(collect_final_text(history)),
            // A fork is a leaf — it cannot delegate via `task`, and its
            // `jobs` tool is the in-process `ForkJobTool` (never routed as
            // `JobAction`). If a weak model somehow lands here, end the
            // iteration rather than spin.
            TurnOutcome::SpawnSubagent { .. }
            | TurnOutcome::SpawnNoninteractive { .. }
            | TurnOutcome::JobAction { .. }
            | TurnOutcome::Handoff { .. } => {
                return Ok(collect_final_text(history));
            }
        }
    }
    Ok(collect_final_text(history))
}

/// Build the ephemeral-fork agent: the parent agent's system + tools, plus
/// the `note` tool and a fork-scoped `jobs` tool that only cancels its own
/// loop and re-routes create-actions to requests.
fn build_fork_agent(
    parent: &Arc<Agent>,
    state: Arc<ForkJobState>,
    turn_tx: mpsc::Sender<TurnEvent>,
) -> Agent {
    let mut tools: ToolBox = parent.tools.clone();
    tools = tools.with(Arc::new(NoteTool::new(state.clone(), turn_tx)));
    tools = tools.with(Arc::new(ForkJobTool::new(state)));
    Agent {
        name: parent.name.clone(),
        system: parent.system.clone(),
        tools,
        model: parent.model.clone(),
        params: parent.params.clone(),
        // The fork inherits the parent's LLM mode so its tool descriptions
        // render identically (`prompts/llm-modes-defensive-normal.md`).
        llm_mode: parent.llm_mode,
    }
}

/// Bundle the terminal result + notes into the budget-capped text injected
/// into main context.
fn bundle_terminal(
    label: &str,
    kind: JobKind,
    iterations: u64,
    last_result: &str,
    notes: &[String],
) -> String {
    let mut writer = BudgetedWriter::new(ASYNC_RESULT_TOKEN_CAP);
    let _ = writer.writeln(&format!(
        "{} `{label}` ended after {iterations} iteration(s).",
        kind.as_str()
    ));
    if !notes.is_empty() {
        let _ = writer.writeln("Notes:");
        for n in notes {
            let _ = writer.writeln(&format!("- {n}"));
        }
    }
    let trimmed = last_result.trim();
    if !trimmed.is_empty() {
        let _ = writer.writeln("Final iteration:");
        let _ = writer.writeln(trimmed);
    }
    writer.into_string()
}

fn collect_final_text(history: &[Message]) -> String {
    for msg in history.iter().rev() {
        if let Message::Assistant { content, .. } = msg {
            let text = extract_text(content);
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    String::new()
}
