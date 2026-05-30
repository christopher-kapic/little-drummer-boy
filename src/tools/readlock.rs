//! `readlock` — acquire-and-read.
//!
//! Acquires the exclusive lock on the file before reading; releases via
//! `writeunlock` / `editunlock` / `unlock`. Output identical to
//! [`crate::tools::read`].

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::engine::tool::{Tool, ToolCtx, ToolOutput};
use crate::tools::common::resolve;
use crate::tools::read::read_impl;

pub struct ReadlockTool;

#[async_trait]
impl Tool for ReadlockTool {
    fn name(&self) -> &str {
        "readlock"
    }

    fn description(&self) -> &str {
        "Acquire exclusive lock on a file and read it; release with writeunlock/editunlock/unlock"
    }

    fn defensive_description(&self) -> Option<String> {
        Some(
            "Take an exclusive lock on one file AND read its current contents in a single step. \
             Do this BEFORE you change a file: the lock proves no one else is editing it and \
             records the exact bytes you are about to modify, which `writeunlock`/`editunlock` \
             require. Always read-lock immediately before writing — never write a file you have \
             not just locked-and-read. You hold the lock until you release it with `writeunlock` \
             (save changes), `editunlock` (save a search/replace), or `unlock` (abandon with no \
             change). Output is line-numbered and capped like `read`."
                .to_string(),
        )
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string", "x-cockpit-kind": "path", "description": "Path to lock and read" },
                "offset": { "type": "integer", "description": "1-indexed start line (default 1)" },
                "limit":  { "type": "integer", "description": "Max lines (default 2000)" }
            },
            "required": ["path"]
        })
    }

    fn defensive_parameters(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string", "x-cockpit-kind": "path", "description": "Path to the single file to lock and read, absolute or relative to the session working directory; the file must already exist" },
                "offset": { "type": "integer", "description": "1-indexed line number to start reading from; defaults to 1. The lock always covers the whole file regardless of which lines you read" },
                "limit":  { "type": "integer", "description": "Maximum number of lines to return from `offset`; defaults to 2000" }
            },
            "required": ["path"]
        }))
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path_arg = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| crate::engine::tool::invalid_input("`path` is required"))?;
        let path = resolve(path_arg, &ctx.cwd);
        // Native-tool boundary check (sandboxing part 2) before taking
        // the lock — a denied path never acquires.
        crate::tools::sandbox::check_native_access(ctx, &path).await?;
        ctx.locks.acquire(&path, &ctx.agent_id, ctx.session.id)?;
        read_impl(args, ctx, true)
    }
}
