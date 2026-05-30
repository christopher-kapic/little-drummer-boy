//! Built-in agent definitions: `Build`, `coder`.
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

/// Embedded prompt for `Build`. The frontmatter is
/// authored opencode-style for forward-compat with [`crate::agents`]
/// — we still pull the prompt out by hand here because the agent loop
/// already knows the tool surface.
pub(crate) const BUILD_PROMPT: &str = include_str!("build.md");
pub(crate) const CODER_PROMPT: &str = include_str!("coder.md");
pub(crate) const EXPLORE_PROMPT: &str = include_str!("explore.md");
/// Docs pipeline stage prompts (GOALS §3a, prompt `docs-agent.md`).
const DOCS_RESOLVER_PROMPT: &str = include_str!("docs_resolver.md");
const DOCS_ANSWERER_PROMPT: &str = include_str!("docs_answerer.md");

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
    /// Whether this agent is being spawned into a user-facing
    /// interactive session (the daemon root, or an interactive handoff
    /// such as `coder`) versus a one-shot leaf delegation
    /// (`run_noninteractive`) or the `docs` pipeline. Gates the
    /// cross-session recall tools (`session_search` / `session_read`):
    /// they're registered only when `true`, so non-interactive contexts
    /// don't pay their description tokens (token economy, GOALS §10).
    /// This is the spawn-time analog of the runtime
    /// [`crate::engine::interrupt::InterruptHub::is_interactive_attached`]
    /// gate — the existing interactive-mode signal, not a new one.
    pub interactive: bool,
}

/// Append the cross-session recall tools (`session_search` /
/// `session_read`, prompt `search-old-sessions.md`) to `tb` when this
/// spawn is interactive. Centralized so every user-facing agent shares
/// one gate rather than each re-spelling the pair + the `interactive`
/// check.
fn with_recall_tools(tb: ToolBox, args: &SpawnArgs) -> ToolBox {
    if !args.interactive {
        return tb;
    }
    tb.with(Arc::new(crate::tools::session_search::SessionSearchTool))
        .with(Arc::new(crate::tools::session_read::SessionReadTool))
}

/// Append the per-session lines (harness identity + version + URLs +
/// optional user name + OS + session id) to the role-specific prompt
/// before handing it to [`Agent::system`]. Per GOALS §17g these stay
/// inside the cached system block — every field is stable for the
/// session's lifetime so prompt-cache hits aren't disturbed; the line
/// order is fixed so identical inputs produce a byte-identical block.
///
/// Also injects the first matching project-guidance file
/// (`extended.agent_guidance_files`, default `AGENTS.md`) found by
/// walking from `cwd` up to the git root. Picking the first match keeps
/// the injection deterministic when multiple legacy names exist. The
/// layered config is loaded once here and reused for both the user name
/// and the guidance-file lookup.
fn compose_system_prompt(role_prompt: &str, session_short_id: &str, cwd: &Path) -> String {
    let cfg = load_extended_config(cwd);
    compose_system_prompt_with(role_prompt, session_short_id, cwd, &cfg)
}

/// Pure assembler for the cached system block, given an already-resolved
/// [`ExtendedConfig`]. Split out from [`compose_system_prompt`] so the
/// formatting (line order, name trim/omit) is testable without depending
/// on which layered config the discovery walk happens to resolve on the
/// host machine. The line order is fixed for cache-stability (GOALS §17g).
fn compose_system_prompt_with(
    role_prompt: &str,
    session_short_id: &str,
    cwd: &Path,
    cfg: &crate::config::extended::ExtendedConfig,
) -> String {
    let os = crate::sysinfo::os_string();
    let mut out = String::with_capacity(role_prompt.len() + 192);
    out.push_str(role_prompt);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str("Harness: cockpit ");
    out.push_str(env!("CARGO_PKG_VERSION"));
    out.push('\n');
    out.push_str("Website: https://flycockpit.dev | App: https://app.flycockpit.dev\n");
    if let Some(name) = cfg.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        out.push_str("User: ");
        out.push_str(name);
        out.push('\n');
    }
    out.push_str("Operating system: ");
    out.push_str(&os);
    out.push('\n');
    if !session_short_id.is_empty() {
        out.push_str("Session: ");
        out.push_str(session_short_id);
        out.push('\n');
    }

    if let Some((found_path, body)) = find_agent_guidance(cwd, &cfg.agent_guidance_files) {
        out.push('\n');
        out.push_str("Project guidance (");
        out.push_str(&found_path.display().to_string());
        out.push_str("):\n");
        out.push_str(&body);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// The full composed system prompt for the user-facing chat agent
/// (`Build`) at `cwd`: role prompt + harness/version/URL
/// lines + (optional) user-name line + OS line + (optional) session
/// line + injected guidance body. Used by the fresh-chat context
/// indicator to size the actual baseline sent to the model, in both
/// daemon (calibrated) and daemonless (raw cl100k) modes. Pass the empty
/// string for `session_short_id` when no session exists yet — it simply
/// omits the `Session:` line, matching what the engine sends.
pub(crate) fn default_chat_system_prompt(cwd: &Path, session_short_id: &str) -> String {
    compose_system_prompt(BUILD_PROMPT, session_short_id, cwd)
}

/// Per-category token sizing of the composed chat system prompt, for the
/// `/context` usage overlay. Splits the single composed block the engine
/// sends into the three buckets that actually make it up, so the overlay
/// can color them distinctly rather than reporting one opaque "system"
/// number. Counts are cl100k_base (`crate::tokens::count`) — the same
/// fallback the chrome's live context indicator uses pre-flight.
///
/// - `base_prompt`: the role/base system prompt (the `Build` agent's
///   `build.md`), the fixed instruction surface.
/// - `system_block`: the appended cached identity lines (harness +
///   version + URLs + optional user name + OS + optional session id),
///   GOALS §17g.
/// - `guidance`: the injected project-guidance / memory file body
///   (`AGENTS.md` / `CLAUDE.md` / …), or 0 when none was found.
///
/// Derived from the exact same assembly as [`compose_system_prompt`]
/// (which appends the system block + guidance to the role prompt), so the
/// three pieces sum to the same prompt the engine actually composes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemPromptBreakdown {
    pub base_prompt: u64,
    pub system_block: u64,
    pub guidance: u64,
}

/// Compute the [`SystemPromptBreakdown`] for the user-facing chat agent
/// (`Build`) at `cwd`. `session_short_id` is empty when no session id is
/// resolved yet (matching what the engine sends on a fresh chat).
pub(crate) fn chat_system_prompt_breakdown(
    cwd: &Path,
    session_short_id: &str,
) -> SystemPromptBreakdown {
    let cfg = load_extended_config(cwd);
    // The full composed prompt, then the same prompt without the role
    // body: the difference is the appended system block + guidance. We
    // recompose with an empty role prompt to isolate the appended part,
    // then split off the guidance body (counted independently) to get the
    // cached identity block on its own. Reusing the real assembler keeps
    // the buckets faithful to what the engine sends.
    let base_prompt = crate::tokens::count(BUILD_PROMPT) as u64;
    let guidance = find_agent_guidance(cwd, &cfg.agent_guidance_files)
        .map(|(_, body)| crate::tokens::count(&body) as u64)
        .unwrap_or(0);
    let full = crate::tokens::count(&compose_system_prompt_with(
        BUILD_PROMPT,
        session_short_id,
        cwd,
        &cfg,
    )) as u64;
    // The composed block = base role prompt + cached identity lines +
    // guidance body (plus a handful of separator newlines folded into the
    // identity block). `system_block` is the remainder after removing the
    // two independently-counted pieces; saturating so a tokenizer rounding
    // quirk can never underflow.
    let system_block = full.saturating_sub(base_prompt).saturating_sub(guidance);
    SystemPromptBreakdown {
        base_prompt,
        system_block,
        guidance,
    }
}

/// Locate the first existing project-guidance file by name, searching
/// `cwd` then its ancestors up to (and including) the git worktree root
/// when there is one — otherwise stop at the filesystem root. Returns
/// the absolute path + file body.
pub(crate) fn load_agent_guidance(cwd: &Path) -> Option<(std::path::PathBuf, String)> {
    let cfg = load_extended_config(cwd);
    find_agent_guidance(cwd, &cfg.agent_guidance_files)
}

/// Load the first parseable layered `extended-config.json` that applies
/// to `cwd` (falling back to defaults when none exists). [`compose_system_prompt`]
/// loads this once and reads both the user name and the guidance-file
/// list from it, so the layered config is never loaded twice per spawn.
fn load_extended_config(cwd: &Path) -> crate::config::extended::ExtendedConfig {
    discover_config_dirs(cwd)
        .into_iter()
        .find_map(|d| ExtendedConfigDoc::load(&d.path.join("extended-config.json")).ok())
        .map(|d| d.config())
        .unwrap_or_default()
}

/// Inner search used by [`load_agent_guidance`]. Walks `cwd` and its
/// ancestors (stopping at the git worktree root) and returns the first
/// existing file whose basename matches an entry in `names`, scanning
/// `names` in order at each directory level. Exposed for tests so they
/// can pin the name list without touching layered config.
fn find_agent_guidance(cwd: &Path, names: &[String]) -> Option<(std::path::PathBuf, String)> {
    if names.is_empty() {
        return None;
    }
    let stop_at = crate::git::find_worktree_root(cwd);
    let mut dir: Option<&Path> = Some(cwd);
    while let Some(d) = dir {
        for name in names {
            let candidate = d.join(name);
            if candidate.is_file()
                && let Ok(body) = std::fs::read_to_string(&candidate)
            {
                return Some((candidate, body));
            }
        }
        if let Some(root) = &stop_at
            && d == root.as_path()
        {
            break;
        }
        dir = d.parent();
    }
    None
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

/// Build an agent by name. Resolution order (overlay model, prompt
/// `user-definable-agents.md`):
///   1. An on-disk override / custom agent ([`crate::agents::resolve`])
///      — the user's edited or new definition wins, and its
///      prompt/tools/model/temperature flow into the constructed agent.
///   2. The embedded factory function for a built-in (no override) —
///      unchanged byte-for-byte so the cached system prefix and exact
///      tool surface are preserved (prompt-cache discipline).
///
/// Returns `Err` for unknown names so the `task` tool can surface
/// "unknown agent" loudly rather than silently spawning the wrong one.
pub fn load(name: &str, args: &SpawnArgs) -> Result<Agent> {
    // The docs pipeline stages are routed by the driver and never reach
    // here through a name; guard them before any disk resolution so a
    // stray `agents/docs.md` can't hijack the pipeline.
    if matches!(name, "docs" | "docs-resolver" | "docs-answerer") {
        bail!(
            "`{name}` is a pipeline stage routed by the driver; load() should be unreachable for it"
        );
    }

    // Overlay: an on-disk override (edited built-in) or a custom agent
    // takes precedence over the embedded factory. A malformed override
    // fails loudly here (naming its source) rather than silently falling
    // back to the embedded default.
    match crate::agents::resolve(&args.cwd, name)? {
        // A genuine on-disk file (override of a built-in, or a custom
        // agent): build generically from the resolved definition so the
        // user's edited tools/model/prompt take effect.
        Some(def) if !def.source.as_os_str().is_empty() => {
            return Ok(agent_from_def(&def, args));
        }
        // An embedded default came back (no override): fall through to the
        // hardcoded factory, which is byte-identical and cache-stable.
        Some(_) => {}
        // Not a built-in and no file on disk: unknown agent.
        None => bail!("unknown agent `{name}`"),
    }

    // Unreachable in practice: `resolve` returned an embedded default, so
    // `name` is a built-in and matches above. Kept exhaustive for safety.
    match name {
        "Build" => Ok(build(args)),
        "coder" => Ok(coder(args)),
        "explore" => Ok(explore(args)),
        other => bail!("unknown built-in agent `{other}`"),
    }
}

/// True if `name` denotes an agent that runs *noninteractively* when
/// delegated to via `task` — the primary dispatches it like a tool call
/// (synchronously) rather than handing the primary conversation off. The
/// driver uses this to route `task(agent=…, …)` correctly.
///
/// `coder` is the sole interactive *handoff* subagent (it takes over the
/// conversation, GOALS §3a/§3b). Everything else delegated via `task` —
/// `explore`, the `docs` pipeline (leaf-terminated, GOALS §3a), and every
/// user-authored custom subagent — runs noninteractively and reports one
/// leaf result up. Defined as the complement of the single interactive
/// agent so custom agents inherit the safe default without a registry.
pub fn is_noninteractive(name: &str) -> bool {
    name != "coder"
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::extended::ExtendedConfig;

    /// Config with a name set, used by the deterministic name-present case.
    fn cfg_with_name(name: &str) -> ExtendedConfig {
        ExtendedConfig {
            name: Some(name.to_string()),
            ..ExtendedConfig::default()
        }
    }

    #[test]
    fn compose_system_prompt_appends_identity_os_and_session() {
        let tmp = tempfile::tempdir().unwrap();
        let out = compose_system_prompt("ROLE PROMPT", "abc123", tmp.path());
        assert!(out.starts_with("ROLE PROMPT"));
        // Harness identity carries the actual build version.
        assert!(out.contains(&format!("Harness: cockpit {}", env!("CARGO_PKG_VERSION"))));
        // Both URLs are present (explicit user decision — keep both).
        assert!(out.contains("https://flycockpit.dev"));
        assert!(out.contains("https://app.flycockpit.dev"));
        assert!(out.contains("Operating system:"));
        assert!(out.contains("Session: abc123"));
    }

    #[test]
    fn compose_system_prompt_omits_session_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let out = compose_system_prompt("ROLE PROMPT", "", tmp.path());
        assert!(out.contains("Operating system:"));
        assert!(!out.contains("Session:"));
    }

    /// Name-present case. Driven through the pure assembler with an
    /// explicit config so the assertion is independent of whichever
    /// layered config the host machine happens to resolve.
    #[test]
    fn compose_system_prompt_includes_user_name_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_name("Ada");
        let out = compose_system_prompt_with("ROLE PROMPT", "abc123", tmp.path(), &cfg);
        assert!(out.contains("User: Ada"), "block was: {out}");
        // Order: the User line sits between the URL line and the OS line.
        let user_at = out.find("User: Ada").unwrap();
        let url_at = out.find("Website: https://flycockpit.dev").unwrap();
        let os_at = out.find("Operating system:").unwrap();
        assert!(url_at < user_at && user_at < os_at, "block was: {out}");
    }

    /// Whitespace-only names are treated as absent (trimmed before the
    /// emptiness check).
    #[test]
    fn compose_system_prompt_omits_user_name_when_blank() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_name("   ");
        let out = compose_system_prompt_with("ROLE PROMPT", "abc123", tmp.path(), &cfg);
        assert!(!out.contains("User:"), "block was: {out}");
    }

    /// Name-absent case. Default config has `name: None`, so the User
    /// line must be omitted entirely.
    #[test]
    fn compose_system_prompt_omits_user_name_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = ExtendedConfig::default();
        let out = compose_system_prompt_with("ROLE PROMPT", "abc123", tmp.path(), &cfg);
        assert!(!out.contains("User:"), "block was: {out}");
    }

    /// Wiring test: the layered loader actually reads `name` out of an
    /// `extended-config.json`. Written into the `.cockpit/` dir of the
    /// test cwd — the project-scoped layer the discovery walk-up finds
    /// ([`load_extended_config`] → [`discover_config_dirs`]).
    #[test]
    fn load_extended_config_reads_name_from_project_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".cockpit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("extended-config.json"),
            r#"{"name":"Christopher"}"#,
        )
        .unwrap();
        // A real home-layer config may take precedence in discovery order
        // on a developer machine; assert the project-dir value is at least
        // reachable by loading that file directly through the same loader.
        let cfg =
            crate::config::extended::ExtendedConfigDoc::load(&dir.join("extended-config.json"))
                .unwrap()
                .config();
        assert_eq!(cfg.name.as_deref(), Some("Christopher"));
        let out = compose_system_prompt_with("ROLE PROMPT", "abc123", tmp.path(), &cfg);
        assert!(out.contains("User: Christopher"), "block was: {out}");
    }

    #[test]
    fn compose_system_prompt_normalizes_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let with_nl = compose_system_prompt("ROLE\n", "abc123", tmp.path());
        let without_nl = compose_system_prompt("ROLE", "abc123", tmp.path());
        // The role-prompt's own newline is preserved either way; the
        // appended lines are identical in both cases.
        assert!(with_nl.contains("\nOperating system:"));
        assert!(without_nl.contains("\nOperating system:"));
    }

    #[test]
    fn compose_system_prompt_injects_first_matching_guidance_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "RULES").unwrap();
        let out = compose_system_prompt("ROLE", "abc", tmp.path());
        assert!(out.contains("Project guidance"));
        assert!(out.contains("RULES"));
    }

    /// Contract test: when multiple configured filenames exist in the
    /// same directory, only the first entry in the user's config list
    /// is loaded. The other files must not contribute.
    #[test]
    fn find_agent_guidance_only_loads_first_match_when_multiple_exist() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "A-CONTENT").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "C-CONTENT").unwrap();

        let names = vec!["AGENTS.md".to_string(), "CLAUDE.md".to_string()];
        let (path, body) = find_agent_guidance(tmp.path(), &names).expect("expected a hit");
        assert!(path.ends_with("AGENTS.md"), "got {path:?}");
        assert_eq!(body, "A-CONTENT");

        // Reverse the order: CLAUDE.md now wins, AGENTS.md is ignored.
        let names_rev = vec!["CLAUDE.md".to_string(), "AGENTS.md".to_string()];
        let (path2, body2) = find_agent_guidance(tmp.path(), &names_rev).expect("expected a hit");
        assert!(path2.ends_with("CLAUDE.md"), "got {path2:?}");
        assert_eq!(body2, "C-CONTENT");
    }

    /// Same shape, but the second-listed file lives in a parent dir.
    /// The first-listed file in the same starting cwd still wins.
    #[test]
    fn find_agent_guidance_first_match_wins_across_ancestors() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "FROM-SUB").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "FROM-ROOT").unwrap();

        // From `sub`, AGENTS.md is right there — CLAUDE.md in the
        // parent must not be loaded.
        let names = vec!["AGENTS.md".to_string(), "CLAUDE.md".to_string()];
        let (path, body) = find_agent_guidance(&sub, &names).expect("expected a hit");
        assert!(path.ends_with("sub/AGENTS.md"), "got {path:?}");
        assert_eq!(body, "FROM-SUB");
    }
}

/// Build an [`Agent`] from a resolved [`crate::agents::AgentDef`] — the
/// path taken for an on-disk override (edited built-in) or a custom
/// agent. The def's `prompt`, `tools`, `temperature`, and (when
/// resolvable) `model` flow into the constructed agent so an edit takes
/// effect on the next run. Invariants were already enforced at load
/// ([`crate::agents::validate_invariants`]); this builds the toolbox from
/// the validated grant.
///
/// When `tools` is absent the agent falls back to its role-default
/// surface: for a built-in name we reuse that built-in's embedded
/// default grant (so an override that only tweaks the prompt keeps the
/// right tools); a custom agent with no grant gets the read-only
/// investigator surface.
fn agent_from_def(def: &crate::agents::AgentDef, args: &SpawnArgs) -> Agent {
    // Resolve the tool-name grant: explicit list, else the role default.
    let grant: Vec<String> = match &def.tools {
        Some(t) => t.clone(),
        None => crate::agents::embedded_default(&def.name)
            .and_then(|d| d.tools)
            .unwrap_or_else(default_custom_tools),
    };

    let mut tb = ToolBox::new();
    for name in &grant {
        tb = add_tool_by_name(tb, name, def, args);
    }
    // Custom-bash tools (webfetch/websearch/…) are config-driven, not part
    // of the named grant — attach them like the built-in factories do.
    tb = with_custom_tools(tb, &args.cwd);
    // Cross-session recall tools, gated on interactive spawn.
    tb = with_recall_tools(tb, args);

    // Per-agent model override: a `provider:model-id` selector resolved
    // through the same provider pipeline the foreground model uses. On any
    // failure (unconfigured provider, malformed selector) fall back to the
    // session model rather than failing the spawn — the override is a
    // preference, not a hard requirement.
    let model = resolve_agent_model(def, args);

    let mut params = args.params.clone();
    if let Some(temp) = def.temperature {
        params.temperature = Some(temp as f64);
    }

    Agent {
        name: def.name.clone(),
        system: compose_system_prompt(def.resolved_prompt(), &args.session_short_id, &args.cwd),
        tools: tb,
        model,
        params,
    }
}

/// Default tool grant for a custom agent that names no `tools:` — the
/// read-only investigator surface (`explore`'s grant). Conservative:
/// never includes write/lock or structural-delegation tools.
fn default_custom_tools() -> Vec<String> {
    [
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
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Append the tool named `name` to `tb`. Structural tools (`task`) are
/// wired with the def's reachable subagents. Unknown names are skipped
/// silently here because they were already rejected at load time by
/// [`crate::agents::validate_invariants`]; the custom-bash tools are
/// attached separately, so a name not handled here is a no-op.
fn add_tool_by_name(
    tb: ToolBox,
    name: &str,
    def: &crate::agents::AgentDef,
    args: &SpawnArgs,
) -> ToolBox {
    use crate::tools;
    match name {
        "read" => tb.with(Arc::new(tools::read::ReadTool)),
        "bash" => tb.with(Arc::new(tools::bash::BashTool::new())),
        "readlock" => tb.with(Arc::new(tools::readlock::ReadlockTool)),
        "writeunlock" => tb.with(Arc::new(tools::writeunlock::WriteunlockTool)),
        "editunlock" => tb.with(Arc::new(tools::editunlock::EditunlockTool)),
        "unlock" => tb.with(Arc::new(tools::unlock::UnlockTool)),
        "tree" => tb.with(Arc::new(tools::intel::TreeTool)),
        "outline" => tb.with(Arc::new(tools::intel::OutlineTool)),
        "symbol_find" => tb.with(Arc::new(tools::intel::SymbolFindTool)),
        "word" => tb.with(Arc::new(tools::intel::WordTool)),
        "deps" => tb.with(Arc::new(tools::intel::DepsTool)),
        "hot" => tb.with(Arc::new(tools::intel::HotTool)),
        "circular" => tb.with(Arc::new(tools::intel::CircularTool)),
        "search" => tb.with(Arc::new(tools::intel::SearchTool)),
        "skill" => tb.with(Arc::new(tools::skill::SkillTool)),
        "question" => tb.with(Arc::new(tools::question::QuestionTool)),
        "jobs" => tb.with(Arc::new(tools::jobs::JobsTool)),
        "task" => {
            let subs = reachable_subagents(def, &args.cwd);
            let sub_refs: Vec<&str> = subs.iter().map(String::as_str).collect();
            tb.with(Arc::new(tools::task::TaskTool::with_subagents(&sub_refs)))
        }
        // `session_search`/`session_read` are added by `with_recall_tools`
        // (interactive-gated); naming them in the grant is a no-op so they
        // aren't double-registered. `grep`/`glob` are sandbox-only and were
        // rejected at load. Anything else is a custom-bash tool, attached
        // by `with_custom_tools`.
        _ => tb,
    }
}

/// The subagents a `task`-granting agent may delegate to: the bundled
/// reachable set (`coder`/`explore`/`docs`, what `Build` ships with) plus
/// any user-authored custom agent whose `mode` makes it reachable as a
/// subagent (`subagent`/`all`). Each is listed once, minus the caller
/// itself to avoid a self-delegation loop. Honors the `mode` field for
/// reachability per `prompts/user-definable-agents.md`.
fn reachable_subagents(def: &crate::agents::AgentDef, cwd: &Path) -> Vec<String> {
    let mut out = build_subagents(cwd);
    out.retain(|s| *s != def.name);
    out
}

/// The bundled reachable subagent set (`coder`/`explore`/`docs`) plus any
/// user-authored custom agent whose `mode` makes it reachable as a
/// subagent (`subagent`/`all`). Shared by the bundled `Build` factory and
/// the generic [`reachable_subagents`] so both honor the `mode` field for
/// reachability (`prompts/user-definable-agents.md`). Each name appears
/// once; the bundled set leads so the cached prefix stays stable when no
/// custom agents are present.
fn build_subagents(cwd: &Path) -> Vec<String> {
    let mut out: Vec<String> = vec![
        "coder".to_string(),
        "explore".to_string(),
        "docs".to_string(),
    ];
    for listing in crate::agents::list_all(cwd) {
        if !matches!(listing.kind, crate::agents::AgentKind::Custom) {
            continue;
        }
        if let Ok(custom) = &listing.def
            && custom.mode.is_subagent()
            && !out.contains(&listing.name)
        {
            out.push(listing.name);
        }
    }
    out
}

/// Resolve a per-agent `model` override to a concrete [`Model`]. Falls
/// back to the session model on any failure so an override pointing at an
/// unconfigured provider degrades gracefully rather than breaking the
/// spawn.
fn resolve_agent_model(def: &crate::agents::AgentDef, args: &SpawnArgs) -> Arc<Model> {
    let Some(selector) = def
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return args.model.clone();
    };
    let Some((provider, model_id)) = selector.split_once(':') else {
        return args.model.clone();
    };
    let cfg = discover_config_dirs(&args.cwd)
        .into_iter()
        .find_map(|d| crate::config::providers::ConfigDoc::load(&d.path.join("config.json")).ok())
        .map(|d| d.providers())
        .unwrap_or_default();
    match Model::for_provider(&cfg, provider, model_id) {
        Ok(m) => Arc::new(m),
        Err(_) => args.model.clone(),
    }
}

/// `Build` — the user-facing primary agent. Owns the chat
/// when the focus is *making the change* (GOALS §3a). Delegates writes
/// to `coder` via `task`.
pub fn build(args: &SpawnArgs) -> Agent {
    // Reachable subagents: the bundled set plus any custom subagent the
    // user has added (`prompts/user-definable-agents.md` discoverability).
    let subs = build_subagents(&args.cwd);
    let sub_refs: Vec<&str> = subs.iter().map(String::as_str).collect();
    let tools = with_recall_tools(
        with_custom_tools(
            ToolBox::new()
                .with(Arc::new(crate::tools::read::ReadTool))
                .with(Arc::new(crate::tools::bash::BashTool::new()))
                .with(Arc::new(crate::tools::intel::TreeTool))
                .with(Arc::new(crate::tools::intel::HotTool))
                // The `jobs` meta-tool (GOALS §22) — fixed minimal schema, so
                // the tools array stays byte-stable as branches are enabled.
                // Structural: intercepted by the engine and routed to the
                // driver-owned async-job authority.
                .with(Arc::new(crate::tools::jobs::JobsTool))
                // `question` (GOALS §3b): structural — blocks the turn until
                // the user answers. Only `Build` + `coder` get
                // it; `explore`/`docs` are leaf-terminated and report up.
                .with(Arc::new(crate::tools::question::QuestionTool))
                // `skill` (GOALS §5): manual on-demand skill loading. Both
                // interactive primaries get it; leaf agents don't.
                .with(Arc::new(crate::tools::skill::SkillTool))
                .with(Arc::new(crate::tools::task::TaskTool::with_subagents(
                    &sub_refs,
                ))),
            &args.cwd,
        ),
        args,
    );

    Agent {
        name: "Build".to_string(),
        system: compose_system_prompt(BUILD_PROMPT, &args.session_short_id, &args.cwd),
        tools,
        model: args.model.clone(),
        params: args.params.clone(),
    }
}

/// `coder` — the only agent that writes. Holds file locks; runs bash;
/// applies edits. Caller-determined interactivity: interactive when
/// spawned from `Build` (GOALS §3a/§3b).
pub fn coder(args: &SpawnArgs) -> Agent {
    let tools = with_recall_tools(
        ToolBox::new()
            .with(Arc::new(crate::tools::read::ReadTool))
            .with(Arc::new(crate::tools::readlock::ReadlockTool))
            .with(Arc::new(crate::tools::writeunlock::WriteunlockTool))
            .with(Arc::new(crate::tools::unlock::UnlockTool))
            .with(Arc::new(crate::tools::editunlock::EditunlockTool))
            .with(Arc::new(crate::tools::bash::BashTool::new()))
            .with(Arc::new(crate::tools::intel::OutlineTool))
            .with(Arc::new(crate::tools::intel::SymbolFindTool))
            .with(Arc::new(crate::tools::intel::DepsTool))
            .with(Arc::new(crate::tools::intel::CircularTool))
            .with(Arc::new(crate::tools::intel::WordTool))
            .with(Arc::new(crate::tools::intel::SearchTool))
            // `question` (GOALS §3b): blocks the turn until the user answers.
            .with(Arc::new(crate::tools::question::QuestionTool))
            // `skill` (GOALS §5): manual on-demand skill loading.
            .with(Arc::new(crate::tools::skill::SkillTool))
            // `coder` delegates dependency-usage questions to the `docs`
            // pipeline (GOALS §3a: coder → docs). Noninteractive; the docs
            // unit returns one leaf report.
            .with(Arc::new(crate::tools::task::TaskTool::with_subagents(&[
                "docs",
            ]))),
        args,
    );

    Agent {
        name: "coder".to_string(),
        system: compose_system_prompt(CODER_PROMPT, &args.session_short_id, &args.cwd),
        tools,
        model: args.model.clone(),
        params: args.params.clone(),
    }
}

/// `explore` — read-only investigator. Leaf in the invocation tree
/// (no `task` of its own). Runs noninteractively from
/// `Build`'s perspective: the primary agent dispatches it
/// via `task(agent="explore", …)` and gets a single text report back
/// as the tool result. The user sees the call rendered like any other
/// tool in the primary agent's history.
pub fn explore(args: &SpawnArgs) -> Agent {
    let tools = with_recall_tools(
        with_custom_tools(
            ToolBox::new()
                .with(Arc::new(crate::tools::read::ReadTool))
                .with(Arc::new(crate::tools::bash::BashTool::new()))
                .with(Arc::new(crate::tools::intel::TreeTool))
                .with(Arc::new(crate::tools::intel::OutlineTool))
                .with(Arc::new(crate::tools::intel::SymbolFindTool))
                .with(Arc::new(crate::tools::intel::WordTool))
                .with(Arc::new(crate::tools::intel::DepsTool))
                .with(Arc::new(crate::tools::intel::HotTool))
                .with(Arc::new(crate::tools::intel::CircularTool))
                .with(Arc::new(crate::tools::intel::SearchTool)),
            &args.cwd,
        ),
        args,
    );

    Agent {
        name: "explore".to_string(),
        system: compose_system_prompt(EXPLORE_PROMPT, &args.session_short_id, &args.cwd),
        tools,
        model: args.model.clone(),
        params: args.params.clone(),
    }
}

/// Docs.1 — the resolver stage of the `docs` pipeline. Runs in the
/// caller's cwd (same trust level as `explore`/`coder`), gated to the
/// registry tools plus `bash`/`webfetch`/`websearch` for registry
/// lookups. Receives **only** the package name (the question never
/// enters its context — token economy, GOALS §10). `resolution` is the
/// shared slot the pipeline reads to learn which package dir to launch
/// Docs.2 in; `target` is the package the caller asked about.
pub fn docs_resolver(
    args: &SpawnArgs,
    resolution: std::sync::Arc<crate::tools::docs::DocsResolution>,
    target: String,
) -> Agent {
    let tools = with_custom_tools(
        ToolBox::new()
            .with(Arc::new(crate::tools::docs::ListPackagesTool::new(
                resolution.clone(),
                target,
            )))
            .with(Arc::new(crate::tools::docs::AddPackageTool::new(
                resolution,
            )))
            .with(Arc::new(crate::tools::bash::BashTool::new())),
        &args.cwd,
    );

    Agent {
        name: "docs-resolver".to_string(),
        system: compose_system_prompt(DOCS_RESOLVER_PROMPT, &args.session_short_id, &args.cwd),
        tools,
        model: args.model.clone(),
        params: args.params.clone(),
    }
}

/// Docs.2 — the answerer stage of the `docs` pipeline. Runs in the
/// resolved package directory (`args.cwd` is the package root). Tools:
/// `read` + the sandboxed `grep`/`glob` only — **no bash, no network, no
/// write** (prompt `docs-agent.md` decision 2/3). The sandbox confines
/// every path to `args.cwd`, which is why bash can be denied: Docs.2 runs
/// inside untrusted third-party source.
pub fn docs_answerer(args: &SpawnArgs) -> Agent {
    let tools = ToolBox::new()
        .with(Arc::new(crate::tools::read::ReadTool))
        .with(Arc::new(crate::tools::grep::GrepTool))
        .with(Arc::new(crate::tools::glob::GlobTool));

    Agent {
        name: "docs-answerer".to_string(),
        system: compose_system_prompt(DOCS_ANSWERER_PROMPT, &args.session_short_id, &args.cwd),
        tools,
        model: args.model.clone(),
        params: args.params.clone(),
    }
}
