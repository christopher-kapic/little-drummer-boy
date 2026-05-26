//! Skill discovery.
//!
//! Walks the following locations:
//!   - `<cwd>/.claude/skills/*/SKILL.md`, ancestors up to the git worktree.
//!   - `<cwd>/.cockpit/skills/*/SKILL.md`, ancestors up to the git worktree.
//!   - `~/.claude/skills/`, `~/.cockpit/skills/`, `~/.config/cockpit/skills/`.
//!
//! The `.claude/skills/` locations are intentional: a project may already
//! have Claude Code skills checked in, and cockpit picks them up
//! transparently (the SKILL.md format is shared).
//!
//! Each `SKILL.md` is YAML frontmatter (`name`, `description`, optional
//! `model`/trigger fields) plus a markdown body that is loaded on-demand
//! when the model invokes the native `skill` tool.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

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

pub fn discover(_cwd: &Path) -> Result<Vec<Skill>> {
    todo!("walk cwd ancestors + global dirs, parse SKILL.md frontmatter")
}

/// Load a skill body on demand (the model invokes `skill <name>`).
pub fn load_body(_skill: &Skill) -> Result<String> {
    todo!()
}
