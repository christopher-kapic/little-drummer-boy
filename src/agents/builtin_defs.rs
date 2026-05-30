//! Embedded default [`AgentDef`]s for the bundled cast.
//!
//! The agent prompt bodies live as `include_str!`-baked markdown in
//! [`crate::engine::builtin`]; this module wraps each with the
//! frontmatter (description / mode / tool surface) that the hardcoded
//! factory functions encode in Rust. Together they are the fallback
//! definition for a built-in when no on-disk override exists — and the
//! faithful source eject writes to `<config_dir>/agents/<name>.md`.
//!
//! In scope: every bundled agent **except the docs pipeline**. The docs
//! resolver/answerer are a fixed two-stage pipeline (GOALS §3a), never an
//! [`AgentDef`], so they are absent here.
//!
//! `model`/`temperature` are left `None` on the defaults: a built-in
//! inherits the session's active model + params unless the user sets an
//! override in the ejected file. `tools` is the explicit role surface so
//! the engine can rebuild the toolbox from an edited grant.

use std::path::PathBuf;

use super::{AgentDef, AgentMode};

/// Names of the built-in agents in scope for user editing, in canonical
/// listing order. Drives the override-resolution, listing, and reset
/// paths. Driven off the code (the factory functions), not docs: `Plan`
/// is documented in `CLAUDE.md` but no such agent ships yet, so it is
/// **not** here.
pub const BUILTIN_AGENT_NAMES: &[&str] = &["Build", "coder", "explore"];

/// True when `name` is one of the editable built-in agents.
pub fn is_builtin_agent(name: &str) -> bool {
    BUILTIN_AGENT_NAMES.contains(&name)
}

/// The embedded default [`AgentDef`] for a built-in `name`, or `None`
/// when `name` is not a built-in. The `prompt` is the same body the
/// factory functions compose into the system prompt.
pub fn embedded_default(name: &str) -> Option<AgentDef> {
    match name {
        "Build" => Some(build_def()),
        "coder" => Some(coder_def()),
        "explore" => Some(explore_def()),
        _ => None,
    }
}

fn def(name: &str, description: &str, mode: AgentMode, tools: &[&str], prompt: &str) -> AgentDef {
    AgentDef {
        name: name.to_string(),
        description: description.to_string(),
        mode,
        model: None,
        temperature: None,
        tools: Some(tools.iter().map(|t| t.to_string()).collect()),
        permission: None,
        // Trim the trailing newline the `include_str!` body carries so an
        // embedded default and the same agent re-parsed from its ejected
        // file compare byte-equal (eject faithfulness).
        prompt: prompt.trim_end().to_string(),
        // Embedded defaults have no on-disk source.
        source: PathBuf::new(),
    }
}

/// `Build` — the user-facing primary agent (GOALS §3a). Delegates writes
/// to `coder` via `task`. Tool surface mirrors
/// [`crate::engine::builtin::build`].
fn build_def() -> AgentDef {
    def(
        "Build",
        "Primary coding agent; decides the change and delegates writes to `coder`.",
        AgentMode::Primary,
        &[
            "read", "bash", "tree", "hot", "jobs", "question", "skill", "task",
        ],
        crate::engine::builtin::BUILD_PROMPT,
    )
}

/// `coder` — the single writer (holds file locks). Tool surface mirrors
/// [`crate::engine::builtin::coder`]; the only agent that may hold the
/// write/lock tools (single-writer rule).
fn coder_def() -> AgentDef {
    def(
        "coder",
        "The only agent that writes files; holds locks and applies edits.",
        AgentMode::Subagent,
        &[
            "read",
            "readlock",
            "writeunlock",
            "unlock",
            "editunlock",
            "bash",
            "outline",
            "symbol_find",
            "deps",
            "circular",
            "word",
            "search",
            "question",
            "skill",
            "task",
        ],
        crate::engine::builtin::CODER_PROMPT,
    )
}

/// `explore` — read-only investigator, leaf in the invocation tree. Tool
/// surface mirrors [`crate::engine::builtin::explore`].
fn explore_def() -> AgentDef {
    def(
        "explore",
        "Read-only investigator; finds where things live and reports back.",
        AgentMode::Subagent,
        &[
            "read",
            "bash",
            "tree",
            "outline",
            "symbol_find",
            "word",
            "deps",
            "hot",
            "circular",
            "search",
        ],
        crate::engine::builtin::EXPLORE_PROMPT,
    )
}
