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

pub struct TaskTool {
    description: String,
    parameters: Value,
}

impl TaskTool {
    /// Build the tool with the agent enum populated from the caller's
    /// available subagents — keeps the schema honest so the model
    /// can't ask to delegate to an agent that doesn't exist.
    ///
    /// `mode` is an optional override of the per-agent default
    /// interactivity. Omitted, the engine routes by the agent's own default
    /// (`coder`/`plan-author` are interactive handoffs; everything else runs
    /// noninteractively). The explicit value is the seam the future
    /// LLM-strategy axis switches on (`design-need-to-discuss-or-test.md`):
    /// the interactive-subagent path is the one wired today.
    pub fn with_subagents(agents: &[&str]) -> Self {
        let list = agents.join("/");
        let description = format!(
            "Delegate a scoped piece of work to a subagent ({list}); an interactive subagent takes over the conversation, others run noninteractively"
        );
        let parameters = serde_json::json!({
            "type": "object",
            "properties": {
                "agent":  {
                    "type": "string",
                    "description": "Subagent name",
                    "enum": agents
                },
                "prompt": {
                    "type": "string",
                    "description": "Self-contained brief: goal, constraints, files, what \"done\" looks like"
                },
                "mode": {
                    "type": "string",
                    "description": "Delegation mode override",
                    "enum": ["subagent", "subagent_interactive"]
                }
            },
            "required": ["agent", "prompt"]
        });
        Self {
            description,
            parameters,
        }
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        Err(anyhow::anyhow!(
            "`task` is intercepted by the engine dispatcher; this code path should be unreachable"
        ))
    }
}
