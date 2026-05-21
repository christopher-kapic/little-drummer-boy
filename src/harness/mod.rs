//! External harness invocation — the engine behind `cockpit meta`.
//!
//! Each harness is described in `extended-config.json` (see `GOALS.md` §4)
//! by:
//!   - `command`           the executable
//!   - `args`              argv template, with `{prompt}` and `{agent_file}`
//!                         placeholders
//!   - `prompt_mode`       `"arg"` (substitute `{prompt}`) or `"stdin"`
//!                         (write prompt to child stdin)
//!   - `model_args`        appended when a model is requested
//!   - `agent_file_args`   appended (with `{agent_file}`) when the user
//!                         supplied `--agent-file` and the harness supports it
//!
//! This module parallels the equivalent code in ralph-rs/src/harness.rs and
//! kctx-local/src/harness.rs; we deliberately do not share a crate to keep
//! each tool independently buildable.

use std::path::Path;

use anyhow::Result;

use crate::config::extended::HarnessConfig;

#[derive(Debug)]
pub struct HarnessOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub async fn invoke(
    _harness: &HarnessConfig,
    _prompt: &str,
    _cwd: &Path,
    _model: Option<&str>,
    _agent_file: Option<&Path>,
    _timeout_secs: u64,
) -> Result<HarnessOutput> {
    todo!("spawn the harness subprocess; mirror ralph-rs's signal handling")
}
