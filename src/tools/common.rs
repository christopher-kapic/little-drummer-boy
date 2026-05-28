//! Shared utilities for the file tools.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::engine::tool::ToolCtx;

/// Resolve a path argument the way every file tool does:
///   - tilde-expand,
///   - relative paths join against the session cwd.
pub fn resolve(arg: &str, cwd: &Path) -> PathBuf {
    let expanded = shellexpand::tilde(arg);
    let p = Path::new(expanded.as_ref());
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Tool-result byte cap per GOALS §10.
pub const OUTPUT_BYTE_CAP: usize = 8 * 1024;
/// Default line cap for the read tools (plan §13a / §10).
pub const READ_LINE_CAP: usize = 2000;

/// Build the §10 truncation marker. Includes a hint for the next call
/// the model should issue.
pub fn truncation_marker(next_offset: usize) -> String {
    format!("... [truncated, ask read with offset {next_offset} to see more]")
}

/// Largest char boundary `<= index`. Polyfill for nightly-only
/// `str::floor_char_boundary`; shared by every tool that caps output.
pub fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while !s.is_char_boundary(i) && i > 0 {
        i -= 1;
    }
    i
}

/// Smallest char boundary `>= index`.
pub fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while !s.is_char_boundary(i) && i < s.len() {
        i += 1;
    }
    i
}

/// Cap `s` to `cap` bytes, byte-boundary-safe, keeping a **head and a
/// tail** so the failure signal (which usually surfaces at the tail —
/// stderr, a non-zero exit line, a panic message) survives. The elided
/// middle is replaced with a one-line `[truncated N bytes]` marker.
/// Returns `s` unchanged when it already fits.
pub fn truncate_head_tail(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    // Reserve room for the marker, then split the remaining budget
    // 3:2 between head and tail.
    let marker_reserve = 48;
    let budget = cap.saturating_sub(marker_reserve);
    let head_budget = budget * 3 / 5;
    let tail_budget = budget - head_budget;
    let head_end = floor_char_boundary(s, head_budget);
    let tail_start = ceil_char_boundary(s, s.len().saturating_sub(tail_budget));
    let elided = tail_start.saturating_sub(head_end);
    let mut out = String::with_capacity(head_end + (s.len() - tail_start) + marker_reserve);
    out.push_str(&s[..head_end]);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("... [truncated {elided} bytes] ...\n"));
    out.push_str(&s[tail_start..]);
    out
}

/// Result of [`read_slice`]: the line-numbered body, whether it was
/// capped, and the 1-indexed line the model/composer should pass as the
/// next `offset` to continue reading.
pub struct ReadSlice {
    pub numbered: String,
    pub truncated: bool,
    pub next_offset: usize,
}

/// Core of the `read` tool's output formatting (plan §13a), factored out
/// so composer `@`-tag inlining produces byte-for-byte identical
/// line-numbered, capped output. `offset` is 1-indexed, `limit` is in
/// lines; applies the 2000-line / 8 KB caps. An `offset` past EOF yields
/// an empty body (caller decides how to message it).
pub fn read_slice(text: &str, offset: usize, limit: usize) -> ReadSlice {
    let all: Vec<&str> = text.lines().collect();
    let total = all.len();
    if offset > total {
        return ReadSlice {
            numbered: String::new(),
            truncated: false,
            next_offset: total + 1,
        };
    }
    let start = offset - 1;
    let mut chunk: Vec<&str> = all[start..].to_vec();
    let mut truncated = false;
    if chunk.len() > limit {
        chunk.truncate(limit);
        truncated = true;
    }
    let next_offset = offset + chunk.len();
    let chunk_text = chunk.join("\n");
    let mut numbered = line_number(&chunk_text, offset);
    let byte_cap = OUTPUT_BYTE_CAP.saturating_sub(80);
    if numbered.len() > byte_cap {
        let safe = floor_char_boundary(&numbered, byte_cap);
        numbered.truncate(safe);
        if !numbered.ends_with('\n') {
            numbered.push('\n');
        }
        truncated = true;
    }
    ReadSlice {
        numbered,
        truncated,
        next_offset,
    }
}

/// Line-number a slice of text in the `${n}: ${line}` format plan §13a
/// requires. `start_line` is 1-indexed.
pub fn line_number(text: &str, start_line: usize) -> String {
    let mut out = String::with_capacity(text.len() + text.lines().count() * 6);
    for (i, line) in text.lines().enumerate() {
        out.push_str(&format!("{:>5}: {}\n", start_line + i, line));
    }
    out
}

/// Detect a binary file from the first 1 KB — NUL byte presence, per
/// plan §13a and §1e. Returns true if the file appears binary.
pub fn looks_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    head.contains(&0u8)
}

/// Detect line-ending style (CRLF vs LF) from the first 1 KB.
pub fn detect_crlf(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    head.windows(2).any(|w| w == b"\r\n")
}

/// Write `bytes` to `path`, release the file lock, and mark the path as
/// read for this session. Creates parent directories as needed.
///
/// Centralizes the post-write sequence shared by every write-capable
/// tool. The `note_read` after `release` is a footgun: skipping it
/// leaves the agent unable to re-edit the same file without an
/// intervening read.
pub fn write_and_release(ctx: &ToolCtx, path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes).map_err(|e| anyhow::anyhow!("write `{}`: {e}", path.display()))?;
    ctx.locks.release(path, &ctx.agent_id)?;
    ctx.locks.note_read(path, &ctx.agent_id, ctx.session.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_head_tail_short_input_unchanged() {
        assert_eq!(truncate_head_tail("hello", 100), "hello");
    }

    #[test]
    fn truncate_head_tail_never_panics_on_multibyte_boundary() {
        // The bug this guards: `String::truncate` panics if the cap
        // lands mid-codepoint. Build a string of 4-byte chars so most
        // byte offsets are NOT char boundaries.
        let s = "🚀".repeat(2000); // 8000 bytes, no ASCII boundaries
        let out = truncate_head_tail(&s, 8 * 1024 / 2); // cap below len
        assert!(out.len() <= 8 * 1024 / 2 + 64);
        assert!(out.contains("truncated"));
        // Output must be valid UTF-8 (guaranteed by &str) and split on
        // rocket boundaries only.
        assert!(
            out.chars()
                .all(|c| c == '🚀' || !c.is_alphanumeric() || c.is_ascii())
        );
    }

    #[test]
    fn truncate_head_tail_keeps_head_and_tail() {
        let s = format!("{}TAILMARKER", "x".repeat(20_000));
        let out = truncate_head_tail(&s, 1000);
        assert!(out.starts_with("xxxx"));
        assert!(out.ends_with("TAILMARKER"));
        assert!(out.contains("truncated"));
    }
}

/// Normalize content for writing: if the original file used CRLF,
/// rewrite plain-LF content to CRLF before writing (per
/// miscellaneous.md §1g).
pub fn normalize_line_endings(content: &str, want_crlf: bool) -> String {
    if want_crlf {
        // Idempotent — never re-double an existing CRLF.
        let mut out = String::with_capacity(content.len() + 16);
        for (i, line) in content.split('\n').enumerate() {
            if i > 0 {
                out.push_str("\r\n");
            }
            // strip a trailing \r left from a previous split if the
            // content already used CRLF
            out.push_str(line.strip_suffix('\r').unwrap_or(line));
        }
        out
    } else {
        // Strip any stray \r so an LF-shaped file stays LF.
        content.replace('\r', "")
    }
}
