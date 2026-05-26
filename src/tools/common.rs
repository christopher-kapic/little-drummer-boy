//! Shared utilities for the file tools.

use std::path::{Path, PathBuf};

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
