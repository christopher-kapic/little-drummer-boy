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
    /// The explicit, steering [`LlmMode::Defensive`] description, built
    /// from the same subagent list (`prompts/llm-modes-defensive-normal.md`).
    defensive_description: String,
    parameters: Value,
    /// The defensive parameter schema — same shape + `enum` + required set
    /// as `parameters`, with explicit parameter descriptions.
    defensive_parameters: Value,
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
        // Defensive (`LlmMode::Defensive`) steering: decompose harder and
        // route narrow pieces through subagents so each does one focused job
        // in its own context and returns a small report
        // (`prompts/llm-modes-defensive-normal.md`). Single-writer +
        // leaf-termination are unchanged — they hold in both modes.
        let defensive_description = format!(
            "Hand a single, well-scoped piece of work to a subagent ({list}) instead of doing it \
             yourself inline. Prefer this for any non-trivial sub-task: break the work into \
             narrow pieces and delegate each one, so the subagent does its focused job in its \
             own context and returns just a short report — keeping your own context lean. Write \
             `prompt` as a complete, standalone brief: the goal, the constraints, the exact \
             files involved, and what \"done\" looks like — the subagent does NOT see your \
             conversation. An interactive subagent (e.g. the writer or the planning interviewer) \
             takes over the conversation with the user; the others run on their own and report \
             back. Only `coder` may write files, in either case."
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
        let defensive_parameters = serde_json::json!({
            "type": "object",
            "properties": {
                "agent":  {
                    "type": "string",
                    "description": "The subagent to delegate to; must be one of the listed names",
                    "enum": agents
                },
                "prompt": {
                    "type": "string",
                    "description": "A complete, standalone brief for the subagent: its goal, the constraints, the exact files in scope, and what \"done\" looks like. The subagent cannot see this conversation, so include everything it needs"
                },
                "mode": {
                    "type": "string",
                    "description": "Optional override of the subagent's default interactivity: `subagent` runs it noninteractively (it reports back), `subagent_interactive` lets it take over the conversation with the user. Omit to use the subagent's default",
                    "enum": ["subagent", "subagent_interactive"]
                }
            },
            "required": ["agent", "prompt"]
        });
        Self {
            description,
            defensive_description,
            parameters,
            defensive_parameters,
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

    fn defensive_description(&self) -> Option<String> {
        Some(self.defensive_description.clone())
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn defensive_parameters(&self) -> Option<Value> {
        Some(self.defensive_parameters.clone())
    }

    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        Err(anyhow::anyhow!(
            "`task` is intercepted by the engine dispatcher; this code path should be unreachable"
        ))
    }
}
