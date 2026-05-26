//! Composer `@`-tagging: file/directory suggestions + inline expansion.
//!
//! See `GOALS.md` §1e for the spec. The composer collects `@partial`
//! tokens; this module walks the cwd (gitignore-aware via the `ignore`
//! crate), ranks candidates, and on submit rewrites every `@path[:range]`
//! into a fenced `<file …>` / `<dir …>` block bounded by the read tool's
//! byte cap.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::tools::common::{OUTPUT_BYTE_CAP, looks_binary, truncation_marker};

/// Max suggestions returned by [`suggestions`]. Matches `AUTOCOMPLETE_ROWS`
/// in `app.rs` — the renderer truncates / pads to the same number.
pub const MAX_SUGGESTIONS: usize = 6;

/// Max directory entries shown for an `@dir/` inline expansion. Anything
/// beyond this becomes a `... N more entries` footer.
const DIR_ENTRY_CAP: usize = 100;

/// One file/directory suggestion the popup renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// Display path relative to `cwd` (forward slashes; trailing `/` for
    /// directories).
    pub display: String,
    /// Replacement text inserted on accept (without the leading `@`).
    pub replacement: String,
    /// True if this entry is a directory.
    pub is_dir: bool,
}

/// Return up to `MAX_SUGGESTIONS` candidates matching `query`, walked
/// from `cwd`. Outside a git repo `ignore` falls back to walking
/// everything readable; inside, gitignored + hidden entries are filtered.
pub fn suggestions(cwd: &Path, query: &str) -> Vec<Suggestion> {
    let query = query.trim_start_matches('@');
    let (dir_part, name_part) = split_query(query);
    let search_root = if dir_part.is_empty() {
        cwd.to_path_buf()
    } else {
        let resolved = resolve_query_dir(cwd, dir_part);
        // If the query references a missing subdir, fall back to cwd so
        // the popup still shows something (helps catch typos earlier).
        if resolved.is_dir() {
            resolved
        } else {
            cwd.to_path_buf()
        }
    };

    let mut walker = WalkBuilder::new(&search_root);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .max_depth(Some(1))
        .follow_links(false);

    let name_lower = name_part.to_ascii_lowercase();
    let mut out: Vec<Suggestion> = Vec::new();
    for dent in walker.build().flatten() {
        if dent.depth() == 0 {
            continue;
        }
        let name = dent.file_name().to_string_lossy().to_string();
        if !name_lower.is_empty() && !name.to_ascii_lowercase().starts_with(&name_lower) {
            continue;
        }
        let is_dir = dent.file_type().is_some_and(|t| t.is_dir());
        let rel = match dent.path().strip_prefix(cwd) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => dent.path().to_string_lossy().to_string(),
        };
        let display = if is_dir { format!("{rel}/") } else { rel.clone() };
        let replacement = display.clone();
        out.push(Suggestion {
            display,
            replacement,
            is_dir,
        });
    }

    // Directories first, then files; alphabetical within each group.
    out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.display.cmp(&b.display),
    });
    out.truncate(MAX_SUGGESTIONS);
    out
}

/// Split `"src/foo"` into (`"src"`, `"foo"`). A trailing slash means the
/// whole query is the dir part with an empty name filter.
fn split_query(q: &str) -> (&str, &str) {
    if let Some(idx) = q.rfind('/') {
        (&q[..idx], &q[idx + 1..])
    } else {
        ("", q)
    }
}

fn resolve_query_dir(cwd: &Path, dir_part: &str) -> PathBuf {
    let p = Path::new(dir_part);
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

/// Parse a tag body like `path/to/file.rs:10-80` into (path, range).
/// `path` is the raw substring after `@`; `range` is `Some((start,end))`
/// 1-indexed inclusive when a `:` suffix is present.
fn parse_tag_body(body: &str) -> (&str, Option<(usize, usize)>) {
    if let Some(colon) = body.rfind(':') {
        let (lhs, rhs) = (&body[..colon], &body[colon + 1..]);
        if let Some(range) = parse_range(rhs) {
            return (lhs, Some(range));
        }
    }
    (body, None)
}

fn parse_range(s: &str) -> Option<(usize, usize)> {
    if let Some((a, b)) = s.split_once('-') {
        let start: usize = a.parse().ok()?;
        let end: usize = b.parse().ok()?;
        if start == 0 || end < start {
            return None;
        }
        Some((start, end))
    } else {
        let n: usize = s.parse().ok()?;
        if n == 0 { None } else { Some((n, n)) }
    }
}

/// Scan `buffer` for every `@path[:range]` token and rewrite it into a
/// fenced `<file>` / `<dir>` block. Tokens that can't be inlined (missing
/// file, binary, etc.) survive verbatim and gain a `[note: ...]` chip.
pub fn expand_tags(buffer: &str, cwd: &Path) -> String {
    let mut out = String::with_capacity(buffer.len());
    let bytes = buffer.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        // `@` starts a tag only at the buffer start or after whitespace,
        // matching how the composer lets users type emails / handles
        // mid-word without triggering expansion.
        let at_boundary = i == 0
            || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r');
        if ch == b'@' && at_boundary {
            let body_start = i + 1;
            let mut j = body_start;
            while j < bytes.len() && !is_tag_terminator(bytes[j]) {
                j += 1;
            }
            if j > body_start {
                let body = &buffer[body_start..j];
                let (path_part, range) = parse_tag_body(body);
                match try_inline(cwd, path_part, range) {
                    Ok(block) => {
                        out.push_str(&block);
                        i = j;
                        continue;
                    }
                    Err(reason) => {
                        out.push('@');
                        out.push_str(body);
                        out.push_str(&format!(
                            " [note: @{body} could not be inlined: {reason}]"
                        ));
                        i = j;
                        continue;
                    }
                }
            }
        }
        out.push(ch as char);
        i += 1;
    }
    out
}

fn is_tag_terminator(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn try_inline(
    cwd: &Path,
    path_part: &str,
    range: Option<(usize, usize)>,
) -> Result<String, String> {
    let resolved = resolve_path(cwd, path_part);
    let meta = std::fs::metadata(&resolved).map_err(|e| format!("{e}"))?;
    if meta.is_dir() {
        if range.is_some() {
            return Err("line range not valid for a directory".into());
        }
        return Ok(render_directory(&resolved, path_part));
    }
    let bytes = std::fs::read(&resolved).map_err(|e| format!("{e}"))?;
    if looks_binary(&bytes) {
        return Err("file looks binary".into());
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(render_file(&text, path_part, range))
}

fn resolve_path(cwd: &Path, path_part: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path_part);
    let p = Path::new(expanded.as_ref());
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

fn render_file(text: &str, display_path: &str, range: Option<(usize, usize)>) -> String {
    let (body, next_offset, truncated) = slice_body(text, range);
    let mut content = body;
    let cap = OUTPUT_BYTE_CAP;
    let mut byte_truncated = false;
    if content.len() > cap {
        let safe = floor_char_boundary(&content, cap);
        content.truncate(safe);
        if !content.ends_with('\n') {
            content.push('\n');
        }
        byte_truncated = true;
    }
    let mut out = format!("\n<file path=\"{display_path}\">\n{content}");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if truncated || byte_truncated {
        out.push_str(&truncation_marker(next_offset));
        out.push('\n');
    }
    out.push_str("</file>\n");
    out
}

/// Extract the requested line slice (or full body when `range` is None).
/// Returns (body, next_offset_for_marker, was_line_truncated).
fn slice_body(text: &str, range: Option<(usize, usize)>) -> (String, usize, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    match range {
        Some((start, end)) => {
            if start > total {
                return (String::new(), total + 1, false);
            }
            let lo = start - 1;
            let hi = end.min(total);
            let chunk = lines[lo..hi].join("\n");
            let mut chunk = chunk;
            chunk.push('\n');
            (chunk, hi + 1, hi < end)
        }
        None => {
            let mut body = text.to_string();
            if !body.ends_with('\n') && !body.is_empty() {
                body.push('\n');
            }
            (body, total + 1, false)
        }
    }
}

fn render_directory(path: &Path, display_path: &str) -> String {
    let display = if display_path.ends_with('/') {
        display_path.to_string()
    } else {
        format!("{display_path}/")
    };
    let mut entries: Vec<(String, bool, u64)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(path) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let (is_dir, size) = match ent.metadata() {
                Ok(m) => (m.is_dir(), m.len()),
                Err(_) => (false, 0),
            };
            entries.push((name, is_dir, size));
        }
    }
    entries.sort_by(|a, b| match (a.1, b.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(&b.0),
    });
    let total = entries.len();
    let mut body = String::new();
    for (name, is_dir, size) in entries.iter().take(DIR_ENTRY_CAP) {
        let kind = if *is_dir { "dir" } else { "file" };
        if *is_dir {
            body.push_str(&format!("{name}/ ({kind})\n"));
        } else {
            body.push_str(&format!("{name} ({kind}) {size}\n"));
        }
    }
    if total > DIR_ENTRY_CAP {
        let remaining = total - DIR_ENTRY_CAP;
        body.push_str(&format!(
            "... {remaining} more entries; @-tag a subdirectory or ask explore for a search\n"
        ));
    }
    format!("\n<dir path=\"{display}\">\n{body}</dir>\n")
}

/// Polyfill for nightly-only `str::floor_char_boundary`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn parse_range_single_and_pair() {
        assert_eq!(parse_range("42"), Some((42, 42)));
        assert_eq!(parse_range("10-80"), Some((10, 80)));
        assert_eq!(parse_range("0"), None);
        assert_eq!(parse_range("10-5"), None);
        assert_eq!(parse_range("nope"), None);
    }

    #[test]
    fn parse_tag_body_splits_path_and_range() {
        assert_eq!(parse_tag_body("foo.rs"), ("foo.rs", None));
        assert_eq!(parse_tag_body("foo.rs:42"), ("foo.rs", Some((42, 42))));
        assert_eq!(parse_tag_body("foo.rs:10-80"), ("foo.rs", Some((10, 80))));
        // Trailing non-range colon survives as part of the path.
        assert_eq!(parse_tag_body("weird:name"), ("weird:name", None));
    }

    #[test]
    fn expand_tags_inlines_existing_file() {
        let root = tmp_root();
        let p = root.path().join("hello.txt");
        fs::write(&p, "hello\nworld\n").unwrap();
        let out = expand_tags("see @hello.txt please", root.path());
        assert!(out.contains("<file path=\"hello.txt\">"));
        assert!(out.contains("hello"));
        assert!(out.contains("</file>"));
    }

    #[test]
    fn expand_tags_handles_line_range() {
        let root = tmp_root();
        let p = root.path().join("nums.txt");
        let mut f = fs::File::create(&p).unwrap();
        for i in 1..=20 {
            writeln!(f, "line{i}").unwrap();
        }
        let out = expand_tags("@nums.txt:5-7", root.path());
        assert!(out.contains("line5"));
        assert!(out.contains("line6"));
        assert!(out.contains("line7"));
        assert!(!out.contains("line8"));
    }

    #[test]
    fn expand_tags_keeps_missing_file_literal() {
        let root = tmp_root();
        let out = expand_tags("see @nope.rs ok", root.path());
        assert!(out.contains("@nope.rs"));
        assert!(out.contains("[note: @nope.rs could not be inlined"));
    }

    #[test]
    fn expand_tags_refuses_binary() {
        let root = tmp_root();
        let p = root.path().join("bin.dat");
        fs::write(&p, [0u8, 1, 2, 3, 4, 5]).unwrap();
        let out = expand_tags("@bin.dat", root.path());
        assert!(out.contains("looks binary"));
        assert!(!out.contains("<file"));
    }

    #[test]
    fn expand_tags_ignores_mid_word_at() {
        let root = tmp_root();
        let out = expand_tags("email me at user@example.com", root.path());
        assert_eq!(out, "email me at user@example.com");
    }

    #[test]
    fn expand_tags_directory_listing() {
        let root = tmp_root();
        fs::write(root.path().join("a.txt"), "a").unwrap();
        fs::write(root.path().join("b.txt"), "bb").unwrap();
        fs::create_dir(root.path().join("sub")).unwrap();
        let out = expand_tags("@./", root.path());
        assert!(out.contains("<dir path=\"./\">"));
        assert!(out.contains("sub/ (dir)"));
        assert!(out.contains("a.txt (file)"));
    }

    #[test]
    fn expand_tags_range_out_of_bounds_yields_empty_body() {
        let root = tmp_root();
        fs::write(root.path().join("x.txt"), "only one line\n").unwrap();
        let out = expand_tags("@x.txt:50-60", root.path());
        // No content, but the block still renders.
        assert!(out.contains("<file path=\"x.txt\">"));
        assert!(out.contains("</file>"));
    }

    #[test]
    fn suggestions_lists_cwd_entries() {
        let root = tmp_root();
        fs::write(root.path().join("alpha.rs"), "").unwrap();
        fs::write(root.path().join("beta.rs"), "").unwrap();
        fs::create_dir(root.path().join("zeta")).unwrap();
        let s = suggestions(root.path(), "");
        let names: Vec<&str> = s.iter().map(|x| x.display.as_str()).collect();
        // Dir first.
        assert_eq!(names.first().copied(), Some("zeta/"));
        assert!(names.iter().any(|n| *n == "alpha.rs"));
    }

    #[test]
    fn suggestions_prefix_filter() {
        let root = tmp_root();
        fs::write(root.path().join("alpha.rs"), "").unwrap();
        fs::write(root.path().join("beta.rs"), "").unwrap();
        let s = suggestions(root.path(), "alp");
        let names: Vec<&str> = s.iter().map(|x| x.display.as_str()).collect();
        assert_eq!(names, vec!["alpha.rs"]);
    }

    #[test]
    fn suggestions_skips_hidden_files() {
        let root = tmp_root();
        fs::write(root.path().join(".hidden"), "").unwrap();
        fs::write(root.path().join("visible.txt"), "").unwrap();
        let s = suggestions(root.path(), "");
        let names: Vec<&str> = s.iter().map(|x| x.display.as_str()).collect();
        assert!(names.iter().all(|n| *n != ".hidden"));
        assert!(names.iter().any(|n| *n == "visible.txt"));
    }
}
