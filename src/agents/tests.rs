//! Tests for agent definition parsing, override resolution, invariant
//! validation, eject/reset, and name→path resolution.

use std::fs;
use std::path::Path;

use super::invariants::{LOCK_WRITE_TOOLS, SANDBOX_ONLY_TOOLS};
use super::*;

/// A `.cockpit/` config dir under `cwd`, so the discovery walk-up finds a
/// project-scoped layer. Returns the `agents/` subdir.
fn project_agents_dir(cwd: &Path) -> std::path::PathBuf {
    let dir = cwd.join(".cockpit").join("agents");
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ── Parsing ──────────────────────────────────────────────────────────────

#[test]
fn parse_agent_reads_frontmatter_and_body() {
    let text = "---\n\
description: A custom reviewer.\n\
mode: subagent\n\
model: anthropic:claude-opus-4-7\n\
temperature: 0.3\n\
tools: [read, bash, search]\n\
---\n\
\n\
You are a reviewer. Be terse.\n";
    let def = parse_agent(text, "my-reviewer", "x.md".into()).unwrap();
    assert_eq!(def.name, "my-reviewer");
    assert_eq!(def.description, "A custom reviewer.");
    assert_eq!(def.mode, AgentMode::Subagent);
    assert_eq!(def.model.as_deref(), Some("anthropic:claude-opus-4-7"));
    assert_eq!(def.temperature, Some(0.3));
    assert_eq!(
        def.tools,
        Some(vec!["read".into(), "bash".into(), "search".into()])
    );
    assert_eq!(def.resolved_prompt(), "You are a reviewer. Be terse.");
}

#[test]
fn parse_agent_defaults_mode_to_all() {
    let text = "---\ndescription: x\n---\nbody\n";
    let def = parse_agent(text, "a", "a.md".into()).unwrap();
    assert_eq!(def.mode, AgentMode::All);
    assert!(def.tools.is_none());
}

#[test]
fn parse_agent_missing_description_fails_with_source() {
    let text = "---\nmode: subagent\n---\nbody\n";
    let err = parse_agent(text, "bad", "/p/bad.md".into()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("bad"), "{msg}");
    assert!(msg.contains("/p/bad.md"), "names the source path: {msg}");
}

#[test]
fn parse_agent_bad_yaml_fails_with_source() {
    let text = "---\ndescription: [unterminated\n---\nbody\n";
    let err = parse_agent(text, "bad", "/p/bad.md".into()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("/p/bad.md"), "names the source: {msg}");
    assert!(msg.contains("invalid frontmatter"), "{msg}");
}

#[test]
fn parse_agent_no_frontmatter_fails() {
    let text = "just a body, no fence\n";
    let err = parse_agent(text, "x", "x.md".into()).unwrap_err();
    assert!(format!("{err}").contains("no YAML frontmatter"));
}

// ── Round-trip / eject faithfulness ──────────────────────────────────────

#[test]
fn to_markdown_round_trips_through_parse() {
    let def = embedded_default("coder").unwrap();
    let md = def.to_markdown().unwrap();
    // Re-parse the ejected form.
    let parsed = parse_agent(&md, "coder", "coder.md".into()).unwrap();
    assert_eq!(parsed.description, def.description);
    assert_eq!(parsed.mode, def.mode);
    assert_eq!(parsed.tools, def.tools);
    assert_eq!(parsed.resolved_prompt(), def.resolved_prompt());
}

// ── Invariant validation ─────────────────────────────────────────────────

fn def_with_tools(name: &str, tools: &[&str]) -> AgentDef {
    AgentDef {
        name: name.into(),
        description: "d".into(),
        mode: AgentMode::Subagent,
        model: None,
        temperature: None,
        tools: Some(tools.iter().map(|s| s.to_string()).collect()),
        permission: None,
        prompt: "body".into(),
        source: "x.md".into(),
    }
}

#[test]
fn non_coder_with_write_tool_is_rejected() {
    let def = def_with_tools("explore", &["read", "writeunlock"]);
    let err = validate_invariants(&def).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("`explore`"), "{msg}");
    assert!(msg.contains("`writeunlock`"), "{msg}");
    assert!(msg.contains("single-writer"), "{msg}");
}

#[test]
fn coder_with_write_tools_is_allowed() {
    let def = def_with_tools("coder", LOCK_WRITE_TOOLS);
    assert!(validate_invariants(&def).is_ok());
}

#[test]
fn user_agent_with_sandbox_tool_is_rejected() {
    for t in SANDBOX_ONLY_TOOLS {
        let def = def_with_tools("my-agent", &["read", t]);
        let err = validate_invariants(&def).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(&format!("`{t}`")), "{msg}");
        assert!(msg.contains("docs-answerer-only"), "{msg}");
    }
}

#[test]
fn even_coder_cannot_get_sandbox_tools() {
    // The sandbox check fires before the writer check, so naming `grep`
    // as `coder` still rejects.
    let def = def_with_tools("coder", &["grep"]);
    let err = validate_invariants(&def).unwrap_err();
    assert!(format!("{err}").contains("docs-answerer-only"));
}

#[test]
fn plan_and_plan_author_embedded_defs_validate() {
    // The bundled `Plan` + `plan-author` defs are admissible: their grants
    // hold no write/lock or sandbox tools (`plan.md §4.6.d`).
    for name in ["Plan", "plan-author"] {
        let def = embedded_default(name).unwrap();
        assert!(
            validate_invariants(&def).is_ok(),
            "embedded `{name}` def must validate"
        );
    }
}

#[test]
fn planning_tools_are_grantable() {
    // The planning + deferral tools are known names any agent may grant
    // (none are write/lock).
    let def = def_with_tools(
        "my-planner",
        &[
            "plan_create",
            "add_step",
            "add_step_dependency",
            "plan_set_branches",
            "plan_list",
            "defer_to_orchestrator",
        ],
    );
    assert!(validate_invariants(&def).is_ok());
}

#[test]
fn plan_author_def_holds_no_write_or_lock_tools() {
    // Defense-in-depth: the plan-author's grant intersects neither the
    // write/lock set nor sandbox tools.
    let def = embedded_default("plan-author").unwrap();
    let tools = def.tools.clone().unwrap();
    for t in LOCK_WRITE_TOOLS.iter().chain(SANDBOX_ONLY_TOOLS) {
        assert!(
            !tools.contains(&t.to_string()),
            "plan-author must not grant `{t}`"
        );
    }
}

#[test]
fn unknown_tool_name_is_rejected_backticked() {
    let def = def_with_tools("my-agent", &["read", "frobnicate"]);
    let err = validate_invariants(&def).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("unknown tool `frobnicate`"), "{msg}");
}

#[test]
fn absent_tools_grant_validates() {
    let mut def = def_with_tools("my-agent", &[]);
    def.tools = None;
    assert!(validate_invariants(&def).is_ok());
}

// ── Override resolution ──────────────────────────────────────────────────

#[test]
fn resolve_returns_embedded_default_when_no_override() {
    let tmp = tempfile::tempdir().unwrap();
    let def = resolve(tmp.path(), "coder").unwrap().unwrap();
    // Embedded default has an empty source.
    assert!(def.source.as_os_str().is_empty());
    assert_eq!(def.name, "coder");
}

#[test]
fn resolve_prefers_on_disk_override() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project_agents_dir(tmp.path());
    fs::write(
        dir.join("coder.md"),
        "---\ndescription: edited coder\nmode: subagent\ntools: [read]\n---\nNEW BODY\n",
    )
    .unwrap();
    let def = resolve(tmp.path(), "coder").unwrap().unwrap();
    assert!(!def.source.as_os_str().is_empty(), "override has a source");
    assert_eq!(def.description, "edited coder");
    assert_eq!(def.resolved_prompt(), "NEW BODY");
    assert_eq!(def.tools, Some(vec!["read".to_string()]));
}

#[test]
fn custom_name_colliding_with_builtin_is_treated_as_override() {
    // A file named `explore.md` overrides the built-in `explore` rather
    // than appearing as a separate custom agent.
    let tmp = tempfile::tempdir().unwrap();
    let dir = project_agents_dir(tmp.path());
    fs::write(
        dir.join("explore.md"),
        "---\ndescription: my explore\n---\nbody\n",
    )
    .unwrap();
    let listings = list_all(tmp.path());
    let explore_rows: Vec<_> = listings.iter().filter(|l| l.name == "explore").collect();
    assert_eq!(explore_rows.len(), 1, "explore appears exactly once");
    assert!(
        matches!(
            explore_rows[0].kind,
            AgentKind::Builtin { overridden: true }
        ),
        "the collision is an override, not a second custom agent"
    );
}

#[test]
fn resolve_returns_none_for_unknown_name() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(resolve(tmp.path(), "no-such-agent").unwrap().is_none());
}

#[test]
fn resolve_malformed_override_fails_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project_agents_dir(tmp.path());
    let path = dir.join("coder.md");
    fs::write(&path, "---\nmode: subagent\n---\nno description\n").unwrap();
    let err = resolve(tmp.path(), "coder").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("coder.md"), "names the source: {msg}");
    // Did NOT silently fall back to the embedded default.
}

#[test]
fn resolve_rejects_override_with_invariant_violation() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project_agents_dir(tmp.path());
    fs::write(
        dir.join("explore.md"),
        "---\ndescription: e\ntools: [read, editunlock]\n---\nbody\n",
    )
    .unwrap();
    let err = resolve(tmp.path(), "explore").unwrap_err();
    assert!(format!("{err}").contains("single-writer"));
}

// ── list_all ─────────────────────────────────────────────────────────────

#[test]
fn list_all_lists_builtins_and_custom() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project_agents_dir(tmp.path());
    fs::write(
        dir.join("my-reviewer.md"),
        "---\ndescription: reviewer\nmode: subagent\n---\nbody\n",
    )
    .unwrap();
    let listings = list_all(tmp.path());
    for name in BUILTIN_AGENT_NAMES {
        assert!(
            listings.iter().any(|l| &l.name == name),
            "built-in {name} listed"
        );
    }
    let custom = listings.iter().find(|l| l.name == "my-reviewer").unwrap();
    assert_eq!(custom.kind, AgentKind::Custom);
    assert!(custom.def.is_ok());
}

// ── Eject ────────────────────────────────────────────────────────────────

#[test]
fn eject_writes_faithful_file() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join(".cockpit");
    fs::create_dir_all(&config_dir).unwrap();
    let (path, written) = eject_builtin(tmp.path(), &config_dir, "coder").unwrap();
    assert!(written, "first eject writes a new file");
    assert!(path.exists());
    let on_disk = fs::read_to_string(&path).unwrap();
    let parsed = parse_agent(&on_disk, "coder", path.clone()).unwrap();
    let embedded = embedded_default("coder").unwrap();
    assert_eq!(parsed.description, embedded.description);
    assert_eq!(parsed.tools, embedded.tools);
    assert_eq!(parsed.resolved_prompt(), embedded.resolved_prompt());
    // And the ejected file is now the resolved override.
    let resolved = resolve(tmp.path(), "coder").unwrap().unwrap();
    assert!(!resolved.source.as_os_str().is_empty());
}

#[test]
fn eject_does_not_clobber_existing_override() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join(".cockpit");
    let dir = project_agents_dir(tmp.path());
    let existing = dir.join("coder.md");
    fs::write(
        &existing,
        "---\ndescription: mine\ntools: [read]\n---\nMY EDITS\n",
    )
    .unwrap();
    let (path, written) = eject_builtin(tmp.path(), &config_dir, "coder").unwrap();
    assert!(!written, "must not clobber");
    assert_eq!(path, existing);
    // The user's content is intact.
    assert!(fs::read_to_string(&existing).unwrap().contains("MY EDITS"));
}

#[test]
fn eject_rejects_non_builtin() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join(".cockpit");
    assert!(eject_builtin(tmp.path(), &config_dir, "my-custom").is_err());
}

// ── Reset ────────────────────────────────────────────────────────────────

#[test]
fn reset_all_removes_builtin_overrides_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project_agents_dir(tmp.path());
    // Two built-in overrides + one custom agent.
    fs::write(
        dir.join("coder.md"),
        "---\ndescription: c\ntools: [read]\n---\nb\n",
    )
    .unwrap();
    fs::write(dir.join("explore.md"), "---\ndescription: e\n---\nb\n").unwrap();
    fs::write(dir.join("my-reviewer.md"), "---\ndescription: r\n---\nb\n").unwrap();

    let removed = reset_all_builtins(tmp.path()).unwrap();
    assert_eq!(removed.len(), 2, "only the two built-in overrides removed");
    assert!(!dir.join("coder.md").exists());
    assert!(!dir.join("explore.md").exists());
    assert!(
        dir.join("my-reviewer.md").exists(),
        "custom agent is untouched by reset"
    );
    // Built-ins now resolve from embedded again.
    assert!(
        resolve(tmp.path(), "coder")
            .unwrap()
            .unwrap()
            .source
            .as_os_str()
            .is_empty()
    );
}

#[test]
fn reset_with_no_overrides_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    project_agents_dir(tmp.path());
    let removed = reset_all_builtins(tmp.path()).unwrap();
    assert!(removed.is_empty());
}

// ── name→path resolution (flat-file form; dir-form readiness) ────────────

#[test]
fn agent_path_in_uses_flat_form_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let p = agent_path_in(tmp.path(), "coder");
    assert!(p.ends_with("coder.md"), "flat-file form: {p:?}");
}

#[test]
fn agent_path_in_prefers_existing_flat_file() {
    let tmp = tempfile::tempdir().unwrap();
    let flat = tmp.path().join("coder.md");
    fs::write(&flat, "x").unwrap();
    assert_eq!(agent_path_in(tmp.path(), "coder"), flat);
}

#[test]
fn agent_path_in_surfaces_dir_form_when_present() {
    // Forward-compat: a `<name>/` directory (the future per-mode layout)
    // is surfaced rather than assuming `<name>.md`.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("coder");
    fs::create_dir_all(&dir).unwrap();
    let resolved = agent_path_in(tmp.path(), "coder");
    assert_eq!(resolved, dir, "dir form is surfaced: {resolved:?}");
    assert!(resolved.is_dir());
}

#[test]
fn dir_form_override_fails_until_supported() {
    // The resolver surfaces the dir form but the loader rejects it (only
    // the flat form ships) — and does so without a panic, naming the path.
    let tmp = tempfile::tempdir().unwrap();
    let agents = project_agents_dir(tmp.path());
    fs::create_dir_all(agents.join("coder")).unwrap();
    let err = resolve(tmp.path(), "coder").unwrap_err();
    assert!(format!("{err}").contains("directory form"));
}
