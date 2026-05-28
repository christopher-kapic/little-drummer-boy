//! `jobs` meta-tool + the fork-only `note` channel (GOALS §22).
//!
//! ## Cache-safety
//!
//! The `jobs` meta-tool's schema is **fixed and minimal** (`action` +
//! `args`). It never changes across a conversation, so the serialized
//! tools array is byte-stable and capability growth never busts the
//! prompt cache. Branches are enabled by two cache-safe moves elsewhere:
//! the dispatcher (driver) starts accepting the action, and an appended
//! **hint message** tells the model the action is available (appended
//! messages extend the cached prefix; they don't reserialize the tools
//! block).
//!
//! ## Two tool surfaces
//!
//! - [`JobsTool`] — the main-context meta-tool. Like `task`, it is a
//!   *structural* tool the engine intercepts by name: the driver owns the
//!   single [`crate::engine::jobs::JobAuthority`], so the action is
//!   dispatched there, not here. The trait impl exists only to advertise
//!   the fixed schema in one place; calling it directly is a loud error.
//! - [`ForkJobTool`] + [`NoteTool`] — injected into ephemeral-fork loop
//!   iterations. `note` is the only fork→main channel; the fork-scoped
//!   `jobs` cancels *its own* loop and re-routes create-actions to
//!   requests (forks cannot spawn async work — anti-runaway).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::engine::agent::TurnEvent;
use crate::engine::jobs::spec::{
    JobAction, SpawnRequest, parse_action, parse_background_start, parse_loop_start,
};
use crate::engine::tool::{Tool, ToolCtx, ToolOutput, invalid_input};

/// The fixed minimal schema for the `jobs` meta-tool. **Byte-stable** for
/// the conversation's lifetime — see the module docs and the
/// `tools_array_is_byte_stable` test in [`crate::engine::driver`].
pub const JOBS_DESCRIPTION: &str = "Schedule async work: loop.start (set limit=1 for a one-shot timer), loop.cancel, background.start, background.tail, background.cancel, list";

/// Build the `jobs` meta-tool's JSON schema. Kept in a free function so
/// the byte-stability test can assert on it directly.
pub fn jobs_parameters() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "description": "Branch: loop.start/loop.cancel/background.start/background.tail/background.cancel/list"
            },
            "args": {
                "type": "object",
                "description": "Per-action arguments"
            }
        },
        "required": ["action"]
    })
}

/// The main-context `jobs` meta-tool. Structural: intercepted by the
/// engine dispatcher (see [`crate::engine::agent::turn`]), which routes
/// the action to the driver-owned authority.
pub struct JobsTool;

#[async_trait]
impl Tool for JobsTool {
    fn name(&self) -> &str {
        "jobs"
    }

    fn description(&self) -> &str {
        JOBS_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        jobs_parameters()
    }

    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        Err(anyhow::anyhow!(
            "`jobs` is intercepted by the engine dispatcher; this code path should be unreachable"
        ))
    }
}

/// Pull the `action` string + the `args` object out of a repaired `jobs`
/// call. `args` defaults to an empty object when omitted.
pub fn split_action(call_args: &Value) -> Result<(JobAction, Value)> {
    let action_str = call_args
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_input("`action` is required"))?;
    let action = parse_action(action_str)?;
    let args = call_args
        .get("args")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    Ok((action, args))
}

// ---- Fork-only tools -------------------------------------------------------

/// Shared state the ephemeral-fork loop's tools write into and the
/// loop runner reads at termination. Notes and re-routed create-requests
/// accumulate here; `cancelled` flips when the fork cancels its own loop.
pub struct ForkJobState {
    /// The job id this fork's loop owns — `loop.cancel` must match it.
    own_job_id: String,
    notes: Mutex<Vec<String>>,
    requests: Mutex<Vec<SpawnRequest>>,
    cancelled: std::sync::atomic::AtomicBool,
}

impl ForkJobState {
    pub fn new(own_job_id: String) -> Self {
        Self {
            own_job_id,
            notes: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn push_note(&self, text: String) {
        self.notes.lock().unwrap().push(text);
    }

    fn push_request(&self, req: SpawnRequest) {
        self.requests.lock().unwrap().push(req);
    }

    fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Drain accumulated notes (called once at termination).
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().unwrap())
    }

    /// Drain accumulated spawn-requests (called once at termination).
    pub fn take_requests(&self) -> Vec<SpawnRequest> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }
}

/// `note(text)` — the only fork→main channel. Shown live in the UI (via a
/// [`TurnEvent::JobNote`]); enters main context only at loop termination,
/// bundled with the terminal result.
pub struct NoteTool {
    state: Arc<ForkJobState>,
    turn_tx: mpsc::Sender<TurnEvent>,
}

impl NoteTool {
    pub fn new(state: Arc<ForkJobState>, turn_tx: mpsc::Sender<TurnEvent>) -> Self {
        Self { state, turn_tx }
    }
}

#[async_trait]
impl Tool for NoteTool {
    fn name(&self) -> &str {
        "note"
    }

    fn description(&self) -> &str {
        "Surface a progress note to the human now; it reaches the main conversation only at loop end"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Progress note" }
            },
            "required": ["text"]
        })
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid_input("`text` is required"))?
            .to_string();
        // Live UI signal (never enters main context here — token economy).
        let _ = self.turn_tx.try_send(TurnEvent::JobNote {
            job_id: self.state.own_job_id.clone(),
            text: text.clone(),
        });
        self.state.push_note(text);
        Ok(ToolOutput::text("noted"))
    }
}

/// The fork-scoped `jobs` meta-tool. Same fixed schema as [`JobsTool`] so
/// the fork's tools array is byte-stable too. Behaviour differs:
/// `loop.cancel` ends *this fork's own* loop; create-actions
/// (`loop.start`/`background.start`) do **not** execute — they record a
/// [`SpawnRequest`] routed to main (anti-runaway). Other actions are
/// rejected with a clear message.
pub struct ForkJobTool {
    state: Arc<ForkJobState>,
}

impl ForkJobTool {
    pub fn new(state: Arc<ForkJobState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for ForkJobTool {
    fn name(&self) -> &str {
        "jobs"
    }

    fn description(&self) -> &str {
        JOBS_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        jobs_parameters()
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let (action, action_args) = split_action(&args)?;
        match action {
            JobAction::LoopCancel => {
                // A fork may cancel its own loop. The job_id arg is
                // tolerated but the fork only owns one loop, so we don't
                // require a match — cancelling is always self-scoped here.
                self.state.cancel();
                Ok(ToolOutput::text(
                    "loop will end after this iteration completes",
                ))
            }
            JobAction::LoopStart => {
                let parsed = parse_loop_start(&action_args)?;
                let summary = SpawnRequest::Loop(parsed.clone()).summary();
                self.state.push_request(SpawnRequest::Loop(parsed));
                Ok(ToolOutput::text(format!(
                    "request recorded — a fork cannot spawn jobs; the main agent will decide whether to start `{summary}`"
                )))
            }
            JobAction::BackgroundStart => {
                let parsed = parse_background_start(&action_args)?;
                let summary = SpawnRequest::Background(parsed.clone()).summary();
                self.state.push_request(SpawnRequest::Background(parsed));
                Ok(ToolOutput::text(format!(
                    "request recorded — a fork cannot spawn jobs; the main agent will decide whether to start `{summary}`"
                )))
            }
            JobAction::BackgroundTail | JobAction::BackgroundCancel | JobAction::List => {
                Err(invalid_input(format!(
                    "`{}` is only available in the main conversation, not inside a loop",
                    action.as_str()
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The core caching invariant (GOALS §22): the serialized tools array
    /// containing the `jobs` meta-tool is **byte-identical** no matter
    /// which branches have been exercised. Branch-enabling is an appended
    /// hint message + dispatch acceptance — never a mutation of the tool's
    /// schema — so the cached prefix is never busted.
    #[test]
    fn tools_array_is_byte_stable_across_branch_enabling() {
        use crate::engine::tool::ToolBox;

        // The tools array a conversation carries (here: just `jobs`; the
        // real orchestrator adds more, but they're equally immutable).
        let toolbox = ToolBox::new().with(Arc::new(JobsTool));
        let before = serde_json::to_string(&toolbox.definitions()).unwrap();

        // Simulate "enabling every branch": the meta-tool's schema is the
        // same object regardless of action. Re-derive it for each branch
        // and confirm nothing about the advertised tool changes.
        for action in [
            "loop.start",
            "loop.cancel",
            "background.start",
            "background.tail",
            "background.cancel",
            "list",
        ] {
            // The action is accepted at dispatch (parses) — that's the
            // cache-safe acceptance half of enabling a branch.
            assert!(parse_action(action).is_ok());
            // The tool definition is unchanged after "enabling" it.
            let after = serde_json::to_string(&toolbox.definitions()).unwrap();
            assert_eq!(
                before, after,
                "tools array changed after enabling `{action}`"
            );
        }

        // And the schema itself is deterministic byte-for-byte.
        assert_eq!(
            serde_json::to_string(&jobs_parameters()).unwrap(),
            serde_json::to_string(&jobs_parameters()).unwrap()
        );
    }

    #[test]
    fn split_action_parses() {
        let (a, args) = split_action(&json!({
            "action": "loop.start",
            "args": { "interval": 30, "prompt": "p" }
        }))
        .unwrap();
        assert_eq!(a, JobAction::LoopStart);
        assert_eq!(args["interval"], 30);
    }

    #[test]
    fn split_action_defaults_args_to_empty_object() {
        let (a, args) = split_action(&json!({ "action": "list" })).unwrap();
        assert_eq!(a, JobAction::List);
        assert!(args.as_object().unwrap().is_empty());
    }

    #[test]
    fn split_action_unknown_errors() {
        assert!(split_action(&json!({ "action": "bogus" })).is_err());
        assert!(split_action(&json!({})).is_err());
    }

    #[tokio::test]
    async fn fork_jobs_routes_create_to_request() {
        let state = Arc::new(ForkJobState::new("job-abc".into()));
        let tool = ForkJobTool::new(state.clone());
        let ctx = test_ctx();
        let out = tool
            .call(
                json!({ "action": "background.start", "args": { "command": "cargo test" } }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.content.contains("request recorded"));
        let reqs = state.take_requests();
        assert_eq!(reqs.len(), 1);
        assert!(matches!(reqs[0], SpawnRequest::Background(_)));
    }

    #[tokio::test]
    async fn fork_jobs_cancel_sets_flag() {
        let state = Arc::new(ForkJobState::new("job-abc".into()));
        let tool = ForkJobTool::new(state.clone());
        let ctx = test_ctx();
        assert!(!state.is_cancelled());
        tool.call(json!({ "action": "loop.cancel" }), &ctx)
            .await
            .unwrap();
        assert!(state.is_cancelled());
    }

    #[tokio::test]
    async fn fork_jobs_rejects_main_only_actions() {
        let state = Arc::new(ForkJobState::new("job-abc".into()));
        let tool = ForkJobTool::new(state);
        let ctx = test_ctx();
        assert!(tool.call(json!({ "action": "list" }), &ctx).await.is_err());
        assert!(
            tool.call(
                json!({ "action": "background.tail", "args": {"job_id":"x"} }),
                &ctx
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn note_records_and_signals() {
        let state = Arc::new(ForkJobState::new("job-abc".into()));
        let (tx, mut rx) = mpsc::channel(8);
        let tool = NoteTool::new(state.clone(), tx);
        let ctx = test_ctx();
        tool.call(json!({ "text": "halfway there" }), &ctx)
            .await
            .unwrap();
        let notes = state.take_notes();
        assert_eq!(notes, vec!["halfway there".to_string()]);
        match rx.try_recv().unwrap() {
            TurnEvent::JobNote { text, .. } => assert_eq!(text, "halfway there"),
            other => panic!("expected JobNote, got {other:?}"),
        }
    }

    /// Minimal `ToolCtx` for unit-testing fork tools (they don't touch the
    /// session / locks / cwd).
    fn test_ctx() -> ToolCtx {
        crate::tools::common::test_ctx(std::path::Path::new("/"))
    }
}
