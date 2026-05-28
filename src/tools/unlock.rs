//! `unlock` — release a held lock without writing.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::engine::tool::{Tool, ToolCtx, ToolOutput};
use crate::tools::common::resolve;

pub struct UnlockTool;

#[async_trait]
impl Tool for UnlockTool {
    fn name(&self) -> &str {
        "unlock"
    }

    fn description(&self) -> &str {
        "Release the lock on a file without writing"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to unlock" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path_arg = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| crate::engine::tool::invalid_input("`path` is required"))?;
        let path = resolve(path_arg, &ctx.cwd);
        ctx.locks.release(&path, &ctx.agent_id)?;
        Ok(ToolOutput::text(format!("unlocked `{}`", path.display())))
    }
}
