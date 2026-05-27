//! Built-in agent definitions: `orchestrator-build`, `coder`.
//!
//! The agent prompts live as Markdown documents alongside this file.
//! `include_str!` bakes them into the binary so a fresh `cargo install
//! cockpit-cli` ships with the bundled cast (GOALS §3a). User-authored
//! agents go through [`crate::agents`] / `agent_dirs`; they're the
//! extension path.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::config::dirs::discover_config_dirs;
use crate::config::extended::{ExtendedConfigDoc, ToolCommandTemplate};
use crate::engine::agent::Agent;
use crate::engine::model::{Model, ModelParams};
use crate::engine::tool::ToolBox;
use crate::tools::custom::CustomBashTool;

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
    /// Session cwd — used to discover the layered `extended-config.json`
    /// so user-defined custom-bash tools (`webfetch`, `websearch`, …)
    /// land on the toolbox for agents that should see them.
    pub cwd: std::path::PathBuf,
    /// 6-char session display id (GOALS §17b). Appended to the cached
    /// system prompt (§17g) so the model knows which conversation it
    /// is participating in. Empty string is acceptable for legacy /
    /// test paths where a session id isn't yet resolved.
    pub session_short_id: String,
}

/// Append the per-session lines (OS + session id) to the role-specific
/// prompt before handing it to [`Agent::system`]. Per GOALS §17g these
/// stay inside the cached system block — both fields are stable for
/// the session's lifetime so prompt-cache hits aren't disturbed.
fn compose_system_prompt(role_prompt: &str, session_short_id: &str) -> String {
    let os = crate::sysinfo::os_string();
    let mut out = String::with_capacity(role_prompt.len() + 96);
    out.push_str(role_prompt);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str("Operating system: ");
    out.push_str(&os);
    out.push('\n');
    if !session_short_id.is_empty() {
        out.push_str("Session: ");
        out.push_str(session_short_id);
        out.push('\n');
    }
    out
}

/// Load user-defined custom-bash tools from the first `extended-config.json`
/// on the layered-config path and append them to `tb`. Falls back to the
/// shipped defaults for any built-in tool name the user hasn't configured.
/// Disabled rows and empty commands are skipped.
fn with_custom_tools(mut tb: ToolBox, cwd: &Path) -> ToolBox {
    let cfg = discover_config_dirs(cwd)
        .into_iter()
        .find_map(|d| ExtendedConfigDoc::load(&d.path.join("extended-config.json")).ok())
        .map(|d| d.config())
        .unwrap_or_default();

    for (name, tpl) in cfg.tools.iter() {
        if !tpl.enabled || tpl.command.trim().is_empty() {
            continue;
        }
        tb = tb.with(Arc::new(CustomBashTool::from_template(name, tpl)));
    }
    for name in crate::tui::settings::builtin_tool_names() {
        if cfg.tools.contains_key(*name) {
            continue;
        }
        let tpl: ToolCommandTemplate = crate::tui::settings::default_template_for(name);
        if tpl.enabled && !tpl.command.trim().is_empty() {
            tb = tb.with(Arc::new(CustomBashTool::from_template(name, &tpl)));
        }
    }
    tb
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_system_prompt_appends_os_and_session() {
        let out = compose_system_prompt("ROLE PROMPT", "abc123");
        assert!(out.starts_with("ROLE PROMPT"));
        assert!(out.contains("Operating system:"));
        assert!(out.contains("Session: abc123"));
    }

    #[test]
    fn compose_system_prompt_omits_session_when_empty() {
        let out = compose_system_prompt("ROLE PROMPT", "");
        assert!(out.contains("Operating system:"));
        assert!(!out.contains("Session:"));
    }

    #[test]
    fn compose_system_prompt_normalizes_trailing_newline() {
        let with_nl = compose_system_prompt("ROLE\n", "abc123");
        let without_nl = compose_system_prompt("ROLE", "abc123");
        // The role-prompt's own newline is preserved either way; the
        // appended lines are identical in both cases.
        assert!(with_nl.contains("\nOperating system:"));
        assert!(without_nl.contains("\nOperating system:"));
    }
}

/// `orchestrator-build` — the user-facing primary agent. Owns the chat
/// when the focus is *making the change* (GOALS §3a). Delegates writes
/// to `coder` via `task`.
pub fn orchestrator_build(args: &SpawnArgs) -> Agent {
    let tools = with_custom_tools(
        ToolBox::new()
            .with(Arc::new(crate::tools::read::ReadTool))
            .with(Arc::new(crate::tools::bash::BashTool::new()))
            .with(Arc::new(crate::tools::task::TaskTool::with_subagents(&[
                "coder", "explore",
            ]))),
        &args.cwd,
    );

    Agent {
        name: "orchestrator-build".to_string(),
        system: compose_system_prompt(ORCHESTRATOR_BUILD_PROMPT, &args.session_short_id),
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
        system: compose_system_prompt(CODER_PROMPT, &args.session_short_id),
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
    let tools = with_custom_tools(
        ToolBox::new()
            .with(Arc::new(crate::tools::read::ReadTool))
            .with(Arc::new(crate::tools::bash::BashTool::new())),
        &args.cwd,
    );

    Agent {
        name: "explore".to_string(),
        system: compose_system_prompt(EXPLORE_PROMPT, &args.session_short_id),
        tools,
        model: args.model.clone(),
        params: args.params.clone(),
        array_fields: Vec::new(),
    }
}
