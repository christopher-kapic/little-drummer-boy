//! `task` — delegate to a subagent.
//!
//! This is a structural tool: the engine's [`crate::engine::agent::turn`]
//! special-cases the name `task` and returns
//! [`crate::engine::agent::TurnOutcome::SpawnSubagent`] instead of
//! dispatching here. We still implement the trait so the tool
//! definition (name + description + parameter schema) advertises in
//! exactly one place — the agent.rs dispatcher loop is what enforces
//! the contract.
//!
//! If this ever runs (it shouldn't), we return an error so the
//! divergence is loud rather than silent.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::engine::tool::{Tool, ToolCtx, ToolOutput};

pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Delegate a scoped piece of work to a subagent (currently: coder); user sees the subagent's actions"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent":  {
                    "type": "string",
                    "description": "Subagent to spawn (only `coder` is available in v0)",
                    "enum": ["coder"]
                },
                "prompt": {
                    "type": "string",
                    "description": "Self-contained brief: goal, constraints, files, what \"done\" looks like"
                }
            },
            "required": ["agent", "prompt"]
        })
    }

    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        Err(anyhow::anyhow!(
            "`task` is intercepted by the engine dispatcher; this code path should be unreachable"
        ))
    }
}
