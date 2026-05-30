//! Agent definition discovery, parsing, resolution, and invariant
//! validation.
//!
//! On-disk format: YAML frontmatter + Markdown body. The frontmatter shape
//! is inspired by opencode's agent files (we own the file layout but
//! the field names track theirs where the design is good — see
//! `opencode-features-review.md` §4 for the schema).
//!
//! ```text
//! ---
//! description: One-line description.
//! mode: subagent
//! model: anthropic:claude-opus-4-7
//! temperature: 0.2
//! tools: [read, bash, search]
//! ---
//! <markdown body == the agent's system prompt>
//! ```
//!
//! Disk model (`prompts/user-definable-agents.md`): the bundled cast
//! (`Build`, `coder`, `explore`) stays **embedded** in the binary as
//! fallback [`AgentDef`]s. Nothing is written on first run. "Editing" a
//! built-in *ejects* its default to `.cockpit/agents/<name>.md`; from then
//! on the on-disk file overrides the embedded default **by name**.
//! "Reset" deletes the override. Custom agents (any non-built-in name)
//! live only on disk and are never touched by reset.
//!
//! The docs two-stage pipeline (Docs.1 / Docs.2) is **not** an [`AgentDef`]
//! — it stays entirely hardcoded in [`crate::engine::builtin`] and
//! [`crate::engine::docs_pipeline`] and is never exposed here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

mod builtin_defs;
pub(crate) mod invariants;

pub use builtin_defs::{BUILTIN_AGENT_NAMES, embedded_default, is_builtin_agent};
pub use invariants::validate_invariants;

/// A fully-resolved agent definition: the embedded default for a
/// built-in, or a user-authored file on disk. The `model`/`temperature`/
/// `tools` here are what the engine builds the agent from — an edited
/// override therefore takes effect on the next agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    /// The agent's name. Not part of the frontmatter — it is the file
    /// stem (`<name>.md` or the `<name>/` directory). Carried here for
    /// dispatch and override-by-name resolution.
    #[serde(skip)]
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub mode: AgentMode,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub permission: Option<serde_json::Value>,
    /// Body of the markdown file (the agent's system prompt). Resolved
    /// through [`AgentDef::resolved_prompt`] rather than read directly so
    /// a future per-`llm_mode` body variant can thread through one path
    /// (forward-compat, `prompts/user-definable-agents.md`).
    #[serde(skip)]
    pub prompt: String,
    /// Path the definition was loaded from (`<dir>/<name>.md`), or empty
    /// for an embedded default. Used for diagnostics and override
    /// detection.
    #[serde(skip)]
    pub source: PathBuf,
}

/// Reachability of an agent in the delegation tree. **Not** the
/// defensive/normal LLM-mode axis (that future feature owns a separate
/// key — see `prompts/user-definable-agents.md` forward-compat notes);
/// do not overload this.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Reachable both as a primary (chat-owning) agent and as a `task`
    /// subagent.
    #[default]
    All,
    /// Reachable only as a primary chat-owning agent.
    Primary,
    /// Reachable only as a `task` subagent.
    Subagent,
}

impl AgentMode {
    /// Whether this agent may be delegated to via `task` (i.e. it is a
    /// reachable subagent). The `Primary`/`All` distinction for chat
    /// ownership is consumed by the future LLM-modes work; only subagent
    /// reachability is load-bearing today.
    pub fn is_subagent(self) -> bool {
        matches!(self, AgentMode::All | AgentMode::Subagent)
    }
}

impl AgentDef {
    /// The agent's effective system prompt. Funneled through this
    /// accessor (rather than reading `self.prompt` at call sites) so a
    /// future per-`llm_mode` body variant can be selected here without
    /// touching every consumer (forward-compat).
    pub fn resolved_prompt(&self) -> &str {
        &self.prompt
    }

    /// Serialize back to the on-disk `<name>.md` form: YAML frontmatter
    /// fence + the markdown body. Used by eject so a built-in's default
    /// materializes as a faithful, re-editable file.
    pub fn to_markdown(&self) -> Result<String> {
        // Build an ordered frontmatter map so the emitted file is stable
        // and human-friendly (description, mode, model, temperature,
        // tools, permission — only the fields that carry a value).
        let mut fm = serde_yaml::Mapping::new();
        fm.insert("description".into(), self.description.clone().into());
        fm.insert(
            "mode".into(),
            serde_yaml::to_value(self.mode)?
                .as_str()
                .unwrap_or("all")
                .into(),
        );
        if let Some(model) = &self.model {
            fm.insert("model".into(), model.clone().into());
        }
        if let Some(temp) = self.temperature {
            fm.insert("temperature".into(), (temp as f64).into());
        }
        if let Some(tools) = &self.tools {
            let seq: Vec<serde_yaml::Value> = tools.iter().map(|t| t.clone().into()).collect();
            fm.insert("tools".into(), serde_yaml::Value::Sequence(seq));
        }
        if let Some(perm) = &self.permission {
            fm.insert("permission".into(), serde_yaml::to_value(perm)?);
        }
        let yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(fm))?;
        let body = self.prompt.trim_end_matches('\n');
        Ok(format!("---\n{yaml}---\n\n{body}\n"))
    }
}

/// Split a `<frontmatter>\n---\n<body>` markdown document into the raw
/// YAML frontmatter and the body. A document with no leading `---` fence
/// has an empty frontmatter and the whole text as body. The opening
/// fence must be the very first line.
fn split_frontmatter(text: &str) -> (&str, &str) {
    let rest = match text.strip_prefix("---\n") {
        Some(r) => r,
        // Tolerate a leading BOM / CRLF opening fence.
        None => match text.strip_prefix("---\r\n") {
            Some(r) => r,
            None => return ("", text),
        },
    };
    // Scan for the closing fence: a line that is exactly `---`.
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            let fm = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (fm, body);
        }
        offset += line.len();
    }
    // No closing fence — treat the whole remainder as frontmatter-less.
    ("", text)
}

/// Parse YAML frontmatter + markdown body into an [`AgentDef`]. `name`
/// is the resolved agent name (the file stem); `source` is the path the
/// text came from (used in diagnostics). A missing `description` or bad
/// YAML fails with the `source` path named so the user's mistake isn't
/// hidden.
pub fn parse_agent(text: &str, name: &str, source: PathBuf) -> Result<AgentDef> {
    let (fm_raw, body) = split_frontmatter(text);

    #[derive(Deserialize)]
    struct Frontmatter {
        description: String,
        #[serde(default)]
        mode: AgentMode,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        temperature: Option<f32>,
        #[serde(default)]
        tools: Option<Vec<String>>,
        #[serde(default)]
        permission: Option<serde_json::Value>,
    }

    if fm_raw.trim().is_empty() {
        bail!(
            "agent `{name}` ({}) has no YAML frontmatter — a `description` field is required",
            source.display()
        );
    }
    let fm: Frontmatter = serde_yaml::from_str(fm_raw).map_err(|e| {
        anyhow::anyhow!(
            "agent `{name}` ({}) has invalid frontmatter: {e}",
            source.display()
        )
    })?;
    if fm.description.trim().is_empty() {
        bail!(
            "agent `{name}` ({}) is missing a non-empty `description`",
            source.display()
        );
    }

    Ok(AgentDef {
        name: name.to_string(),
        description: fm.description,
        mode: fm.mode,
        model: fm.model,
        temperature: fm.temperature,
        tools: fm.tools,
        permission: fm.permission,
        // Trim the blank line(s) the frontmatter fence leaves before the
        // body and any trailing newline, so the stored prompt matches the
        // embedded-default form (the composer re-adds a single newline).
        prompt: body.trim_start_matches('\n').trim_end().to_string(),
        source,
    })
}

/// Load a single agent file from an arbitrary path. The file does not
/// need to live in any particular directory. Used by `cockpit run
/// --agent-file …`. The agent name is the file stem.
pub fn load_from_file(path: &Path) -> Result<AgentDef> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading agent file {}: {e}", path.display()))?;
    let name = agent_name_from_path(path)
        .ok_or_else(|| anyhow::anyhow!("agent file {} has no usable file stem", path.display()))?;
    let def = parse_agent(&text, &name, path.to_path_buf())?;
    validate_invariants(&def)?;
    Ok(def)
}

/// Extract the agent name from a path. For the flat-file form that is the
/// file stem (`coder.md` → `coder`); the dir form (`coder/`) — reserved
/// for the future per-`llm_mode` layout — would resolve to the directory
/// name. Centralized so the dir form can be accepted later without
/// touching call sites.
fn agent_name_from_path(path: &Path) -> Option<String> {
    if path.is_dir() {
        return path.file_name().map(|s| s.to_string_lossy().into_owned());
    }
    path.file_stem().map(|s| s.to_string_lossy().into_owned())
}

/// The on-disk agents directory inside a discovered config dir.
fn agents_subdir(config_dir: &Path) -> PathBuf {
    config_dir.join("agents")
}

/// Every directory to search for on-disk agent files, in left-to-right
/// override precedence: the layered config dirs (home/global, machine-
/// local, then project ancestors — see [`crate::config::dirs`]) each
/// contribute their `agents/` subdir, followed by any configured
/// `extended.agent_dirs` (tilde-expanded). Reuses the existing config
/// discovery; no parallel scheme.
pub fn agent_search_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = crate::config::dirs::discover_config_dirs(cwd)
        .into_iter()
        .map(|d| agents_subdir(&d.path))
        .collect();
    let cfg = crate::config::extended::load_for_cwd(cwd);
    for d in &cfg.agent_dirs {
        let expanded = shellexpand::tilde(&d.to_string_lossy()).into_owned();
        dirs.push(PathBuf::from(expanded));
    }
    dirs
}

/// Resolve the on-disk path an agent named `name` would resolve to in
/// `dir`, **without** requiring it to exist. Accepts the flat-file form
/// (`<dir>/<name>.md`); structured so the future per-`llm_mode` directory
/// form (`<dir>/<name>/`) can be added here without a rewrite. Returns the
/// existing form when one is present, else the flat-file path (the form
/// eject writes).
pub fn agent_path_in(dir: &Path, name: &str) -> PathBuf {
    let flat = dir.join(format!("{name}.md"));
    if flat.is_file() {
        return flat;
    }
    // Forward-compat seam: a `<dir>/<name>/` directory will hold the
    // per-mode files once LLM modes land. We don't read it yet, but the
    // resolver must not assume a name always maps to exactly one `.md`
    // file — so we surface the directory when it exists.
    let dir_form = dir.join(name);
    if dir_form.is_dir() {
        return dir_form;
    }
    flat
}

/// Find the first existing on-disk override file for `name`, scanning
/// [`agent_search_dirs`] in precedence order. Returns the path (flat-file
/// or — once supported — the dir form) of the highest-precedence match,
/// or `None` when no override exists (the embedded default applies).
pub fn find_override(cwd: &Path, name: &str) -> Option<PathBuf> {
    for dir in agent_search_dirs(cwd) {
        let candidate = agent_path_in(&dir, name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve the effective [`AgentDef`] for `name` at `cwd`: the highest-
/// precedence on-disk override if one exists, else the embedded default
/// (for a built-in name). Returns `Ok(None)` when `name` is neither a
/// built-in nor present on disk. A malformed override file fails loudly
/// (naming its `source`) rather than silently falling back to the
/// embedded default — that would hide the user's mistake.
pub fn resolve(cwd: &Path, name: &str) -> Result<Option<AgentDef>> {
    if let Some(path) = find_override(cwd, name) {
        // Flat-file form only is read today; the dir form is reserved.
        if path.is_dir() {
            bail!(
                "agent `{name}` ({}) uses the per-mode directory form, which is not supported yet",
                path.display()
            );
        }
        return Ok(Some(load_from_file(&path)?));
    }
    Ok(embedded_default(name))
}

/// Discover every agent visible at `cwd`: each built-in (overridden when
/// an on-disk file exists), plus every custom agent found on disk.
/// Override-by-name means a custom file whose stem collides with a
/// built-in name is folded into that built-in's entry, not listed twice.
/// Malformed files are surfaced as `Err` entries paired with the name so
/// callers (the `/settings` page) can show the problem rather than drop
/// the agent silently.
pub fn list_all(cwd: &Path) -> Vec<AgentListing> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<AgentListing> = Vec::new();

    // Built-ins first, in their canonical order, so the list leads with
    // the bundled cast.
    for &name in BUILTIN_AGENT_NAMES {
        let overridden = find_override(cwd, name).is_some();
        let result = resolve(cwd, name).map(|o| o.expect("built-in always resolves"));
        out.push(AgentListing {
            name: name.to_string(),
            kind: AgentKind::Builtin { overridden },
            def: result,
        });
        seen.insert(name.to_string());
    }

    // Then custom agents from disk, de-duplicated across the search path
    // (highest-precedence wins) and skipping built-in names (already
    // folded in above as overrides).
    for dir in agent_search_dirs(cwd) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = agent_file_candidate_name(&path) else {
                continue;
            };
            if seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());
            let def = if path.is_dir() {
                Err(anyhow::anyhow!(
                    "agent `{name}` ({}) uses the per-mode directory form, which is not supported yet",
                    path.display()
                ))
            } else {
                load_from_file(&path)
            };
            out.push(AgentListing {
                name,
                kind: AgentKind::Custom,
                def,
            });
        }
    }

    out
}

/// Return the candidate agent name for a dir entry: the stem of a `.md`
/// file, or a directory name (the reserved per-mode form). Non-`.md`
/// files are ignored.
fn agent_file_candidate_name(path: &Path) -> Option<String> {
    if path.is_dir() {
        return path.file_name().map(|s| s.to_string_lossy().into_owned());
    }
    if path.extension().and_then(|e| e.to_str()) == Some("md") {
        return path.file_stem().map(|s| s.to_string_lossy().into_owned());
    }
    None
}

/// One row in the agents listing: a built-in (possibly overridden) or a
/// custom agent, with its parsed definition or the parse error.
pub struct AgentListing {
    pub name: String,
    pub kind: AgentKind,
    pub def: Result<AgentDef>,
}

/// Whether a listed agent is one of the bundled cast or user-authored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// A built-in agent. `overridden` is true when an on-disk file
    /// shadows its embedded default.
    Builtin { overridden: bool },
    /// A user-authored custom agent (any non-built-in name).
    Custom,
}

/// Eject a built-in agent's embedded default to `<config_dir>/agents/
/// <name>.md` for editing. If an override already exists anywhere on the
/// search path, **do not clobber** it — return its existing path so the
/// caller can open/select it instead. Returns `(path, newly_written)`.
pub fn eject_builtin(cwd: &Path, config_dir: &Path, name: &str) -> Result<(PathBuf, bool)> {
    if !is_builtin_agent(name) {
        bail!("`{name}` is not a built-in agent and cannot be ejected");
    }
    if let Some(existing) = find_override(cwd, name) {
        return Ok((existing, false));
    }
    let def = embedded_default(name).expect("built-in always has an embedded default");
    let dir = agents_subdir(config_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("creating agents dir {}: {e}", dir.display()))?;
    let path = dir.join(format!("{name}.md"));
    let md = def.to_markdown()?;
    std::fs::write(&path, md)
        .map_err(|e| anyhow::anyhow!("writing agent file {}: {e}", path.display()))?;
    Ok((path, true))
}

/// Reset all built-in agent overrides: delete every on-disk override
/// file for a **built-in** name across the whole search path, restoring
/// the embedded defaults. Custom agents (non-built-in names) are never
/// touched. With no overrides present this is a safe no-op. Returns the
/// paths that were removed.
pub fn reset_all_builtins(cwd: &Path) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for dir in agent_search_dirs(cwd) {
        for &name in BUILTIN_AGENT_NAMES {
            let flat = dir.join(format!("{name}.md"));
            if flat.is_file() {
                std::fs::remove_file(&flat)
                    .map_err(|e| anyhow::anyhow!("removing {}: {e}", flat.display()))?;
                removed.push(flat);
            }
            // Reserved per-mode dir form — remove it too so a reset is
            // complete once that form ships.
            let dir_form = dir.join(name);
            if dir_form.is_dir() {
                std::fs::remove_dir_all(&dir_form)
                    .map_err(|e| anyhow::anyhow!("removing {}: {e}", dir_form.display()))?;
                removed.push(dir_form);
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests;
