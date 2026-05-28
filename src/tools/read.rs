//! `read` — snapshot read with no lock.
//!
//! Used by `orchestrator-build` for shallow inspection and by `coder`
//! for read-only context. Lock-acquiring reads go through
//! [`crate::tools::readlock`]. Both share output format + caps.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::engine::tool::{Tool, ToolCtx, ToolOutput};
use crate::tools::common::{READ_LINE_CAP, looks_binary, read_slice, resolve, truncation_marker};

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
                "path":       { "type": "string", "x-cockpit-kind": "path", "description": "Path to read" },
                "offset":     { "type": "integer", "description": "1-indexed start line (default 1)" },
                "limit":      { "type": "integer", "description": "Max lines (default 2000)" },
                "start_line": { "type": "integer", "description": "1-indexed inclusive range start" },
                "end_line":   { "type": "integer", "description": "1-indexed inclusive range end" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        // Native-tool boundary check (sandboxing part 2): a path outside
        // cwd + session tmp escalates via the approval prompt (naming the
        // exact path) before any read happens.
        if let Some(p) = args.get("path").and_then(Value::as_str) {
            crate::tools::sandbox::check_native_access(ctx, &resolve(p, &ctx.cwd)).await?;
        }
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
        .ok_or_else(|| crate::engine::tool::invalid_input("`path` is required"))?;
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

    // Range mode: an explicit `start_line`/`end_line` reads that
    // inclusive 1-indexed slice and prepends a content-hash header. This
    // is a separate path; when neither is present the behavior below is
    // byte-identical to before.
    if args.get("start_line").is_some() || args.get("end_line").is_some() {
        return read_range(&bytes, &text, &path, args, ctx, was_locked);
    }

    let (offset, default_offset) = match args.get("offset").and_then(Value::as_u64) {
        Some(o) if o >= 1 => (o as usize, false),
        _ => (1, true),
    };
    let (limit, default_limit) = match args.get("limit").and_then(Value::as_u64) {
        Some(l) if l > 0 => (l as usize, false),
        _ => (READ_LINE_CAP, true),
    };

    let total = text.lines().count();
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

    let slice = read_slice(&text, offset, limit);

    let mut prelude = String::new();
    if was_locked {
        prelude.push_str(&format!(
            "Note: lock acquired on `{}`; release with writeunlock / editunlock / unlock.\n",
            path.display()
        ));
    }
    if default_offset && default_limit && slice.truncated {
        prelude.push_str(
            "Note: `limit` defaulted to 2000; pass both `offset` and `limit` to override.\n",
        );
    }
    if slice.truncated {
        let mut tail = slice.numbered;
        tail.push_str(&truncation_marker(slice.next_offset));
        tail.push('\n');
        ctx.locks.note_read(&path, &ctx.agent_id, ctx.session.id);
        return Ok(ToolOutput::truncated_text(format!("{prelude}{tail}")));
    }

    ctx.locks.note_read(&path, &ctx.agent_id, ctx.session.id);
    Ok(ToolOutput::text(format!("{prelude}{}", slice.numbered)))
}

/// Range-mode read: returns the inclusive 1-indexed `[start_line,
/// end_line]` slice with a `[hash=<12hex> total_lines=<n>
/// returned=<a>-<b>]` header so a caller (the intel tools) can verify
/// the file hasn't shifted under it. `end_line` defaults to EOF;
/// `start_line` defaults to 1.
fn read_range(
    bytes: &[u8],
    text: &str,
    path: &std::path::Path,
    args: Value,
    ctx: &ToolCtx,
    was_locked: bool,
) -> Result<ToolOutput> {
    let total = text.lines().count();
    let start = args
        .get("start_line")
        .and_then(Value::as_u64)
        .map(|s| s.max(1) as usize)
        .unwrap_or(1);
    let end = args
        .get("end_line")
        .and_then(Value::as_u64)
        .map(|e| e as usize)
        .unwrap_or(total)
        .max(start);

    // 12-hex prefix of the file's SHA-256.
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let hash = crate::intel::hex_lower(&digest);
    let hash12 = &hash[..hash.len().min(12)];

    ctx.locks.note_read(path, &ctx.agent_id, ctx.session.id);

    if start > total {
        let header = format!("[hash={hash12} total_lines={total} returned=none]\n");
        let note = format!("Note: start_line {start} exceeds file length ({total} lines).\n");
        return Ok(ToolOutput::text(format!("{header}{note}")));
    }
    let end = end.min(total);
    // read_slice handles the 8KB cap + line-numbering; offset = start,
    // limit = inclusive span.
    let slice = read_slice(text, start, end - start + 1);
    let header = format!("[hash={hash12} total_lines={total} returned={start}-{end}]\n");
    let mut prelude = String::new();
    if was_locked {
        prelude.push_str(&format!(
            "Note: lock acquired on `{}`; release with writeunlock / editunlock / unlock.\n",
            path.display()
        ));
    }
    if slice.truncated {
        let mut tail = slice.numbered;
        tail.push_str(&truncation_marker(slice.next_offset));
        tail.push('\n');
        return Ok(ToolOutput::truncated_text(format!(
            "{header}{prelude}{tail}"
        )));
    }
    Ok(ToolOutput::text(format!(
        "{header}{prelude}{}",
        slice.numbered
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::common::test_ctx;

    #[tokio::test]
    async fn range_mode_prepends_hash_header() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let ctx = test_ctx(tmp.path());
        let args = serde_json::json!({
            "path": file.to_string_lossy(),
            "start_line": 2,
            "end_line": 4
        });
        let out = ReadTool.call(args, &ctx).await.unwrap();
        // Header present, with total_lines and the requested range.
        assert!(out.content.starts_with("[hash="), "got: {}", out.content);
        assert!(out.content.contains("total_lines=5"));
        assert!(out.content.contains("returned=2-4"));
        // Only the requested lines are numbered in the body.
        assert!(out.content.contains("    2: l2"));
        assert!(out.content.contains("    4: l4"));
        assert!(!out.content.contains("    1: l1"));
        assert!(!out.content.contains("    5: l5"));
    }

    #[tokio::test]
    async fn plain_mode_has_no_header() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, "a\nb\nc\n").unwrap();
        let ctx = test_ctx(tmp.path());
        let args = serde_json::json!({ "path": file.to_string_lossy() });
        let out = ReadTool.call(args, &ctx).await.unwrap();
        // No range header in the default path — behavior unchanged.
        assert!(!out.content.contains("[hash="));
        assert!(out.content.contains("    1: a"));
        assert!(out.content.contains("    3: c"));
    }
}
