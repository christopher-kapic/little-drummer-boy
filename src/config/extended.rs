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
    #[serde(default, deserialize_with = "deserialize_vim_mode_setting")]
    pub vim_mode: VimModeSetting,
    pub show_cwd: bool,
    pub show_branch: bool,
}

/// Tri-state vim mode: `hint` (default; vim enabled, hint shown on
/// entry to Normal), `enabled` (vim on, no hint), `disabled` (vim off).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum VimModeSetting {
    #[default]
    Hint,
    Enabled,
    Disabled,
}

impl VimModeSetting {
    pub fn vim_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub fn show_hint(self) -> bool {
        matches!(self, Self::Hint)
    }
}

/// Accept the legacy `vim_mode: bool` schema as well as the new
/// string enum. `true` maps to `Hint` (the default), `false` to
/// `Disabled`. Lets us roll the schema forward without breaking
/// existing configs on disk.
fn deserialize_vim_mode_setting<'de, D>(d: D) -> Result<VimModeSetting, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Bool(true) => Ok(VimModeSetting::Hint),
        serde_json::Value::Bool(false) => Ok(VimModeSetting::Disabled),
        serde_json::Value::String(s) => match s.as_str() {
            "hint" => Ok(VimModeSetting::Hint),
            "enabled" => Ok(VimModeSetting::Enabled),
            "disabled" => Ok(VimModeSetting::Disabled),
            other => Err(D::Error::custom(format!(
                "unknown vim_mode `{other}` (expected hint|enabled|disabled)"
            ))),
        },
        serde_json::Value::Null => Ok(VimModeSetting::default()),
        _ => Err(D::Error::custom("vim_mode must be a string or bool")),
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            vim_mode: VimModeSetting::default(),
            show_cwd: true,
            show_branch: true,
        }
    }
}

fn default_agent_guidance_files() -> Vec<String> {
    vec!["AGENTS.md".into(), "CLAUDE.md".into()]
}
