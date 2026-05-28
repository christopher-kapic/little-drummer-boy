//! Skill discovery, parsing, and body assembly.
//!
//! A *skill* is a `<dir>/<name>/SKILL.md` file: YAML frontmatter
//! (`name`, `description`, optional `model`) plus a markdown body. The
//! `(name, description)` catalog is cheap and surfaced for progressive
//! disclosure (GOALS §10) — bodies load only when a skill is selected by
//! the utility model (auto path) or invoked by name via the `skill` tool
//! (manual path).
//!
//! Scan directories come from [`crate::config::extended::SkillsConfig`].
//! When unset, the defaults are `~/.agents/skills` plus the project-local
//! `./.agents/skills` (resolved against cwd, walked up to the git
//! worktree root). Configured entries support `~` home expansion, `$VAR`
//! references (via [`crate::envref`]), and relative paths resolved against
//! cwd. Non-existent directories are silently ignored; a malformed
//! `SKILL.md` is skipped with a logged warning and never aborts the scan.
//!
//! ## `!`-command processing (Claude vs Codex mode)
//!
//! A body may embed Claude-style inline `` !`command` `` directives.
//! [`render_body`] resolves them according to the auto-`!` toggle:
//!   - **Claude mode (enabled):** run each command, replace the inline
//!     directive with the command's stdout. Output is routed through
//!     [`crate::redact::RedactionTable::scrub`] (non-bypassable, GOALS
//!     §7) before it enters context. A nonzero exit / spawn failure
//!     injects a clear inline error marker rather than crashing the turn.
//!   - **Codex mode (disabled, the default):** the `` !`command` ``
//!     directive is left verbatim — the model sees the literal text and
//!     the command never runs.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::extended::SkillsConfig;
use crate::redact::RedactionTable;

pub mod auto_select;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub frontmatter: SkillFrontmatter,
    pub source: PathBuf,
}

/// Discover every skill reachable from `cwd` under the configured scan
/// directories. Malformed/missing frontmatter skips that skill with a
/// logged warning; a non-existent directory is silently ignored. Results
/// are de-duplicated by skill `name` keeping the first occurrence — the
/// scan-dir order is the precedence order.
pub fn discover(cwd: &Path, cfg: &SkillsConfig) -> Result<Vec<Skill>> {
    let dirs = resolve_scan_dirs(cwd, cfg);
    let mut skills: Vec<Skill> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for dir in dirs {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // Non-existent / unreadable scan dir: silently ignored.
            Err(_) => continue,
        };
        // Sort entries so discovery order is deterministic across
        // platforms (readdir order is filesystem-dependent).
        let mut subdirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();

        for sub in subdirs {
            let manifest = sub.join("SKILL.md");
            if !manifest.is_file() {
                continue;
            }
            match parse_skill(&manifest) {
                Ok(skill) => {
                    if seen.insert(skill.frontmatter.name.clone()) {
                        skills.push(skill);
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %manifest.display(), error = %e, "skipping malformed SKILL.md");
                }
            }
        }
    }

    Ok(skills)
}

/// Parse one `SKILL.md` into a [`Skill`] (frontmatter only — the body is
/// loaded on demand by [`load_body`]). Errors on missing/unparseable
/// frontmatter so [`discover`] can skip-and-warn.
fn parse_skill(path: &Path) -> Result<Skill> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let (frontmatter_src, _body) = split_frontmatter(&raw)
        .with_context(|| format!("no YAML frontmatter in {}", path.display()))?;
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(frontmatter_src)
        .with_context(|| format!("parsing frontmatter in {}", path.display()))?;
    if frontmatter.name.trim().is_empty() {
        anyhow::bail!("SKILL.md frontmatter `name` is empty");
    }
    Ok(Skill {
        frontmatter,
        source: path.to_path_buf(),
    })
}

/// Load a skill's raw markdown body (everything after the frontmatter).
/// On-demand: called only when a skill is selected or invoked.
pub fn load_body(skill: &Skill) -> Result<String> {
    let raw = std::fs::read_to_string(&skill.source)
        .with_context(|| format!("reading {}", skill.source.display()))?;
    match split_frontmatter(&raw) {
        Some((_, body)) => Ok(body.to_string()),
        // A skill with no frontmatter shouldn't have made it through
        // discovery, but tolerate it: the whole file is the body.
        None => Ok(raw),
    }
}

/// Split a `---`-delimited YAML frontmatter block off the front of a
/// markdown document. Returns `(frontmatter_src, body)`. The opening
/// `---` must be the first line; the closing `---` ends the block. `None`
/// when there's no well-formed frontmatter.
///
/// This is cockpit's shared frontmatter splitter for SKILL.md (and the
/// agent-file format); it deliberately avoids pulling in a separate
/// front-matter crate — the parse itself is `serde_yaml`, already a
/// dependency.
fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    // Tolerate a leading BOM before the fence.
    let rest = raw.trim_start_matches('\u{feff}');
    // The opening fence must be the first content.
    if !rest.starts_with("---") {
        return None;
    }
    // Advance past the opening `---` line.
    let after_open = match rest.find('\n') {
        Some(nl) => {
            // Ensure the opening line is *only* `---` (allow trailing CR).
            let first_line = rest[..nl].trim_end_matches('\r');
            if first_line != "---" {
                return None;
            }
            &rest[nl + 1..]
        }
        None => return None,
    };

    // Find the closing fence: a line consisting solely of `---`.
    let mut idx = 0usize;
    for line in after_open.split_inclusive('\n') {
        let bare = line.trim_end_matches('\n').trim_end_matches('\r');
        if bare == "---" {
            let fm = &after_open[..idx];
            let body_start = idx + line.len();
            let body = after_open.get(body_start..).unwrap_or("");
            // Trim a single leading newline so the body starts cleanly.
            let body = body.strip_prefix('\n').unwrap_or(body);
            return Some((fm, body));
        }
        idx += line.len();
    }
    None
}

/// Resolve the ordered list of scan directories for `cwd`. Configured
/// entries (when present) win; otherwise the defaults
/// (`~/.agents/skills` + every `./.agents/skills` from cwd up to the git
/// worktree root) apply. Returned paths are absolute and may not exist —
/// [`discover`] tolerates missing dirs.
pub fn resolve_scan_dirs(cwd: &Path, cfg: &SkillsConfig) -> Vec<PathBuf> {
    if cfg.scan_dirs.is_empty() {
        return default_scan_dirs(cwd);
    }
    cfg.scan_dirs
        .iter()
        .filter_map(|entry| resolve_dir_entry(entry, cwd))
        .collect()
}

/// Default scan dirs: `~/.agents/skills` plus the project-local
/// `.agents/skills` at cwd and each ancestor up to (and including) the
/// git worktree root. Ancestors are walked so a skill checked into a
/// repo root is found from any subdirectory.
fn default_scan_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".agents").join("skills"));
    }

    let stop_at = crate::git::find_worktree_root(cwd);
    let mut dir: Option<&Path> = Some(cwd);
    while let Some(d) = dir {
        out.push(d.join(".agents").join("skills"));
        if let Some(root) = &stop_at
            && d == root.as_path()
        {
            break;
        }
        dir = d.parent();
    }
    out
}

/// Resolve a single configured scan-dir entry. Supports `~` home
/// expansion, `$VAR` references (via [`crate::envref`]), and relative
/// paths resolved against `cwd`. Returns `None` only when `~` can't be
/// expanded (no home dir) — every other shape yields a path.
fn resolve_dir_entry(entry: &str, cwd: &Path) -> Option<PathBuf> {
    // `$VAR` expansion first, so a value like `$PROJECTS/skills` becomes
    // a concrete path before tilde / relative handling.
    let expanded = crate::envref::resolve(entry).value;
    let expanded = expanded.trim();
    if expanded.is_empty() {
        return None;
    }

    // `~` / `~/...` home expansion.
    let tilde = shellexpand::tilde(expanded).into_owned();
    let path = PathBuf::from(tilde);

    if path.is_absolute() {
        Some(path)
    } else {
        Some(cwd.join(path))
    }
}

/// Render a skill body for injection into context, applying the
/// auto-`!`-command toggle. `redact` scrubs Claude-mode command output
/// before it enters context (GOALS §7). In Codex mode (`auto_bang_commands
/// == false`) directives are returned verbatim and no command runs.
pub fn render_body(
    body: &str,
    cwd: &Path,
    auto_bang_commands: bool,
    redact: &RedactionTable,
) -> String {
    if !auto_bang_commands {
        // Codex mode: inject verbatim.
        return body.to_string();
    }
    substitute_bang_commands(body, cwd, redact)
}

/// Walk `body` replacing each `` !`command` `` directive with the
/// command's stdout (Claude mode). Output passes through `redact` before
/// it lands in the returned string. Failures inject a bracketed error
/// marker in place of the directive.
fn substitute_bang_commands(body: &str, cwd: &Path, redact: &RedactionTable) -> String {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    // `i` always sits on a char boundary: the opener `` !` `` and the
    // closing backtick are single-byte ASCII, and the copy step below
    // advances by whole `str::find`/slice spans that begin and end on
    // boundaries.
    let mut i = 0;
    while i < bytes.len() {
        // Look for the `` !` `` opener at the current boundary.
        if bytes[i] == b'!'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'`'
            && let Some(close_rel) = body[i + 2..].find('`')
        {
            let cmd = &body[i + 2..i + 2 + close_rel];
            let replacement = run_bang_command(cmd, cwd, redact);
            out.push_str(&replacement);
            i = i + 2 + close_rel + 1;
            continue;
        }
        // Copy up to (but not including) the next `!`, or the rest of the
        // string if there's no further `!`. This advances by a whole
        // char-boundary-aligned slice without per-codepoint bookkeeping.
        let next = body[i + 1..].find('!').map(|rel| i + 1 + rel);
        let end = next.unwrap_or(bytes.len());
        out.push_str(&body[i..end]);
        i = end;
    }
    out
}

/// Run one inline `!`-command and return the (redacted) stdout, or a
/// bracketed error marker on failure / nonzero exit. Never panics.
fn run_bang_command(cmd: &str, cwd: &Path, redact: &RedactionTable) -> String {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return "[skill command error: empty command]".to_string();
    }
    let output = Command::new("sh")
        .arg("-c")
        .arg(trimmed)
        .current_dir(cwd)
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            // Trim the trailing newline command stdout usually carries so
            // the substitution reads inline-naturally; redact before it
            // enters context.
            redact.scrub(stdout.trim_end_matches('\n'))
        }
        Ok(out) => {
            let code = out
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signaled".to_string());
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stderr = redact.scrub(stderr.trim());
            if stderr.is_empty() {
                format!("[skill command `{trimmed}` failed: exit {code}]")
            } else {
                format!("[skill command `{trimmed}` failed: exit {code}: {stderr}]")
            }
        }
        Err(e) => format!("[skill command `{trimmed}` failed to run: {e}]"),
    }
}

/// Locate a discovered skill by exact `name`. Used by the `skill` tool's
/// manual-invocation path.
pub fn find_by_name<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills.iter().find(|s| s.frontmatter.name == name)
}

/// Build the cheap-model catalog string: one `- name: description` line
/// per skill. This is the only payload the utility model ever sees for
/// selection (token economy, GOALS §10) — never a body.
pub fn catalog_lines(skills: &[Skill]) -> String {
    let mut out = String::new();
    for s in skills {
        out.push_str("- ");
        out.push_str(&s.frontmatter.name);
        out.push_str(": ");
        out.push_str(&s.frontmatter.description);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::extended::RedactConfig;

    fn no_redact() -> RedactionTable {
        RedactionTable::build(&RedactConfig::default(), Path::new("/")).unwrap()
    }

    fn write_skill(dir: &Path, name: &str, frontmatter: &str, body: &str) {
        let sub = dir.join(name);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("SKILL.md"), format!("{frontmatter}{body}")).unwrap();
    }

    #[test]
    fn split_frontmatter_basic() {
        let raw = "---\nname: x\ndescription: y\n---\nBODY HERE\n";
        let (fm, body) = split_frontmatter(raw).unwrap();
        assert!(fm.contains("name: x"));
        assert_eq!(body, "BODY HERE\n");
    }

    #[test]
    fn split_frontmatter_none_when_no_fence() {
        assert!(split_frontmatter("no frontmatter here").is_none());
    }

    #[test]
    fn split_frontmatter_none_when_unterminated() {
        assert!(split_frontmatter("---\nname: x\nno close").is_none());
    }

    #[test]
    fn parse_skill_reads_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "greet",
            "---\nname: greet\ndescription: say hi\n---\n",
            "BODY",
        );
        let skill = parse_skill(&tmp.path().join("greet").join("SKILL.md")).unwrap();
        assert_eq!(skill.frontmatter.name, "greet");
        assert_eq!(skill.frontmatter.description, "say hi");
        assert!(skill.frontmatter.model.is_none());
    }

    #[test]
    fn parse_skill_preserves_optional_model() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "m",
            "---\nname: m\ndescription: d\nmodel: anthropic:claude\n---\n",
            "B",
        );
        let skill = parse_skill(&tmp.path().join("m").join("SKILL.md")).unwrap();
        assert_eq!(skill.frontmatter.model.as_deref(), Some("anthropic:claude"));
    }

    #[test]
    fn discover_finds_configured_dir_and_skips_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let scan = tmp.path().join("scan");
        std::fs::create_dir_all(&scan).unwrap();
        write_skill(&scan, "ok", "---\nname: ok\ndescription: d\n---\n", "B");
        // Malformed: no frontmatter at all.
        let bad = scan.join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("SKILL.md"), "just text, no frontmatter").unwrap();
        // Malformed: frontmatter missing required field.
        write_skill(&scan, "nodesc", "---\nname: nodesc\n---\n", "B");

        let cfg = SkillsConfig {
            scan_dirs: vec![scan.to_string_lossy().into_owned()],
            auto_bang_commands: false,
        };
        let found = discover(tmp.path(), &cfg).unwrap();
        let names: Vec<&str> = found.iter().map(|s| s.frontmatter.name.as_str()).collect();
        assert_eq!(names, vec!["ok"], "only the well-formed skill survives");
    }

    #[test]
    fn resolve_dir_entry_expands_env_and_relative() {
        let cwd = Path::new("/tmp/project");
        // Relative resolves against cwd.
        let rel = resolve_dir_entry("skills/dir", cwd).unwrap();
        assert_eq!(rel, PathBuf::from("/tmp/project/skills/dir"));
        // Absolute stays absolute.
        let abs = resolve_dir_entry("/abs/skills", cwd).unwrap();
        assert_eq!(abs, PathBuf::from("/abs/skills"));
    }

    #[test]
    fn resolve_dir_entry_expands_dollar_var() {
        // SAFETY: single-threaded test; we set then read a unique var.
        unsafe {
            std::env::set_var("COCKPIT_TEST_SKILLS_ROOT", "/var/skills");
        }
        let got = resolve_dir_entry("$COCKPIT_TEST_SKILLS_ROOT/sub", Path::new("/cwd")).unwrap();
        assert_eq!(got, PathBuf::from("/var/skills/sub"));
        unsafe {
            std::env::remove_var("COCKPIT_TEST_SKILLS_ROOT");
        }
    }

    #[test]
    fn default_scan_dirs_includes_home_and_project_local() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = default_scan_dirs(tmp.path());
        // The cwd-local `.agents/skills` is always present.
        assert!(
            dirs.iter()
                .any(|d| d.ends_with(PathBuf::from(".agents").join("skills"))),
            "expected a project-local `.agents/skills` entry, got {dirs:?}"
        );
    }

    #[test]
    fn render_body_codex_mode_injects_verbatim() {
        let body = "before !`echo hi` after";
        let out = render_body(body, Path::new("."), false, &no_redact());
        assert_eq!(out, body, "Codex mode leaves the directive verbatim");
    }

    #[test]
    fn render_body_claude_mode_runs_command() {
        let body = "value: !`echo hello`";
        let out = render_body(body, Path::new("."), true, &no_redact());
        assert_eq!(out, "value: hello", "Claude mode substitutes stdout");
    }

    #[test]
    fn render_body_claude_mode_error_marker_on_failure() {
        let body = "x !`exit 3` y";
        let out = render_body(body, Path::new("."), true, &no_redact());
        assert!(
            out.contains("[skill command") && out.contains("exit 3"),
            "expected an inline error marker, got {out:?}"
        );
        // The turn never crashes — surrounding text survives.
        assert!(out.starts_with("x ") && out.ends_with(" y"));
    }

    #[test]
    fn render_body_claude_mode_scrubs_output() {
        // Build a redaction table that maps a secret value to the
        // placeholder, then have the command echo the secret.
        let mut cfg = RedactConfig::default();
        cfg.denylist = vec!["SUPERSECRETTOKEN".to_string()];
        let redact = RedactionTable::build(&cfg, Path::new("/")).unwrap();
        let body = "leak: !`echo SUPERSECRETTOKEN`";
        let out = render_body(body, Path::new("."), true, &redact);
        assert!(
            !out.contains("SUPERSECRETTOKEN"),
            "Claude-mode output must be scrubbed, got {out:?}"
        );
        assert!(out.contains("REDACTED"), "got {out:?}");
    }

    #[test]
    fn catalog_lines_is_name_description_only() {
        let skills = vec![
            Skill {
                frontmatter: SkillFrontmatter {
                    name: "a".into(),
                    description: "first".into(),
                    model: None,
                },
                source: PathBuf::from("/x/a/SKILL.md"),
            },
            Skill {
                frontmatter: SkillFrontmatter {
                    name: "b".into(),
                    description: "second".into(),
                    model: None,
                },
                source: PathBuf::from("/x/b/SKILL.md"),
            },
        ];
        let cat = catalog_lines(&skills);
        assert_eq!(cat, "- a: first\n- b: second\n");
    }
}
