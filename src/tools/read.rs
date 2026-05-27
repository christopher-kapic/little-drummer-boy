//! `read` — snapshot read with no lock.
//!
//! Used by `orchestrator-build` for shallow inspection and by `coder`
//! for read-only context. Lock-acquiring reads go through
//! [`crate::tools::readlock`]. Both share output format + caps.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::engine::tool::{Tool, ToolCtx, ToolOutput};
use crate::tools::common::{
    OUTPUT_BYTE_CAP, READ_LINE_CAP, line_number, looks_binary, resolve, truncation_marker,
};

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Snapshot-read a file; line-numbered output, 2000-line/8KB cap, no lock"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string", "description": "Path to read" },
                "offset": { "type": "integer", "description": "1-indexed start line (default 1)" },
                "limit":  { "type": "integer", "description": "Max lines (default 2000)" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        read_impl(args, ctx, false)
    }
}

/// Shared implementation for `read` and `readlock`. The locking variant
/// acquires the lock first, then calls this. Both produce identical
/// output and both mark the file as read in the lock manager's
/// read-tracker (so a subsequent `writeunlock` is permitted).
pub(crate) fn read_impl(args: Value, ctx: &ToolCtx, was_locked: bool) -> Result<ToolOutput> {
    let path_arg = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("`path` is required"))?;
    let path = resolve(path_arg, &ctx.cwd);

    let bytes =
        std::fs::read(&path).map_err(|e| anyhow::anyhow!("read `{}`: {e}", path.display()))?;
    if looks_binary(&bytes) {
        return Ok(ToolOutput::text(format!(
            "Error: `{}` looks binary (NUL bytes in first 1 KB); use `bash` with `head -c` or `file` for binary inspection",
            path.display()
        )));
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();

    let (offset, default_offset) = match args.get("offset").and_then(Value::as_u64) {
        Some(o) if o >= 1 => (o as usize, false),
        _ => (1, true),
    };
    let (limit, default_limit) = match args.get("limit").and_then(Value::as_u64) {
        Some(l) if l > 0 => (l as usize, false),
        _ => (READ_LINE_CAP, true),
    };

    let mut all_lines: Vec<&str> = text.lines().collect();
    let total = all_lines.len();
    if offset > total {
        let mut out = String::new();
        if default_offset && default_limit {
            // Empty file is a clean read — no Note needed.
        } else {
            out.push_str(&format!(
                "Note: offset {offset} exceeds file length ({total} lines).\n"
            ));
        }
        // Always track the read attempt so a subsequent write is allowed.
        ctx.locks.note_read(&path, &ctx.agent_id, ctx.session.id);
        return Ok(ToolOutput::text(out));
    }
    let mut start_idx = offset - 1;
    let mut chunk: Vec<&str> = all_lines.drain(start_idx..).collect();

    let mut truncated = false;
    if chunk.len() > limit {
        chunk.truncate(limit);
        truncated = true;
    }

    let chunk_text = chunk.join("\n");
    let mut numbered = line_number(&chunk_text, offset);

    let byte_truncate_to = OUTPUT_BYTE_CAP.saturating_sub(80);
    if numbered.len() > byte_truncate_to {
        let safe_truncate = floor_char_boundary(&numbered, byte_truncate_to);
        numbered.truncate(safe_truncate);
        if !numbered.ends_with('\n') {
            numbered.push('\n');
        }
        truncated = true;
    }

    let mut prelude = String::new();
    if was_locked {
        prelude.push_str(&format!(
            "Note: lock acquired on `{}`; release with writeunlock / editunlock / unlock.\n",
            path.display()
        ));
    }
    if default_offset && default_limit && truncated {
        prelude.push_str(
            "Note: `limit` defaulted to 2000; pass both `offset` and `limit` to override.\n",
        );
    }
    if truncated {
        // The "next offset" is the first line we *didn't* show.
        let next_offset = offset + chunk.len();
        let _ = next_offset; // start_idx unused below; keep variable mute
        let mut tail = numbered;
        tail.push_str(&truncation_marker(offset + chunk.len()));
        tail.push('\n');
        start_idx = 0; // silence unused
        let _ = start_idx;
        ctx.locks.note_read(&path, &ctx.agent_id, ctx.session.id);
        return Ok(ToolOutput::truncated_text(format!("{prelude}{tail}")));
    }

    ctx.locks.note_read(&path, &ctx.agent_id, ctx.session.id);
    Ok(ToolOutput::text(format!("{prelude}{numbered}")))
}

/// `floor_char_boundary` polyfill — `str::floor_char_boundary` is still
/// nightly-only.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while !s.is_char_boundary(i) && i > 0 {
        i -= 1;
    }
    i
}
