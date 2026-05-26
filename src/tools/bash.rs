//! `bash` — execute a shell command.
//!
//! Auto-allow for v0 (GOALS bootstrap policy). The `exec_approval` flow
//! and Shift+Tab approval-mode cycling will land alongside the rest of
//! plan §3e.
//!
//! Safety:
//!   - Output is capped at [`crate::tools::common::OUTPUT_BYTE_CAP`].
//!   - The env scrub list from plan §3c removes the well-known
//!     injection-vector vars (`BASH_ENV`, `PROMPT_COMMAND`, …) and
//!     anything matching the `*_KEY` / `*_SECRET` / `*_TOKEN` patterns.
//!     Secrets the model needs come back through the API layer, not
//!     subprocess env.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::engine::tool::{Tool, ToolCtx, ToolOutput};
use crate::tools::common::OUTPUT_BYTE_CAP;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command; returns stdout/stderr/exit code (8 KB cap, 120 s default timeout)"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command":    { "type": "string", "description": "Shell command" },
                "cwd":        { "type": "string", "description": "Working directory; defaults to session cwd" },
                "timeout_ms": { "type": "integer", "description": "Hard timeout in ms (max 600000)" }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("`command` is required"))?;
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(|s| crate::tools::common::resolve(s, &ctx.cwd))
            .unwrap_or_else(|| ctx.cwd.clone());
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        tracing::debug!(command, timeout_ms, "bash: spawning");

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(&cwd);
        scrub_env(&mut cmd);

        let child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::text(format!("Error spawning shell: {e}"))),
        };

        let timeout = std::time::Duration::from_millis(timeout_ms);
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Ok(ToolOutput::text(format!("Error running command: {e}"))),
            Err(_) => {
                return Ok(ToolOutput::truncated_text(format!(
                    "Error: timeout after {timeout_ms} ms"
                )));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit = output.status.code().unwrap_or(-1);
        let signaled = !output.status.success() && output.status.code().is_none();

        let body = format_combined(&stdout, &stderr, exit, signaled);
        if body.len() > OUTPUT_BYTE_CAP {
            let mut truncated: String = body.chars().take(OUTPUT_BYTE_CAP).collect();
            truncated.push_str("\n... [truncated]");
            Ok(ToolOutput::truncated_text(truncated))
        } else {
            Ok(ToolOutput::text(body))
        }
    }
}

fn format_combined(stdout: &str, stderr: &str, exit: i32, signaled: bool) -> String {
    let mut out = String::new();
    if !stdout.is_empty() {
        out.push_str("stdout:\n");
        out.push_str(stdout);
        if !stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    if !stderr.is_empty() {
        out.push_str("stderr:\n");
        out.push_str(stderr);
        if !stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    if signaled {
        out.push_str("exit: signaled\n");
    } else {
        out.push_str(&format!("exit: {exit}\n"));
    }
    out
}

fn scrub_env(cmd: &mut tokio::process::Command) {
    const FIXED_REMOVE: &[&str] = &[
        "BASH_ENV",
        "ENV",
        "PROMPT_COMMAND",
        "NODE_OPTIONS",
        "SHELLOPTS",
        "BASHOPTS",
        "GREP_OPTIONS",
        "GREP_COLORS",
    ];
    for var in FIXED_REMOVE {
        cmd.env_remove(var);
    }
    // Pattern strip: anything ending in _KEY / _SECRET / _TOKEN, case
    // insensitive.
    for (k, _v) in std::env::vars() {
        let upper = k.to_uppercase();
        if upper.ends_with("_KEY") || upper.ends_with("_SECRET") || upper.ends_with("_TOKEN") {
            cmd.env_remove(&k);
        }
    }
}
