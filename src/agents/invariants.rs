//! Core-invariant validation for user-loadable agent definitions
//! (edited built-ins + custom agents). Enforced at load time with a
//! clear, actionable error per the project error-style (backticks for
//! identifiers/literals).
//!
//! Two invariants gate the editable `tools:` grant
//! (`prompts/user-definable-agents.md`):
//!
//!   1. **Single-writer** (GOALS §3a / `CLAUDE.md`): the file-mutating +
//!      lock tools that today only `coder` holds may be granted to **at
//!      most the one writer** in a delegation tree. Any non-`coder` agent
//!      requesting one is rejected — the lock manager assumes a single
//!      writer.
//!   2. **Docs-answerer sandbox**: the sandboxed `grep`/`glob` tools are
//!      Docs.2-only (`CLAUDE.md`). No user agent may acquire them.
//!
//! Unknown tool names are rejected with the offending name backticked.

use anyhow::{Result, bail};

use super::AgentDef;

/// The file-mutating + lock tools. Today only `coder` holds these; the
/// single-writer rule lets at most one writer in a delegation tree have
/// them. Sourced from the `coder` factory's tool surface in
/// [`crate::engine::builtin`].
pub const LOCK_WRITE_TOOLS: &[&str] = &["readlock", "writeunlock", "editunlock", "unlock"];

/// The docs-answerer-only sandboxed search tools (Docs.2). Never
/// grantable to a user agent — they exist solely so the docs answerer can
/// explore a cloned dependency without shell access, hard-confined to its
/// package root.
pub const SANDBOX_ONLY_TOOLS: &[&str] = &["grep", "glob"];

/// The agent name permitted to hold the [`LOCK_WRITE_TOOLS`] — the single
/// writer.
const WRITER_AGENT: &str = "coder";

/// Every tool name a user-facing agent may legitimately *name* in its
/// `tools:` frontmatter. This is the union of:
///   - the read/inspect tools every agent can use,
///   - the codebase-intelligence tools (GOALS §21),
///   - the interactive/structural tools (`task`, `skill`, `question`,
///     `jobs`),
///   - the cross-session recall tools (registered only on interactive
///     spawns, but a valid name to grant),
///   - the single-writer lock/write tools (only valid for `coder`, but a
///     *known* name — the single-writer check rejects them for others
///     with a more specific message than "unknown tool"),
///   - the sandbox tools (Docs.2-only — known names, rejected by the
///     sandbox check).
///
/// User-defined custom-bash tools (`webfetch`/`websearch`/…) are *not*
/// listed: they are config-driven and resolved separately onto the
/// toolbox, so naming them in `tools:` is not how they're granted.
pub fn known_tool_names() -> &'static [&'static str] {
    &[
        // read / inspect
        "read",
        "bash",
        // intel (GOALS §21)
        "tree",
        "outline",
        "symbol_find",
        "word",
        "deps",
        "hot",
        "circular",
        "search",
        // structural / interactive
        "task",
        "skill",
        "question",
        "jobs",
        // `Auto`'s structural front-door handoff tool (`src/tools/handoff.rs`).
        // Holds no write/lock, so any agent may name it.
        "handoff",
        // planning tools (`src/tools/plan.rs`) + the subagent deferral tool
        // (`plan.md §3d`). None hold write/lock, so any agent may grant them.
        "plan_create",
        "add_step",
        "add_step_dependency",
        "plan_set_branches",
        "plan_list",
        "defer_to_orchestrator",
        // cross-session recall (interactive-only at spawn)
        "session_search",
        "session_read",
        // single-writer lock/write set
        "readlock",
        "writeunlock",
        "editunlock",
        "unlock",
        // docs-answerer sandbox (rejected for user agents)
        "grep",
        "glob",
    ]
}

/// Validate `def` against the core invariants. Returns `Ok(())` when the
/// definition is admissible, else an `Err` whose message names the
/// specific reason (the offending tool / agent, backticked). The
/// offending tool is **never** silently stripped.
pub fn validate_invariants(def: &AgentDef) -> Result<()> {
    let Some(tools) = &def.tools else {
        // No explicit tool grant — the agent inherits its role-default
        // surface from the factory; nothing to validate here.
        return Ok(());
    };

    let known = known_tool_names();
    for tool in tools {
        // Unknown tool name.
        if !known.contains(&tool.as_str()) {
            bail!("agent `{}` requests unknown tool `{tool}`", def.name);
        }
        // Docs-answerer sandbox: never grantable to a user agent.
        if SANDBOX_ONLY_TOOLS.contains(&tool.as_str()) {
            bail!(
                "agent `{}` may not use the docs-answerer-only sandboxed tool `{tool}`",
                def.name
            );
        }
        // Single-writer: write/lock tools only for the one writer.
        if LOCK_WRITE_TOOLS.contains(&tool.as_str()) && def.name != WRITER_AGENT {
            bail!(
                "agent `{}` may not hold the write/lock tool `{tool}` — only `{WRITER_AGENT}` writes files (single-writer rule)",
                def.name
            );
        }
    }
    Ok(())
}
