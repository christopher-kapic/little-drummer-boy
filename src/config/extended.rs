//! Loader for `extended-config.json` — the cockpit-only config layer.
//!
//! Lives alongside `config.json` in each discovered `.cockpit/` directory
//! (see `config::dirs`). Schema reference: `GOALS.md` §4. All fields are
//! optional; a missing file is fine (defaults apply).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExtendedConfig {
    #[serde(default)]
    pub harnesses: HashMap<String, HarnessConfig>,

    /// Ordered list of agent-guidance file names. The first file from this
    /// list that exists in the cwd (or its ancestors up to the git root)
    /// is loaded. Default: `["AGENTS.md", "CLAUDE.md"]`.
    #[serde(default = "default_agent_guidance_files")]
    pub agent_guidance_files: Vec<String>,

    /// Concurrency model when an agent fans out: `"subagents"` (in-process)
    /// or `"fork"` (separate cockpit/other-harness subprocess per sub-task).
    #[serde(default)]
    pub concurrency: Concurrency,

    /// Extra directories to search for agent definition files. Paths are
    /// tilde-expanded.
    #[serde(default)]
    pub agent_dirs: Vec<PathBuf>,

    #[serde(default)]
    pub redact: RedactConfig,

    #[serde(default)]
    pub tui: TuiConfig,

    /// Opt-in to fetching remote `.well-known/cockpit` configs.
    #[serde(default)]
    pub allow_remote_config: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub prompt_mode: PromptMode,
    #[serde(default)]
    pub model_args: Vec<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub supports_skills: bool,
    #[serde(default)]
    pub supports_agent_file: bool,
    #[serde(default)]
    pub agent_file_args: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    #[default]
    Arg,
    Stdin,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Concurrency {
    #[default]
    Subagents,
    Fork,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactConfig {
    pub enabled: bool,
    pub scan_environment: bool,
    pub scan_dotenv: bool,
    #[serde(default)]
    pub extra_dotenv_paths: Vec<PathBuf>,
    pub min_secret_length: usize,
    pub placeholder: String,
}

impl Default for RedactConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_environment: true,
            scan_dotenv: true,
            extra_dotenv_paths: vec![],
            min_secret_length: 8,
            placeholder: "***redacted-by-cockpit-cli***".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    pub vim_mode: bool,
    pub show_cwd: bool,
    pub show_branch: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            vim_mode: true,
            show_cwd: true,
            show_branch: true,
        }
    }
}

fn default_agent_guidance_files() -> Vec<String> {
    vec!["AGENTS.md".into(), "CLAUDE.md".into()]
}
