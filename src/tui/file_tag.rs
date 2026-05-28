//! Composer `@`-tagging: file/directory suggestions + inline expansion.
//!
//! See `GOALS.md` §1e for the spec. The composer collects `@partial`
//! tokens; this module walks the cwd (gitignore-aware via the `ignore`
//! crate), ranks candidates, and on submit rewrites every `@path[:range]`
//! into a fenced `<file …>` / `<dir …>` block bounded by the read tool's
//! byte cap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::tools::common::{
    OUTPUT_BYTE_CAP, READ_LINE_CAP, looks_binary, read_slice, truncation_marker,
};

/// Size of the visible suggestion window. Matches `AUTOCOMPLETE_ROWS` in
/// `app.rs` — the renderer shows this many rows and scrolls within the
/// full (possibly longer) candidate list.
pub const MAX_SUGGESTIONS: usize = 6;

/// Once a query yields fewer than this many matches at the current
/// depth, [`suggestions`] widens one directory deeper at a time until it
/// reaches this many (or exhausts the subtree). Equal to the visible
/// window so the popup is full whenever the tree can fill it.
const DEEPEN_TARGET: usize = MAX_SUGGESTIONS;

/// Hard ceiling on suggestions returned. The user can arrow through the
/// whole list; this just bounds memory/scan work in pathological trees.
const MAX_RESULTS: usize = 200;

/// Hard ceiling on filesystem entries scanned per `suggestions` call,
/// so a deepening walk in a huge repo can't stall the UI.
const MAX_WALK_ENTRIES: usize = 10_000;

/// Safety bound on deepening depth (symlinks are not followed, so loops
/// aren't possible; this guards against absurdly deep trees).
const MAX_DEEPEN_DEPTH: usize = 32;

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
pub fn suggestions(cwd: &Path, query: &str, counts: &HashMap<String, u64>) -> Vec<Suggestion> {
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

    let name_lower = name_part.to_ascii_lowercase();
    let mut out: Vec<Suggestion> = Vec::new();
    // Breadth-first deepening: matches at the current depth come first;
    // if the level doesn't fill the window we descend one level at a
    // time (into *all* subdirs, since a match can live under a
    // non-matching dir name) until we hit `DEEPEN_TARGET` or run out.
    let mut frontier: Vec<PathBuf> = vec![search_root];
    let mut walked = 0usize;
    let mut depth = 0usize;

    while !frontier.is_empty() && depth < MAX_DEEPEN_DEPTH {
        depth += 1;
        let mut level: Vec<Suggestion> = Vec::new();
        let mut next: Vec<PathBuf> = Vec::new();
        let mut bailed = false;
        for dir in &frontier {
            for (path, is_dir) in level_entries(dir) {
                walked += 1;
                if walked > MAX_WALK_ENTRIES {
                    bailed = true;
                    break;
                }
                // Descend into every subdir regardless of name match.
                if is_dir {
                    next.push(path.clone());
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !name_lower.is_empty() && !name.to_ascii_lowercase().starts_with(&name_lower) {
                    continue;
                }
                let rel = match path.strip_prefix(cwd) {
                    Ok(p) => p.to_string_lossy().replace('\\', "/"),
                    Err(_) => path.to_string_lossy().to_string(),
                };
                let display = if is_dir { format!("{rel}/") } else { rel };
                level.push(Suggestion {
                    replacement: display.clone(),
                    display,
                    is_dir,
                });
                if out.len() + level.len() >= MAX_RESULTS {
                    bailed = true;
                    break;
                }
            }
            if bailed {
                break;
            }
        }
        // Directories first, then 30-day usage count desc (keyed on the
        // replacement path), then alphabetical — applied within this
        // depth level so shallower matches stay on top and the deepening
        // fill sits below them. Dirs stay pinned above a more-frequent
        // file.
        level.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => {
                let ca = counts.get(&a.replacement).copied().unwrap_or(0);
                let cb = counts.get(&b.replacement).copied().unwrap_or(0);
                cb.cmp(&ca).then_with(|| a.display.cmp(&b.display))
            }
        });
        out.extend(level);
        if bailed || out.len() >= DEEPEN_TARGET || out.len() >= MAX_RESULTS {
            break;
        }
        frontier = next;
    }

    out.truncate(MAX_RESULTS);
    out
}

/// List the immediate children of `dir`, gitignore-aware (hidden +
/// gitignored entries filtered), returning `(path, is_dir)`. A depth-1
/// `ignore` walk so the full gitignore stack — including ancestor
/// `.gitignore`s — is honored exactly as the crate intends.
fn level_entries(dir: &Path) -> Vec<(PathBuf, bool)> {
    let mut walker = WalkBuilder::new(dir);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .max_depth(Some(1))
        .follow_links(false);
    let mut out = Vec::new();
    for dent in walker.build().flatten() {
        if dent.depth() == 0 {
            continue;
        }
        let is_dir = dent.file_type().is_some_and(|t| t.is_dir());
        out.push((dent.path().to_path_buf(), is_dir));
    }
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
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
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

/// One `@`-tag the submit-time pass expanded, surfaced to the chat as a
/// harness-automatic tool-call entry (GOALS §1e; the agent didn't invoke
/// it — the composer did). `ok = false` covers "referenced but not
/// inlined" cases (too large, binary, missing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagExpansion {
    /// `"read"` for files, `"list"` for directories.
    pub tool: &'static str,
    /// The tagged path as the user wrote it (display form).
    pub path: String,
    /// One-line detail for the chat entry, e.g. `142 lines`,
    /// `23 entries`, `9001 lines — referenced, not inlined`.
    pub detail: String,
    /// False when nothing was inlined (renders as a `✗`/skip in chat).
    pub ok: bool,
}

/// Result of [`expand_tags`]: the wire payload (tags rewritten into
/// fenced blocks / references) plus the per-tag expansions to surface in
/// the chat.
#[derive(Debug, Clone, Default)]
pub struct ExpandResult {
    pub wire: String,
    pub expansions: Vec<TagExpansion>,
}

/// True when `path` contains a character that would break the
/// whitespace-terminated tag scanner (currently: any whitespace). Such
/// paths must be quoted (`@"path with spaces"`) — the submit-time pass
/// does this automatically for autocompleted tags.
pub fn needs_quoting(path: &str) -> bool {
    path.chars().any(char::is_whitespace)
}

/// Wrap every tracked accepted-tag path (those containing spaces) in
/// quotes on a copy of `buffer`, so the whitespace-terminated scanner in
/// [`expand_tags`] reads them as one token. Content-matched at each `@`
/// boundary (longest path first) — robust to edits elsewhere in the
/// buffer. The composer shows the unquoted form; only this submit-time
/// copy carries the quotes.
pub fn quote_tracked_tags(buffer: &str, accepted: &[String]) -> String {
    let mut tracked: Vec<&String> = accepted.iter().filter(|p| needs_quoting(p)).collect();
    if tracked.is_empty() {
        return buffer.to_string();
    }
    tracked.sort_by_key(|p| std::cmp::Reverse(p.len()));
    let bytes = buffer.as_bytes();
    let mut out = String::with_capacity(buffer.len() + tracked.len() * 2);
    let mut i = 0;
    while i < buffer.len() {
        let at_boundary = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r');
        if bytes[i] == b'@' && at_boundary {
            let rest = &buffer[i + 1..];
            // Don't double-quote an already-quoted tag.
            if !rest.starts_with('"')
                && let Some(p) = tracked.iter().find(|p| rest.starts_with(p.as_str()))
            {
                out.push('@');
                out.push('"');
                out.push_str(p);
                out.push('"');
                i += 1 + p.len();
                continue;
            }
        }
        let len = char_len_at(buffer, i);
        out.push_str(&buffer[i..i + len]);
        i += len;
    }
    out
}

/// Scan `buffer` for every `@path[:range]` (or quoted `@"path"[:range]`)
/// token and rewrite it into a fenced `<file>` / `<dir>` block. Tokens
/// that can't be inlined (missing, binary, too large) survive verbatim
/// with a `[note: ...]` chip. Returns the wire payload plus the per-tag
/// expansions for the chat (GOALS §1e).
pub fn expand_tags(buffer: &str, cwd: &Path) -> ExpandResult {
    let mut wire = String::with_capacity(buffer.len());
    let mut expansions: Vec<TagExpansion> = Vec::new();
    let bytes = buffer.as_bytes();
    let mut i = 0;
    while i < buffer.len() {
        // `@` starts a tag only at the buffer start or after whitespace,
        // so emails (`user@host`) and mid-word `@` don't trigger.
        let at_boundary = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r');
        if bytes[i] == b'@'
            && at_boundary
            && let Some((consumed, path_part, range, raw)) = parse_tag_at(buffer, i)
        {
            let exp = try_inline(cwd, path_part, range, raw);
            wire.push_str(&exp.wire_piece);
            expansions.push(exp.expansion);
            i += consumed;
            continue;
        }
        let len = char_len_at(buffer, i);
        wire.push_str(&buffer[i..i + len]);
        i += len;
    }
    ExpandResult { wire, expansions }
}

/// UTF-8-safe length of the char beginning at byte `i`.
fn char_len_at(s: &str, i: usize) -> usize {
    s[i..].chars().next().map(char::len_utf8).unwrap_or(1)
}

/// `(bytes_consumed_including_@, path, range, raw_token)`.
type ParsedTag<'a> = (usize, &'a str, Option<(usize, usize)>, &'a str);

/// Parse a tag beginning at the `@` byte index `at`. Returns the parsed
/// tag, or `None` for a lone `@` with no body.
fn parse_tag_at(buffer: &str, at: usize) -> Option<ParsedTag<'_>> {
    let after = at + 1;
    let rest = &buffer[after..];
    if let Some(stripped) = rest.strip_prefix('"') {
        // Quoted: @"path"[:range] — read to the closing quote.
        let inner_start = after + 1;
        let close_rel = stripped.find('"')?;
        let path = &buffer[inner_start..inner_start + close_rel];
        if path.is_empty() {
            return None;
        }
        let mut end = inner_start + close_rel + 1; // past closing quote
        let mut range = None;
        if buffer[end..].starts_with(':') {
            let range_start = end + 1;
            let range_end = buffer[range_start..]
                .find(char::is_whitespace)
                .map(|o| range_start + o)
                .unwrap_or(buffer.len());
            if let Some(r) = parse_range(&buffer[range_start..range_end]) {
                range = Some(r);
                end = range_end;
            }
        }
        Some((end - at, path, range, &buffer[at..end]))
    } else {
        // Bare: terminate at the next whitespace.
        let body_end = rest
            .find(char::is_whitespace)
            .map(|o| after + o)
            .unwrap_or(buffer.len());
        if body_end == after {
            return None; // lone '@'
        }
        let body = &buffer[after..body_end];
        let (path, range) = parse_tag_body(body);
        Some((body_end - at, path, range, &buffer[at..body_end]))
    }
}

fn resolve_path(cwd: &Path, path_part: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path_part);
    let p = Path::new(expanded.as_ref());
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// What an `@`-tag contributes to the wire payload + how it shows in chat.
struct Expanded {
    /// Substituted into the wire in place of the raw token: either the
    /// fenced block (success) or the raw token followed by a `[note:…]`
    /// (skip / reference).
    wire_piece: String,
    expansion: TagExpansion,
}

fn try_inline(cwd: &Path, path_part: &str, range: Option<(usize, usize)>, raw: &str) -> Expanded {
    let resolved = resolve_path(cwd, path_part);
    let meta = match std::fs::metadata(&resolved) {
        Ok(m) => m,
        Err(e) => {
            return skip(
                "read",
                path_part,
                raw,
                format!("could not be inlined: {e}"),
                "not found",
            );
        }
    };

    if meta.is_dir() {
        if range.is_some() {
            return skip(
                "list",
                path_part,
                raw,
                "line range not valid for a directory".into(),
                "skipped",
            );
        }
        let (block, count) = render_directory(&resolved, path_part);
        return Expanded {
            wire_piece: block,
            expansion: TagExpansion {
                tool: "list",
                path: path_part.to_string(),
                detail: format!("{count} entries"),
                ok: true,
            },
        };
    }

    let bytes = match std::fs::read(&resolved) {
        Ok(b) => b,
        Err(e) => {
            return skip(
                "read",
                path_part,
                raw,
                format!("could not be inlined: {e}"),
                "unreadable",
            );
        }
    };
    if looks_binary(&bytes) {
        return skip(
            "read",
            path_part,
            raw,
            "file looks binary".into(),
            "binary, skipped",
        );
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();

    // Over-cap full-file tags are left as a *reference*, not inlined —
    // a multi-thousand-line dump the user may not need is exactly the
    // context bloat the token economy avoids (GOALS §1e / §10). A tag
    // with an explicit range is always inlined (the slice is bounded).
    if range.is_none() {
        let line_count = text.lines().count();
        if line_count > READ_LINE_CAP || bytes.len() > OUTPUT_BYTE_CAP {
            let note = format!(
                " [note: @{path_part} is {line_count} lines — not inlined; ask read with offset/limit]"
            );
            return Expanded {
                wire_piece: format!("{raw}{note}"),
                expansion: TagExpansion {
                    tool: "read",
                    path: path_part.to_string(),
                    detail: format!("{line_count} lines — referenced, not inlined"),
                    ok: false,
                },
            };
        }
    }

    let (block, lines_shown) = render_file(&text, path_part, range);
    Expanded {
        wire_piece: block,
        expansion: TagExpansion {
            tool: "read",
            path: path_part.to_string(),
            detail: format!("{lines_shown} lines"),
            ok: true,
        },
    }
}

/// Build a skip `Expanded`: keep the raw token + append a `[note:…]`, and
/// record a not-ok chat entry.
fn skip(tool: &'static str, path: &str, raw: &str, note_body: String, detail: &str) -> Expanded {
    Expanded {
        wire_piece: format!("{raw} [note: @{path} {note_body}]"),
        expansion: TagExpansion {
            tool,
            path: path.to_string(),
            detail: detail.to_string(),
            ok: false,
        },
    }
}

/// Render a file as a line-numbered `<file>` block via the shared `read`
/// formatter. Returns the block and the number of lines shown.
fn render_file(text: &str, display_path: &str, range: Option<(usize, usize)>) -> (String, usize) {
    let (offset, limit) = match range {
        Some((start, end)) => (start, end - start + 1),
        None => (1, READ_LINE_CAP),
    };
    let slice = read_slice(text, offset, limit);
    let lines_shown = slice.numbered.lines().count();
    let mut out = format!("\n<file path=\"{display_path}\">\n{}", slice.numbered);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if slice.truncated {
        out.push_str(&truncation_marker(slice.next_offset));
        out.push('\n');
    }
    out.push_str("</file>\n");
    (out, lines_shown)
}

/// Internal portable directory listing (no shell-out). Returns the
/// `<dir>` block and the total entry count.
fn render_directory(path: &Path, display_path: &str) -> (String, usize) {
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
    (format!("\n<dir path=\"{display}\">\n{body}</dir>\n"), total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// Suggestions with an empty frequency map — these tests exercise
    /// the dirs-first/alpha ordering, not the count tie-breaker.
    fn sug(cwd: &Path, q: &str) -> Vec<Suggestion> {
        suggestions(cwd, q, &HashMap::new())
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
        let out = expand_tags("see @hello.txt please", root.path()).wire;
        assert!(out.contains("<file path=\"hello.txt\">"));
        assert!(out.contains("hello"));
        assert!(out.contains("</file>"));
    }

    #[test]
    fn expand_tags_inlines_with_line_numbers() {
        let root = tmp_root();
        fs::write(root.path().join("hello.txt"), "alpha\nbeta\n").unwrap();
        let res = expand_tags("@hello.txt", root.path());
        // Routed through the read formatter → line-numbered output.
        assert!(res.wire.contains("    1: alpha"), "wire: {}", res.wire);
        assert!(res.wire.contains("    2: beta"), "wire: {}", res.wire);
        assert_eq!(res.expansions.len(), 1);
        assert_eq!(res.expansions[0].tool, "read");
        assert!(res.expansions[0].ok);
    }

    #[test]
    fn expand_tags_over_cap_file_is_referenced_not_inlined() {
        let root = tmp_root();
        let mut big = String::new();
        for i in 0..3000 {
            big.push_str(&format!("line {i}\n"));
        }
        fs::write(root.path().join("big.rs"), big).unwrap();
        let res = expand_tags("@big.rs", root.path());
        // Not inlined: no <file> block; the @path survives + a note.
        assert!(!res.wire.contains("<file"), "wire: {}", res.wire);
        assert!(res.wire.contains("@big.rs"));
        assert!(res.wire.contains("not inlined"));
        assert_eq!(res.expansions.len(), 1);
        assert!(!res.expansions[0].ok);
    }

    #[test]
    fn expand_tags_over_cap_with_range_still_inlines() {
        let root = tmp_root();
        let mut big = String::new();
        for i in 0..3000 {
            big.push_str(&format!("line {i}\n"));
        }
        fs::write(root.path().join("big.rs"), big).unwrap();
        // An explicit range is bounded, so it inlines even on a big file.
        let res = expand_tags("@big.rs:10-12", root.path());
        assert!(res.wire.contains("<file"), "wire: {}", res.wire);
        assert!(res.wire.contains("line 9")); // 1-indexed line 10 == "line 9"
    }

    #[test]
    fn needs_quoting_flags_spaces_only() {
        assert!(needs_quoting("src/my file.rs"));
        assert!(!needs_quoting("src/plain.rs"));
    }

    #[test]
    fn quote_tracked_tags_wraps_spaced_path() {
        let accepted = vec!["src/my file.rs".to_string()];
        let out = quote_tracked_tags("see @src/my file.rs ok", &accepted);
        assert_eq!(out, "see @\"src/my file.rs\" ok");
        // Untracked plain paths are untouched.
        assert_eq!(quote_tracked_tags("see @a.rs", &accepted), "see @a.rs");
    }

    #[test]
    fn expand_tags_inlines_quoted_spaced_path() {
        let root = tmp_root();
        fs::write(root.path().join("my file.rs"), "x = 1\n").unwrap();
        let res = expand_tags("@\"my file.rs\"", root.path());
        assert!(
            res.wire.contains("<file path=\"my file.rs\">"),
            "wire: {}",
            res.wire
        );
        assert!(res.wire.contains("    1: x = 1"));
    }

    #[test]
    fn expand_tags_quoted_path_with_range() {
        let root = tmp_root();
        fs::write(root.path().join("my file.rs"), "a\nb\nc\nd\n").unwrap();
        let res = expand_tags("@\"my file.rs\":2-3", root.path());
        assert!(res.wire.contains("    2: b"));
        assert!(res.wire.contains("    3: c"));
        assert!(!res.wire.contains("    1: a"));
    }

    #[test]
    fn expand_tags_handles_line_range() {
        let root = tmp_root();
        let p = root.path().join("nums.txt");
        let mut f = fs::File::create(&p).unwrap();
        for i in 1..=20 {
            writeln!(f, "line{i}").unwrap();
        }
        let out = expand_tags("@nums.txt:5-7", root.path()).wire;
        assert!(out.contains("line5"));
        assert!(out.contains("line6"));
        assert!(out.contains("line7"));
        assert!(!out.contains("line8"));
    }

    #[test]
    fn expand_tags_keeps_missing_file_literal() {
        let root = tmp_root();
        let out = expand_tags("see @nope.rs ok", root.path()).wire;
        assert!(out.contains("@nope.rs"));
        assert!(out.contains("[note: @nope.rs could not be inlined"));
    }

    #[test]
    fn expand_tags_refuses_binary() {
        let root = tmp_root();
        let p = root.path().join("bin.dat");
        fs::write(&p, [0u8, 1, 2, 3, 4, 5]).unwrap();
        let out = expand_tags("@bin.dat", root.path()).wire;
        assert!(out.contains("looks binary"));
        assert!(!out.contains("<file"));
    }

    #[test]
    fn expand_tags_ignores_mid_word_at() {
        let root = tmp_root();
        let out = expand_tags("email me at user@example.com", root.path()).wire;
        assert_eq!(out, "email me at user@example.com");
    }

    #[test]
    fn expand_tags_directory_listing() {
        let root = tmp_root();
        fs::write(root.path().join("a.txt"), "a").unwrap();
        fs::write(root.path().join("b.txt"), "bb").unwrap();
        fs::create_dir(root.path().join("sub")).unwrap();
        let out = expand_tags("@./", root.path()).wire;
        assert!(out.contains("<dir path=\"./\">"));
        assert!(out.contains("sub/ (dir)"));
        assert!(out.contains("a.txt (file)"));
    }

    #[test]
    fn expand_tags_range_out_of_bounds_yields_empty_body() {
        let root = tmp_root();
        fs::write(root.path().join("x.txt"), "only one line\n").unwrap();
        let out = expand_tags("@x.txt:50-60", root.path()).wire;
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
        let s = sug(root.path(), "");
        let names: Vec<&str> = s.iter().map(|x| x.display.as_str()).collect();
        // Dir first.
        assert_eq!(names.first().copied(), Some("zeta/"));
        assert!(names.iter().any(|n| *n == "alpha.rs"));
    }

    #[test]
    fn suggestions_rank_by_count_then_dirs_pinned() {
        let root = tmp_root();
        fs::write(root.path().join("alpha.rs"), "").unwrap();
        fs::write(root.path().join("beta.rs"), "").unwrap();
        fs::create_dir(root.path().join("zeta")).unwrap();
        // beta is picked more often → ranks above alpha even though alpha
        // sorts first alphabetically. The directory stays pinned on top
        // regardless of the file counts.
        let mut counts = HashMap::new();
        counts.insert("beta.rs".to_string(), 5u64);
        let s = suggestions(root.path(), "", &counts);
        let names: Vec<&str> = s.iter().map(|x| x.display.as_str()).collect();
        assert_eq!(
            names.first().copied(),
            Some("zeta/"),
            "dir not pinned: {names:?}"
        );
        let a = names.iter().position(|n| *n == "alpha.rs").unwrap();
        let b = names.iter().position(|n| *n == "beta.rs").unwrap();
        assert!(
            b < a,
            "more-frequent beta.rs should outrank alpha.rs: {names:?}"
        );
    }

    #[test]
    fn suggestions_prefix_filter() {
        let root = tmp_root();
        fs::write(root.path().join("alpha.rs"), "").unwrap();
        fs::write(root.path().join("beta.rs"), "").unwrap();
        let s = sug(root.path(), "alp");
        let names: Vec<&str> = s.iter().map(|x| x.display.as_str()).collect();
        assert_eq!(names, vec!["alpha.rs"]);
    }

    #[test]
    fn suggestions_deepen_when_shallow_level_is_sparse() {
        // cwd has one file + one subdir; the subdir holds several files.
        // An empty query should deepen into the subdir to fill the list.
        let root = tmp_root();
        fs::write(root.path().join("top.rs"), "").unwrap();
        let sub = root.path().join("sub");
        fs::create_dir(&sub).unwrap();
        for n in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            fs::write(sub.join(n), "").unwrap();
        }
        let s = sug(root.path(), "");
        let names: Vec<&str> = s.iter().map(|x| x.display.as_str()).collect();
        // Level 1 (sub/, top.rs) is only 2 entries → deepen into sub/.
        assert!(names.contains(&"sub/"));
        assert!(names.contains(&"top.rs"));
        assert!(names.contains(&"sub/a.rs"), "deeper entries: {names:?}");
        assert!(names.contains(&"sub/d.rs"), "deeper entries: {names:?}");
    }

    #[test]
    fn suggestions_deepen_prefix_match_finds_nested_file() {
        // Typing a basename that only exists deeper should still surface
        // it via the deepening walk.
        let root = tmp_root();
        let nested = root.path().join("router");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("match.ts"), "").unwrap();
        let s = sug(root.path(), "match");
        let names: Vec<&str> = s.iter().map(|x| x.display.as_str()).collect();
        assert!(names.contains(&"router/match.ts"), "got {names:?}");
    }

    #[test]
    fn suggestions_returns_more_than_window_when_available() {
        // Ten matching files at the top level: all should be returned
        // (the renderer windows them), not truncated to six.
        let root = tmp_root();
        for n in 0..10 {
            fs::write(root.path().join(format!("file{n}.rs")), "").unwrap();
        }
        let s = sug(root.path(), "file");
        assert_eq!(s.len(), 10, "expected all matches, got {}", s.len());
    }

    #[test]
    fn suggestions_skips_hidden_files() {
        let root = tmp_root();
        fs::write(root.path().join(".hidden"), "").unwrap();
        fs::write(root.path().join("visible.txt"), "").unwrap();
        let s = sug(root.path(), "");
        let names: Vec<&str> = s.iter().map(|x| x.display.as_str()).collect();
        assert!(names.iter().all(|n| *n != ".hidden"));
        assert!(names.iter().any(|n| *n == "visible.txt"));
    }
}
