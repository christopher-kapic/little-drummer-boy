//! Built-in agent definitions: `orchestrator-build`, `coder`.
//!
//! The agent prompts live as Markdown documents alongside this file.
//! `include_str!` bakes them into the binary so a fresh `cargo install
//! cockpit-cli` ships with the bundled cast (GOALS §3a). User-authored
//! agents go through [`crate::agents`] / `agent_dirs`; they're the
//! extension path.

use std::sync::Arc;

use anyhow::{Result, bail};

use crate::engine::agent::Agent;
use crate::engine::model::{Model, ModelParams};
use crate::engine::tool::ToolBox;

/// Embedded prompt for `orchestrator-build`. The frontmatter is
/// authored opencode-style for forward-compat with [`crate::agents`]
/// — we still pull the prompt out by hand here because the agent loop
/// already knows the tool surface.
const ORCHESTRATOR_BUILD_PROMPT: &str = include_str!("orchestrator_build.md");
const CODER_PROMPT: &str = include_str!("coder.md");
const EXPLORE_PROMPT: &str = include_str!("explore.md");

/// Per-spawn knobs threaded from the driver.
#[derive(Clone)]
pub struct SpawnArgs {
    pub model: Arc<Model>,
    pub params: ModelParams,
}

/// Build a built-in agent by name. Returns `Err` for unknown names so
/// the `task` tool can surface "unknown agent" loudly rather than
/// silently spawning the wrong one.
pub fn load(name: &str, args: &SpawnArgs) -> Result<Agent> {
    match name {
        "orchestrator-build" => Ok(orchestrator_build(args)),
        "coder" => Ok(coder(args)),
        "explore" => Ok(explore(args)),
        other => bail!("unknown built-in agent `{other}`"),
    }
}

/// True if `name` denotes a built-in agent that runs *noninteractively*
/// — the orchestrator dispatches it like a tool call (synchronously)
/// rather than handing the primary conversation off. The driver uses
/// this to route `task(agent=…, …)` correctly.
pub fn is_noninteractive(name: &str) -> bool {
    matches!(name, "explore")
}

/// `orchestrator-build` — the user-facing primary agent. Owns the chat
/// when the focus is *making the change* (GOALS §3a). Delegates writes
/// to `coder` via `task`.
pub fn orchestrator_build(args: &SpawnArgs) -> Agent {
    let tools = ToolBox::new()
        .with(Arc::new(crate::tools::read::ReadTool))
        .with(Arc::new(crate::tools::bash::BashTool::new()))
        .with(Arc::new(crate::tools::task::TaskTool::with_subagents(&[
            "coder", "explore",
        ])));

    Agent {
        name: "orchestrator-build".to_string(),
        system: ORCHESTRATOR_BUILD_PROMPT.to_string(),
        tools,
        model: args.model.clone(),
        params: args.params.clone(),
        array_fields: Vec::new(),
    }
}

/// `coder` — the only agent that writes. Holds file locks; runs bash;
/// applies edits. Caller-determined interactivity: interactive when
/// spawned from `orchestrator-build` (GOALS §3a/§3b).
pub fn coder(args: &SpawnArgs) -> Agent {
    let tools = ToolBox::new()
        .with(Arc::new(crate::tools::read::ReadTool))
        .with(Arc::new(crate::tools::readlock::ReadlockTool))
        .with(Arc::new(crate::tools::writeunlock::WriteunlockTool))
        .with(Arc::new(crate::tools::unlock::UnlockTool))
        .with(Arc::new(crate::tools::editunlock::EditunlockTool))
        .with(Arc::new(crate::tools::bash::BashTool::new()));

    Agent {
        name: "coder".to_string(),
        system: CODER_PROMPT.to_string(),
        tools,
        model: args.model.clone(),
        params: args.params.clone(),
        array_fields: Vec::new(),
    }
}

/// `explore` — read-only investigator. Leaf in the invocation tree
/// (no `task` of its own). Runs noninteractively from
/// `orchestrator-build`'s perspective: the orchestrator dispatches it
/// via `task(agent="explore", …)` and gets a single text report back
/// as the tool result. The user sees the call rendered like any other
/// tool in the orchestrator's history.
pub fn explore(args: &SpawnArgs) -> Agent {
    let tools = ToolBox::new()
        .with(Arc::new(crate::tools::read::ReadTool))
        .with(Arc::new(crate::tools::bash::BashTool::new()));

    Agent {
        name: "explore".to_string(),
        system: EXPLORE_PROMPT.to_string(),
        tools,
        model: args.model.clone(),
        params: args.params.clone(),
        array_fields: Vec::new(),
    }
}
