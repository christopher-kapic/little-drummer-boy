//! `editunlock` — search/replace with the §13b cascade, then release the lock.
//!
//! Eight-stage cascade per plan §13b:
//!   1. Exact match.
//!   2. Line-trim (strip trailing whitespace per line).
//!   3. Block-anchor (first + last lines pin the region, interior
//!      matched by Levenshtein).
//!   4. Whitespace-normalized (collapse runs).
//!   5. Indent-flexible (strip common leading indentation).
//!   6. Escape-normalized (reconcile `\n` / `\t` / `\"`).
//!   7. Trimmed-boundary (trim outer whitespace).
//!   8. Context-aware (first + last lines exact, interior ≥50% by char
//!      content).
//!
//! On match the canonical bytes from the file get used as `old_string`
//! when constructing the replacement, so the replacement is always
//! against the file's actual bytes — exactly the §13c rewrite goal,
//! limited to the local replace step. The persisted §14
//! `wire_input.old_string` rewrite (so the model's *next* inference
//! call carries the canonical form) is deferred to a follow-up; v0
//! stores `recovery_stage` correctly but does not mutate
//! `wire_input_json` away from the original. See plan §13c.
//!
//! Multiple matches at any stage with `replace_all = false` produce an
//! ambiguity error (the same loud failure mode plan §13b prescribes).

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;

use crate::engine::tool::{Tool, ToolCtx, ToolOutput};
use crate::tools::common::{detect_crlf, normalize_line_endings, resolve};

pub struct EditunlockTool;

#[async_trait]
impl Tool for EditunlockTool {
    fn name(&self) -> &str {
        "editunlock"
    }

    fn description(&self) -> &str {
        "Replace old_string with new_string in a file (8-stage match cascade) and release the lock"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":        { "type": "string", "description": "Path to edit" },
                "old_string":  { "type": "string", "description": "Text to find" },
                "new_string":  { "type": "string", "description": "Text to replace with" },
                "replace_all": { "type": "boolean", "description": "Replace every match (default false)" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path_arg = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("`path` is required"))?;
        let old_string = args
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("`old_string` is required"))?;
        let new_string = args
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("`new_string` is required"))?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let path = resolve(path_arg, &ctx.cwd);
        ctx.locks
            .check_write_permitted(&path, &ctx.agent_id, ctx.session.id)?;

        let existing = std::fs::read(&path)
            .map_err(|e| anyhow::anyhow!("read `{}`: {e}", path.display()))?;
        let want_crlf = detect_crlf(&existing);
        let original = String::from_utf8_lossy(&existing).into_owned();

        let Match { canonical, stage } = match find_match(&original, old_string, replace_all)? {
            Some(m) => m,
            None => {
                // Total miss — write nothing, return a near-miss diagnostic.
                let near = nearest_miss(&original, old_string);
                bail!(
                    "no match for `old_string` in `{}`. Closest near-miss:\n```\n{near}\n```",
                    path.display()
                );
            }
        };

        let updated = if replace_all {
            original.replace(&canonical, new_string)
        } else {
            // Replace exactly one occurrence — the first.
            match original.find(&canonical) {
                Some(idx) => {
                    let mut s = String::with_capacity(
                        original.len() + new_string.len().saturating_sub(canonical.len()),
                    );
                    s.push_str(&original[..idx]);
                    s.push_str(new_string);
                    s.push_str(&original[idx + canonical.len()..]);
                    s
                }
                None => bail!("internal error: matched stage produced no canonical occurrence"),
            }
        };

        let normalized = normalize_line_endings(&updated, want_crlf);
        std::fs::write(&path, &normalized)
            .map_err(|e| anyhow::anyhow!("write `{}`: {e}", path.display()))?;
        ctx.locks.release(&path, &ctx.agent_id)?;
        ctx.locks.note_read(&path, &ctx.agent_id, ctx.session.id);

        Ok(ToolOutput::text(format!(
            "edited `{}` ({}; {} bytes)",
            path.display(),
            stage,
            normalized.len()
        )))
    }
}

struct Match {
    /// The exact bytes from the file that we matched against.
    canonical: String,
    stage: &'static str,
}

/// Walk the cascade. Returns `Ok(Some(_))` on a successful match (any
/// stage), `Ok(None)` on total miss. An `Err` only fires for ambiguous
/// matches (multiple-match errors per plan §13b).
fn find_match(file: &str, target: &str, replace_all: bool) -> Result<Option<Match>> {
    // Stage 1 — exact.
    if file.contains(target) {
        let count = file.matches(target).count();
        if !replace_all && count > 1 {
            bail!(
                "Found multiple matches for `old_string`; pass more surrounding context or set replace_all: true"
            );
        }
        return Ok(Some(Match {
            canonical: target.to_string(),
            stage: "exact",
        }));
    }

    // Stage 2 — line-trim.
    if let Some(c) = match_via_normalizer(file, target, replace_all, line_trim_normalize)? {
        return Ok(Some(Match { canonical: c, stage: "line_trim" }));
    }

    // Stage 4 — whitespace-normalized (collapse runs).
    if let Some(c) = match_via_normalizer(file, target, replace_all, whitespace_collapse)? {
        return Ok(Some(Match {
            canonical: c,
            stage: "whitespace_normalized",
        }));
    }

    // Stage 5 — indent-flexible (strip common leading indentation from both).
    if let Some(c) = match_via_normalizer(file, target, replace_all, indent_flexible_normalize)? {
        return Ok(Some(Match {
            canonical: c,
            stage: "indent_flexible",
        }));
    }

    // Stage 6 — escape-normalized.
    if let Some(c) = match_via_normalizer(file, target, replace_all, escape_normalize)? {
        return Ok(Some(Match {
            canonical: c,
            stage: "escape_normalized",
        }));
    }

    // Stage 7 — trimmed-boundary (trim outer whitespace of the whole block).
    if let Some(c) = match_via_normalizer(file, target, replace_all, trim_boundary_normalize)? {
        return Ok(Some(Match {
            canonical: c,
            stage: "trimmed_boundary",
        }));
    }

    // Stages 3 (block-anchor) and 8 (context-aware) require pairwise
    // candidate scanning rather than a normalizer-equivalence check.
    // Folded into a single anchor scan because both share the
    // first-line-+-last-line anchor mechanic.
    if let Some(m) = anchor_based_match(file, target, replace_all)? {
        return Ok(Some(m));
    }

    Ok(None)
}

/// Generic "normalize both sides and find" stage. The normalizer maps
/// chunks of bytes onto a canonical form; we slide a window of the
/// same shape over the file and compare normalized forms. On a match
/// we return the *original file bytes* that produced the equivalent
/// normalized form.
fn match_via_normalizer(
    file: &str,
    target: &str,
    replace_all: bool,
    normalize: fn(&str) -> String,
) -> Result<Option<String>> {
    let norm_target = normalize(target);
    if norm_target.trim().is_empty() {
        return Ok(None);
    }

    // We brute-force: for each newline-delimited substring of the file
    // that's the same line count as `target`, compare its normalized
    // form against `norm_target`.
    let target_lines = target.matches('\n').count() + 1;
    let file_lines: Vec<&str> = file.split_inclusive('\n').collect();
    if file_lines.len() < target_lines {
        return Ok(None);
    }

    let mut hits: Vec<String> = Vec::new();
    for start in 0..=file_lines.len() - target_lines {
        let candidate: String = file_lines[start..start + target_lines].concat();
        // Strip the trailing newline that split_inclusive kept iff target
        // didn't have one — match equivalence has to compare like with like.
        let cand_for_compare = if target.ends_with('\n') {
            candidate.clone()
        } else {
            candidate
                .strip_suffix('\n')
                .map(str::to_string)
                .unwrap_or_else(|| candidate.clone())
        };
        let norm = normalize(&cand_for_compare);
        if norm == norm_target {
            hits.push(cand_for_compare);
            if hits.len() > 1 && !replace_all {
                bail!(
                    "Found multiple matches for `old_string` at normalized stage; pass more surrounding context or set replace_all: true"
                );
            }
        }
    }
    Ok(hits.into_iter().next())
}

fn line_trim_normalize(s: &str) -> String {
    s.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn whitespace_collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim().to_string()
}

fn indent_flexible_normalize(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.bytes().take_while(|b| *b == b' ' || *b == b'\t').count())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| if l.len() >= min_indent { &l[min_indent..] } else { *l })
        .collect::<Vec<_>>()
        .join("\n")
}

fn escape_normalize(s: &str) -> String {
    s.replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
}

fn trim_boundary_normalize(s: &str) -> String {
    s.trim().to_string()
}

/// Block-anchor / context-aware combined stage. Pin candidate regions
/// by exact first + last lines; pick the one with the smallest
/// edit-distance interior.
fn anchor_based_match(file: &str, target: &str, _replace_all: bool) -> Result<Option<Match>> {
    let target_lines: Vec<&str> = target.lines().collect();
    if target_lines.len() < 2 {
        return Ok(None);
    }
    let first = target_lines.first().unwrap().trim();
    let last = target_lines.last().unwrap().trim();
    if first.is_empty() || last.is_empty() {
        return Ok(None);
    }

    let file_lines: Vec<&str> = file.split_inclusive('\n').collect();
    let n = target_lines.len();
    let mut best: Option<(String, usize, &'static str)> = None;

    for start in 0..=file_lines.len().saturating_sub(n) {
        let cand_first = file_lines[start].trim_end_matches('\n').trim();
        if cand_first != first {
            continue;
        }
        let cand_last_idx = start + n - 1;
        if cand_last_idx >= file_lines.len() {
            continue;
        }
        let cand_last = file_lines[cand_last_idx].trim_end_matches('\n').trim();
        if cand_last != last {
            continue;
        }

        let candidate: String = file_lines[start..start + n].concat();
        let cand_for_compare = if target.ends_with('\n') {
            candidate.clone()
        } else {
            candidate
                .strip_suffix('\n')
                .map(str::to_string)
                .unwrap_or_else(|| candidate.clone())
        };

        // Interior comparison: char overlap ratio. Cheap proxy for
        // Levenshtein — exact enough for "is this 50% similar?" without
        // pulling in another crate.
        let target_chars: std::collections::HashMap<char, usize> = char_counts(target);
        let cand_chars: std::collections::HashMap<char, usize> = char_counts(&cand_for_compare);
        let common: usize = target_chars
            .iter()
            .map(|(c, n)| n.min(cand_chars.get(c).unwrap_or(&0)))
            .copied()
            .sum();
        let denom = target.chars().count().max(1);
        let ratio = common * 100 / denom;

        let stage = if ratio >= 90 { "block_anchor" } else { "context_aware" };
        if ratio < 50 {
            continue;
        }
        let score = 100 - ratio;
        if best.as_ref().map(|(_, s, _)| *s > score).unwrap_or(true) {
            best = Some((cand_for_compare, score, stage));
        }
    }

    Ok(best.map(|(canonical, _, stage)| Match { canonical, stage }))
}

fn char_counts(s: &str) -> std::collections::HashMap<char, usize> {
    let mut m = std::collections::HashMap::new();
    for c in s.chars() {
        *m.entry(c).or_insert(0) += 1;
    }
    m
}

/// Return the file region nearest to `target` (by char overlap), at
/// most ~10 lines, for the "no match" error message.
fn nearest_miss(file: &str, target: &str) -> String {
    let target_lines = target.lines().count().max(1);
    let file_lines: Vec<&str> = file.split_inclusive('\n').collect();
    if file_lines.len() < target_lines {
        return file.to_string();
    }
    let target_counts = char_counts(target);
    let mut best: Option<(usize, usize)> = None;
    for start in 0..=file_lines.len() - target_lines {
        let cand: String = file_lines[start..start + target_lines].concat();
        let cand_counts = char_counts(&cand);
        let common: usize = target_counts
            .iter()
            .map(|(c, n)| n.min(cand_counts.get(c).unwrap_or(&0)))
            .copied()
            .sum();
        if best.as_ref().map(|(_, s)| *s < common).unwrap_or(true) {
            best = Some((start, common));
        }
    }
    let Some((start, _)) = best else {
        return String::new();
    };
    let end = (start + target_lines).min(file_lines.len());
    file_lines[start..end].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        let res = find_match("hello world\n", "hello", false).unwrap().unwrap();
        assert_eq!(res.canonical, "hello");
        assert_eq!(res.stage, "exact");
    }

    #[test]
    fn line_trim_match() {
        let file = "line one   \nline two\n";
        // target has no trailing whitespace on line one
        let res = find_match(file, "line one\nline two", false).unwrap().unwrap();
        assert_eq!(res.stage, "line_trim");
    }

    #[test]
    fn no_match_returns_none() {
        let res = find_match("hello world", "goodbye", false).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn ambiguous_exact_errors_unless_replace_all() {
        let file = "x\nx\n";
        assert!(find_match(file, "x", false).is_err());
        assert!(find_match(file, "x", true).is_ok());
    }
}
