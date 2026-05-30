//! Codebase-intelligence tools (GOALS §21, Phase 1).
//!
//! Eight tools backed by the on-demand [`crate::intel::Index`]: `tree`,
//! `outline`, `symbol_find`, `word`, `deps`, `hot`, `circular`,
//! `search`. Each index-backed tool calls [`Index::ensure_fresh`] first
//! so it never answers from stale data. `hot` is pure-FS (no index).
//! `search` shells `rg --json` (falling back to `grep -rn`) and
//! budget-caps its output via [`crate::intel::budget::BudgetedWriter`].
//!
//! Output never self-scrubs: `engine::agent::turn` runs every tool
//! result through `redact::scrub` before it reaches the model.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::Value;

use crate::engine::tool::{Tool, ToolCtx, ToolOutput, invalid_input};
use crate::intel::budget::BudgetedWriter;
use crate::intel::lang::{Language, regex_outline};
use crate::intel::{DepEdge, Index};

/// Token cap shared by the index tools. `search` uses a larger default
/// per the spec (4000); structural tools are terser so a tighter cap
/// keeps them well within the §10 economy.
const SEARCH_TOKEN_CAP: usize = 4000;
const STRUCT_TOKEN_CAP: usize = 3000;

/// Build an index handle from the tool ctx (project-root scoped).
fn index_of(ctx: &ToolCtx) -> Index {
    Index::new(ctx.session.db.clone(), ctx.session.project_root.clone())
}

/// Normalize a path arg to a relative forward-slash path against the
/// project root — the form stored in the index.
fn rel_path(arg: &str, ctx: &ToolCtx) -> String {
    let root = &ctx.session.project_root;
    let abs = crate::tools::common::resolve(arg, &ctx.cwd);
    match abs.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => arg.trim_start_matches("./").replace('\\', "/"),
    }
}

fn finish(writer: BudgetedWriter, note: &str) -> ToolOutput {
    if writer.is_truncated() {
        let mut out = writer.into_string();
        out.push_str(note);
        ToolOutput::truncated_text(out)
    } else {
        ToolOutput::text(writer.into_string())
    }
}

// ---- tree ------------------------------------------------------------------

pub struct TreeTool;

#[async_trait]
impl Tool for TreeTool {
    fn name(&self) -> &str {
        "tree"
    }
    fn description(&self) -> &str {
        "List indexed files with language, size, line count, and symbol count"
    }
    fn defensive_description(&self) -> Option<String> {
        Some(
            "Get a map of the codebase: every indexed source file with its language, size, line \
             count, and number of symbols. Use this first when you're new to a repo to see how \
             it's laid out and where the big/important files are, before diving in with `read`. \
             Pass `path` to limit the listing to one subtree. This reads cockpit's on-demand \
             code index, so it is faster and quieter than shelling out to `ls`/`find`."
                .to_string(),
        )
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "x-cockpit-kind": "path", "description": "Subtree path filter relative to project root" }
            }
        })
    }
    fn defensive_parameters(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "x-cockpit-kind": "path", "description": "Optional subtree to restrict the listing to, relative to the project root; omit to list the whole indexed tree" }
            }
        }))
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let index = index_of(ctx);
        index.ensure_fresh().await?;
        let filter = args
            .get("path")
            .and_then(Value::as_str)
            .map(|s| rel_path(s, ctx));

        // Indexed files (with symbol counts) keyed by path.
        let indexed: HashMap<String, (String, i64, i64)> = index
            .tree_rows()?
            .into_iter()
            .map(|(p, lang, size, syms)| (p, (lang, size, syms)))
            .collect();

        // The on-disk gitignore walk is the authority for which files
        // exist (it sees unknown-language files the index doesn't store).
        let mut entries = list_files(&ctx.session.project_root);
        entries.sort();

        let mut writer = BudgetedWriter::new(STRUCT_TOKEN_CAP);
        for (rel, abs, size) in &entries {
            if let Some(f) = &filter
                && !(rel == f || rel.starts_with(&format!("{f}/")))
            {
                continue;
            }
            let lang = Language::from_path(Path::new(rel));
            let (lang_str, sym_part) = match indexed.get(rel) {
                Some((l, _s, syms)) => (l.clone(), format!("[{syms} sym]")),
                None => (lang.as_str().to_string(), "[not indexed]".to_string()),
            };
            let lines = count_lines(abs);
            let line = format!("{rel}  {lang_str} {size}b {lines}L {sym_part}");
            if !writer.writeln(&line) {
                break;
            }
        }
        if writer.is_empty() && !writer.is_truncated() {
            return Ok(ToolOutput::text("No files match.".to_string()));
        }
        Ok(finish(
            writer,
            "\n... [truncated; pass `path` to scope to a subtree]\n",
        ))
    }
}

// ---- outline ---------------------------------------------------------------

pub struct OutlineTool;

#[async_trait]
impl Tool for OutlineTool {
    fn name(&self) -> &str {
        "outline"
    }
    fn description(&self) -> &str {
        "Show a file's symbols and imports in line order; regex fallback for unknown languages"
    }
    fn defensive_description(&self) -> Option<String> {
        Some(
            "Get a structural outline of one file: its functions, types, methods, and imports \
             listed in source order with their line numbers — without reading the whole file. \
             Use this to understand what a file contains and jump straight to the right line \
             with a ranged `read`, instead of paging through it. Cheaper than reading the file \
             when you only need its shape. Falls back to a regex scan for languages cockpit \
             can't fully parse."
                .to_string(),
        )
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "x-cockpit-kind": "path", "description": "File path to outline" }
            },
            "required": ["path"]
        })
    }
    fn defensive_parameters(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "x-cockpit-kind": "path", "description": "Path to the single source file to outline, relative to the project root or absolute" }
            },
            "required": ["path"]
        }))
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path_arg = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("`path` is required"))?;
        // Native-tool boundary check (sandboxing part 2): the regex
        // fallback below reads the file off disk, so an out-of-cwd path
        // must escalate first.
        crate::tools::sandbox::check_native_access(
            ctx,
            &crate::tools::common::resolve(path_arg, &ctx.cwd),
        )
        .await?;
        let rel = rel_path(path_arg, ctx);
        let index = index_of(ctx);
        index.ensure_fresh().await?;

        let (symbols, imports, language) = index.outline_rows(&rel)?;
        let mut writer = BudgetedWriter::new(STRUCT_TOKEN_CAP);

        // Unknown / not-indexed language → regex fallback (never errors).
        if language.is_empty() || language == "unknown" {
            let abs = crate::tools::common::resolve(path_arg, &ctx.cwd);
            let body = match std::fs::read_to_string(&abs) {
                Ok(b) => b,
                Err(e) => {
                    return Err(invalid_input(format!("read `{rel}`: {e}")));
                }
            };
            writer.writeln(&format!(
                "{rel} (unknown language — regex outline, may be incomplete)"
            ));
            let hits = regex_outline(&body);
            if hits.is_empty() {
                writer.writeln("  (no definitions matched)");
            }
            for (name, line) in hits {
                if !writer.writeln(&format!("  {line}: {name}")) {
                    break;
                }
            }
            return Ok(finish(writer, "\n... [truncated]\n"));
        }

        writer.writeln(&format!("{rel} ({language})"));
        if !imports.is_empty() {
            writer.writeln("imports:");
            for (target, line) in &imports {
                if !writer.writeln(&format!("  {line}: {target}")) {
                    return Ok(finish(writer, "\n... [truncated]\n"));
                }
            }
        }
        if !symbols.is_empty() {
            writer.writeln("symbols:");
            for s in &symbols {
                let vis = s
                    .visibility
                    .as_deref()
                    .map(|v| format!("{v} "))
                    .unwrap_or_default();
                let parent = s
                    .parent
                    .as_deref()
                    .map(|p| format!("{p}."))
                    .unwrap_or_default();
                let span = if s.end_line > s.line {
                    format!("{}-{}", s.line, s.end_line)
                } else {
                    s.line.to_string()
                };
                // Prefer the captured signature (first source line) for
                // callables; fall back to the synthesized form otherwise.
                let sig = match (s.kind.as_str(), &s.signature) {
                    ("function" | "method", Some(sig)) if !sig.is_empty() => {
                        format!("{vis}{}", sig.trim())
                    }
                    _ => format!("{vis}{} {parent}{}", s.kind, s.name),
                };
                if !writer.writeln(&format!("  {span}: {sig}")) {
                    break;
                }
            }
        }
        if symbols.is_empty() && imports.is_empty() {
            writer.writeln("  (no symbols or imports)");
        }
        Ok(finish(writer, "\n... [truncated]\n"))
    }
}

// ---- symbol_find -----------------------------------------------------------

pub struct SymbolFindTool;

#[async_trait]
impl Tool for SymbolFindTool {
    fn name(&self) -> &str {
        "symbol_find"
    }
    fn description(&self) -> &str {
        "Find symbol definitions by name (exact or prefix), optionally filtered by kind"
    }
    fn defensive_description(&self) -> Option<String> {
        Some(
            "Find where a symbol is DEFINED — function, struct, class, method, etc. — by name, \
             across the whole indexed codebase, and get the file + line of each definition. Use \
             this to answer \"where is X defined?\" instead of grepping: it returns definitions \
             only, not every mention. By default it matches `name` as a prefix (good for \
             discovery); set `exact` for an exact name, and `kind` to narrow to one symbol kind. \
             To find every USE of a name instead of its definition, use `word`."
                .to_string(),
        )
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name":   { "type": "string", "description": "Symbol name or prefix" },
                "exact":  { "type": "boolean", "description": "Exact-match toggle (default prefix match)" },
                "kind":   { "type": "string", "description": "Kind filter (function/struct/class/method/...)" }
            },
            "required": ["name"]
        })
    }
    fn defensive_parameters(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "name":   { "type": "string", "description": "The symbol name (or, by default, name prefix) to find the definition of" },
                "exact":  { "type": "boolean", "description": "When true, match `name` exactly instead of as a prefix; defaults to prefix matching for discovery" },
                "kind":   { "type": "string", "description": "Optional symbol-kind filter, e.g. `function`, `struct`, `class`, `method`; omit to match any kind" }
            },
            "required": ["name"]
        }))
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("`name` is required"))?;
        let exact = args.get("exact").and_then(Value::as_bool).unwrap_or(false);
        let kind = args.get("kind").and_then(Value::as_str);
        let index = index_of(ctx);
        index.ensure_fresh().await?;

        let hits = index.symbol_find(name, exact, kind)?;
        if hits.is_empty() {
            return Ok(ToolOutput::text(format!("No symbol matches `{name}`.")));
        }
        let mut writer = BudgetedWriter::new(STRUCT_TOKEN_CAP);
        for s in &hits {
            let parent = s
                .parent
                .as_deref()
                .map(|p| format!("{p}."))
                .unwrap_or_default();
            let line = format!("{}:{} {} {parent}{}", s.path, s.line, s.kind, s.name);
            if !writer.writeln(&line) {
                break;
            }
        }
        Ok(finish(
            writer,
            "\n... [truncated; narrow with `exact` or `kind`]\n",
        ))
    }
}

// ---- word ------------------------------------------------------------------

pub struct WordTool;

#[async_trait]
impl Tool for WordTool {
    fn name(&self) -> &str {
        "word"
    }
    fn description(&self) -> &str {
        "List files and lines where an identifier token appears, from the index"
    }
    fn defensive_description(&self) -> Option<String> {
        Some(
            "Find every place an identifier TOKEN appears across the codebase — all uses, not \
             just the definition — and get the file + line of each. Use this to trace where a \
             function/variable/type is called or referenced before you change it. It matches \
             whole identifier tokens from the index (not arbitrary substrings or regex); for a \
             general-text or regex search use `search`, and to find only the definition use \
             `symbol_find`. Set `case_insensitive` to ignore case."
                .to_string(),
        )
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "token":            { "type": "string", "description": "Identifier token to look up" },
                "case_insensitive": { "type": "boolean", "description": "Case-insensitive match toggle" }
            },
            "required": ["token"]
        })
    }
    fn defensive_parameters(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "token":            { "type": "string", "description": "The exact identifier token to find uses of; matched as a whole word, not a substring" },
                "case_insensitive": { "type": "boolean", "description": "When true, match the token regardless of letter case; defaults to case-sensitive" }
            },
            "required": ["token"]
        }))
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let token = args
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("`token` is required"))?;
        let ci = args
            .get("case_insensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let index = index_of(ctx);
        index.ensure_fresh().await?;

        let grouped = index.word_hits(token, ci)?;
        if grouped.is_empty() {
            return Ok(ToolOutput::text(format!(
                "`{token}` not found in the index."
            )));
        }
        let mut writer = BudgetedWriter::new(STRUCT_TOKEN_CAP);
        for (path, lines) in &grouped {
            let joined = lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join(",");
            if !writer.writeln(&format!("{path}: {joined}")) {
                break;
            }
        }
        Ok(finish(writer, "\n... [truncated]\n"))
    }
}

// ---- deps ------------------------------------------------------------------

pub struct DepsTool;

#[async_trait]
impl Tool for DepsTool {
    fn name(&self) -> &str {
        "deps"
    }
    fn description(&self) -> &str {
        "Show a file's resolved import dependencies forward/reverse within a hop limit"
    }
    fn defensive_description(&self) -> Option<String> {
        Some(
            "See how one file connects to the rest of the codebase through imports: `forward` = \
             the files it depends on, `reverse` = the files that depend on it, `both` = both \
             directions. Use `reverse` to find everything you might break before changing a \
             file, and `forward` to learn what a file relies on. `hops` walks the graph that \
             many levels deep (1 = direct neighbours only). Imports are resolved through \
             cockpit's index, so this is more accurate than grepping for import lines."
                .to_string(),
        )
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string", "x-cockpit-kind": "path", "description": "File whose dependencies to walk" },
                "direction": { "type": "string", "description": "forward, reverse, or both (default both)" },
                "hops":      { "type": "integer", "description": "Max hops, 1-10 (default 1)" }
            },
            "required": ["path"]
        })
    }
    fn defensive_parameters(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string", "x-cockpit-kind": "path", "description": "Path to the file whose import dependency graph to walk, relative to the project root or absolute" },
                "direction": { "type": "string", "description": "Which way to walk: `forward` (files this one imports), `reverse` (files that import this one), or `both`; defaults to `both`" },
                "hops":      { "type": "integer", "description": "How many levels deep to follow the graph, 1-10; defaults to 1 (direct neighbours only)" }
            },
            "required": ["path"]
        }))
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path_arg = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("`path` is required"))?;
        let rel = rel_path(path_arg, ctx);
        let direction = args
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("both");
        let hops = args
            .get("hops")
            .and_then(Value::as_u64)
            .map(|h| h.clamp(1, 10) as usize)
            .unwrap_or(1);
        let index = index_of(ctx);
        index.ensure_fresh().await?;

        let edges = index.dep_edges()?;
        // forward: importer → importee; reverse: importee → importer.
        let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut unresolved: Vec<&DepEdge> = Vec::new();
        for e in &edges {
            match &e.importee {
                Some(imp) => {
                    forward.entry(&e.importer).or_default().push(imp);
                    reverse.entry(imp).or_default().push(&e.importer);
                }
                None if e.importer == rel => unresolved.push(e),
                None => {}
            }
        }

        let mut writer = BudgetedWriter::new(STRUCT_TOKEN_CAP);
        writer.writeln(&format!("deps for {rel} (hops={hops})"));

        if direction == "forward" || direction == "both" {
            let reached = bfs(&forward, &rel, hops);
            writer.writeln(&format!("forward ({}):", reached.len()));
            for (dist, p) in &reached {
                if !writer.writeln(&format!("  [{dist}] {p}")) {
                    return Ok(finish(writer, "\n... [truncated]\n"));
                }
            }
        }
        if direction == "reverse" || direction == "both" {
            let reached = bfs(&reverse, &rel, hops);
            writer.writeln(&format!("reverse ({}):", reached.len()));
            for (dist, p) in &reached {
                if !writer.writeln(&format!("  [{dist}] {p}")) {
                    return Ok(finish(writer, "\n... [truncated]\n"));
                }
            }
        }
        if !unresolved.is_empty() {
            writer.writeln(&format!("unresolved imports ({}):", unresolved.len()));
            for e in &unresolved {
                if !writer.writeln(&format!("  {}: {}", e.line, e.raw_target)) {
                    break;
                }
            }
        }
        Ok(finish(writer, "\n... [truncated]\n"))
    }
}

/// Shortest-distance BFS over an adjacency map, capped at `max_hops`.
/// Returns `(distance, node)` pairs (excludes the start node), sorted by
/// distance then path.
fn bfs<'a>(
    adj: &HashMap<&'a str, Vec<&'a str>>,
    start: &str,
    max_hops: usize,
) -> Vec<(usize, String)> {
    let mut dist: HashMap<&str, usize> = HashMap::new();
    let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    // Seed from the start node's own key (must match a &str inside adj).
    let start_key = adj.keys().find(|k| **k == start).copied();
    if let Some(sk) = start_key {
        dist.insert(sk, 0);
        queue.push_back(sk);
    } else {
        // Start has no outgoing edges in this map; still allow reverse
        // lookups by treating `start` as present with distance 0.
        return Vec::new();
    }
    while let Some(node) = queue.pop_front() {
        let d = dist[node];
        if d >= max_hops {
            continue;
        }
        if let Some(neighbors) = adj.get(node) {
            for &n in neighbors {
                if !dist.contains_key(n) {
                    dist.insert(n, d + 1);
                    queue.push_back(n);
                }
            }
        }
    }
    let mut out: Vec<(usize, String)> = dist
        .into_iter()
        .filter(|(_, d)| *d > 0)
        .map(|(p, d)| (d, p.to_string()))
        .collect();
    out.sort();
    out
}

// ---- hot -------------------------------------------------------------------

pub struct HotTool;

#[async_trait]
impl Tool for HotTool {
    fn name(&self) -> &str {
        "hot"
    }
    fn description(&self) -> &str {
        "List the most recently modified tracked files by mtime"
    }
    fn defensive_description(&self) -> Option<String> {
        Some(
            "List the files that were edited most recently, newest first, by modification time. \
             Use this to orient on a task quickly — recently-touched files are usually where the \
             active work is — or to find what changed last. `limit` caps how many to return. \
             This is a ranking by recency, not a snapshot of any one file."
                .to_string(),
        )
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max files (default 20)" }
            }
        })
    }
    fn defensive_parameters(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Maximum number of recently-modified files to return; defaults to 20" }
            }
        }))
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|l| l.clamp(1, 500) as usize)
            .unwrap_or(20);
        // Pure-FS: no index. Gitignore walk, sort by mtime desc.
        let root = &ctx.session.project_root;
        let mut files: Vec<(std::time::SystemTime, String, u64)> = Vec::new();
        let mut walker = WalkBuilder::new(root);
        walker
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .parents(true)
            .require_git(false)
            .follow_links(false);
        for dent in walker.build().flatten() {
            if !dent.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let abs = dent.path();
            let Ok(rel) = abs.strip_prefix(root) else {
                continue;
            };
            if let Ok(meta) = std::fs::metadata(abs)
                && let Ok(mtime) = meta.modified()
            {
                files.push((mtime, rel.to_string_lossy().replace('\\', "/"), meta.len()));
            }
        }
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        files.truncate(limit);
        if files.is_empty() {
            return Ok(ToolOutput::text("No tracked files.".to_string()));
        }
        let mut writer = BudgetedWriter::new(STRUCT_TOKEN_CAP);
        for (_, rel, size) in &files {
            if !writer.writeln(&format!("{rel}  {size}b")) {
                break;
            }
        }
        Ok(finish(writer, "\n... [truncated; lower `limit`]\n"))
    }
}

// ---- circular --------------------------------------------------------------

pub struct CircularTool;

#[async_trait]
impl Tool for CircularTool {
    fn name(&self) -> &str {
        "circular"
    }
    fn description(&self) -> &str {
        "Detect import cycles via strongly-connected components of the dependency graph"
    }
    fn defensive_description(&self) -> Option<String> {
        Some(
            "Find import cycles in the codebase: groups of files that depend on each other \
             directly or transitively. Use this when you suspect a circular-dependency problem, \
             or before a refactor that moves code between modules, to see which files are \
             tangled together. Takes no arguments — it analyses the whole project dependency \
             graph and reports each cycle it finds."
                .to_string(),
        )
    }
    fn parameters(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn defensive_parameters(&self) -> Option<Value> {
        Some(serde_json::json!({ "type": "object", "properties": {} }))
    }

    async fn call(&self, _args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let index = index_of(ctx);
        index.ensure_fresh().await?;
        let edges = index.dep_edges()?;

        // Build the resolved graph (importee NOT NULL).
        let mut nodes: Vec<String> = Vec::new();
        let mut idx: HashMap<String, usize> = HashMap::new();
        let mut adj: Vec<Vec<usize>> = Vec::new();
        let mut seen_edges: HashSet<(usize, usize)> = HashSet::new();
        for e in &edges {
            if let Some(importee) = &e.importee {
                let a = intern(&e.importer, &mut nodes, &mut idx, &mut adj);
                let b = intern(importee, &mut nodes, &mut idx, &mut adj);
                if seen_edges.insert((a, b)) {
                    adj[a].push(b);
                }
            }
        }

        let sccs = tarjan_scc(&adj);
        // Keep cycles only: SCC size > 1, or a self-loop.
        let mut cycles: Vec<Vec<usize>> = Vec::new();
        for comp in sccs {
            if comp.len() > 1 {
                cycles.push(comp);
            } else if comp.len() == 1 {
                let n = comp[0];
                if adj[n].contains(&n) {
                    cycles.push(comp);
                }
            }
        }
        if cycles.is_empty() {
            return Ok(ToolOutput::text("No import cycles found.".to_string()));
        }
        let mut writer = BudgetedWriter::new(STRUCT_TOKEN_CAP);
        writer.writeln(&format!("{} cycle(s):", cycles.len()));
        for comp in &cycles {
            let mut names: Vec<&str> = comp.iter().map(|&i| nodes[i].as_str()).collect();
            names.sort();
            let mut chain = names.clone();
            chain.push(names[0]);
            if !writer.writeln(&format!("  {}", chain.join(" -> "))) {
                break;
            }
        }
        Ok(finish(writer, "\n... [truncated]\n"))
    }
}

/// Intern a node name into the (nodes, index, adjacency) tables,
/// returning its dense index.
fn intern(
    name: &str,
    nodes: &mut Vec<String>,
    idx: &mut HashMap<String, usize>,
    adj: &mut Vec<Vec<usize>>,
) -> usize {
    if let Some(&i) = idx.get(name) {
        return i;
    }
    let i = nodes.len();
    nodes.push(name.to_string());
    idx.insert(name.to_string(), i);
    adj.push(Vec::new());
    i
}

/// Iterative Tarjan strongly-connected-components over an adjacency
/// list. Returns one Vec of node indices per SCC. No `petgraph`.
fn tarjan_scc(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut index_counter = 0usize;
    let mut indices = vec![usize::MAX; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut result: Vec<Vec<usize>> = Vec::new();

    // Explicit work stack: (node, next-child-cursor).
    for start in 0..n {
        if indices[start] != usize::MAX {
            continue;
        }
        let mut work: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(v, ci)) = work.last() {
            if ci == 0 {
                indices[v] = index_counter;
                lowlink[v] = index_counter;
                index_counter += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if ci < adj[v].len() {
                let w = adj[v][ci];
                // Advance the cursor for v.
                work.last_mut().unwrap().1 += 1;
                if indices[w] == usize::MAX {
                    work.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(indices[w]);
                }
            } else {
                // Done with v's children: propagate lowlink to parent and
                // pop an SCC root.
                if lowlink[v] == indices[v] {
                    let mut comp = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    result.push(comp);
                }
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
            }
        }
    }
    result
}

// ---- search ----------------------------------------------------------------

pub struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Budgeted structured regex search across the repo (ripgrep-backed)"
    }
    fn defensive_description(&self) -> Option<String> {
        Some(
            "Search the repository's text for a regular expression and get back matching \
             file:line locations, ripgrep-backed and budget-capped so the result stays small. \
             This is the general-purpose search: use it for arbitrary text, comments, strings, \
             or patterns that aren't a single identifier. When you're looking specifically for \
             where a symbol is DEFINED use `symbol_find`, and for whole-token USES use `word` — \
             those are more precise. Narrow the search with `path`/`glob` and add `context` \
             lines to see surrounding code. Prefer this over `bash` + raw `rg` so the output \
             stays budgeted."
                .to_string(),
        )
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern":          { "type": "string", "description": "Regex to search for" },
                "path":             { "type": "string", "x-cockpit-kind": "path", "description": "Path filter relative to project root" },
                "ignore_case":      { "type": "boolean", "description": "Case-insensitive match toggle" },
                "context":          { "type": "integer", "description": "Context lines around each match" },
                "glob":             { "type": "string", "description": "Glob include filter (e.g. *.rs)" }
            },
            "required": ["pattern"]
        })
    }
    fn defensive_parameters(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern":          { "type": "string", "description": "The regular expression to search for across file contents" },
                "path":             { "type": "string", "x-cockpit-kind": "path", "description": "Optional path to restrict the search to, relative to the project root; omit to search the whole repo" },
                "ignore_case":      { "type": "boolean", "description": "When true, match case-insensitively; defaults to case-sensitive" },
                "context":          { "type": "integer", "description": "Number of lines of surrounding context to include around each match; defaults to none" },
                "glob":             { "type": "string", "description": "Optional glob to include only matching files, e.g. `*.rs` or `src/**`" }
            },
            "required": ["pattern"]
        }))
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("`pattern` is required"))?;
        let path = args.get("path").and_then(Value::as_str);
        let ignore_case = args
            .get("ignore_case")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let context = args
            .get("context")
            .and_then(Value::as_u64)
            .map(|c| c.min(10));
        let glob = args.get("glob").and_then(Value::as_str);

        let root = ctx.session.project_root.clone();
        let search_dir = match path {
            Some(p) => crate::tools::common::resolve(p, &ctx.cwd),
            None => root.clone(),
        };
        // Native-tool boundary check (sandboxing part 2): a `path` filter
        // pointing outside cwd + session tmp must escalate before the
        // search reads any file contents there.
        crate::tools::sandbox::check_native_access(ctx, &search_dir).await?;
        let have_rg = which::which("rg").is_ok();
        let raw = run_search(have_rg, pattern, &search_dir, ignore_case, context, glob).await?;

        let body = if have_rg {
            format_rg_json(&raw, &root)
        } else {
            format_grep(&raw, &root)
        };
        if body.is_empty() {
            return Ok(ToolOutput::text(format!("No matches for `{pattern}`.")));
        }
        let mut writer = BudgetedWriter::new(SEARCH_TOKEN_CAP);
        for line in body.lines() {
            if !writer.writeln(line) {
                break;
            }
        }
        Ok(finish(
            writer,
            "\n... [truncated; narrow the query or add a `path`/`glob` filter]\n",
        ))
    }
}

/// Spawn `rg --json` (preferred) or `grep -rn` and return stdout.
async fn run_search(
    have_rg: bool,
    pattern: &str,
    dir: &Path,
    ignore_case: bool,
    context: Option<u64>,
    glob: Option<&str>,
) -> Result<String> {
    let mut cmd = if have_rg {
        let mut c = tokio::process::Command::new("rg");
        c.arg("--json")
            .arg("--line-number")
            .arg("--column")
            .arg("--no-heading")
            .arg("--color")
            .arg("never");
        if ignore_case {
            c.arg("--ignore-case");
        }
        if let Some(n) = context {
            c.arg("--context").arg(n.to_string());
        }
        if let Some(g) = glob {
            c.arg("--glob").arg(g);
        }
        c.arg("--").arg(pattern).arg(".");
        c
    } else {
        let mut c = tokio::process::Command::new("grep");
        c.arg("-rn");
        if ignore_case {
            c.arg("-i");
        }
        if let Some(n) = context {
            c.arg(format!("-C{n}"));
        }
        if let Some(g) = glob {
            c.arg(format!("--include={g}"));
        }
        c.arg("-e").arg(pattern).arg(".");
        c
    };
    cmd.current_dir(dir);
    let output = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawning search: {e}"))?;
    // rg/grep exit 1 means "no matches" — not an error.
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse rg's NDJSON stream into terse `path:line:col: text` records.
fn format_rg_json(stdout: &str, root: &Path) -> String {
    let mut out = String::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "match" | "context" => {
                let data = match v.get("data") {
                    Some(d) => d,
                    None => continue,
                };
                let path = data
                    .get("path")
                    .and_then(|p| p.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let line_no = data.get("line_number").and_then(Value::as_u64).unwrap_or(0);
                let text = data
                    .get("lines")
                    .and_then(|l| l.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim_end_matches('\n');
                let col = data
                    .get("submatches")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(|m| m.get("start"))
                    .and_then(Value::as_u64)
                    .map(|c| c + 1);
                let disp = display_path(path, root);
                let sep = if ty == "context" { "-" } else { ":" };
                match col {
                    Some(c) => out.push_str(&format!("{disp}:{line_no}:{c}{sep} {text}\n")),
                    None => out.push_str(&format!("{disp}:{line_no}{sep} {text}\n")),
                }
            }
            _ => {}
        }
    }
    out
}

/// `grep -rn` output is already `path:line:text`; just normalize paths.
fn format_grep(stdout: &str, root: &Path) -> String {
    let mut out = String::new();
    for line in stdout.lines() {
        if let Some((path, rest)) = line.split_once(':') {
            let disp = display_path(path, root);
            out.push_str(&format!("{disp}:{rest}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Make a path from search output relative + forward-slashed for display.
fn display_path(p: &str, root: &Path) -> String {
    let stripped = p.trim_start_matches("./");
    // rg/grep run with cwd=search_dir, so paths are already relative to
    // it; if `path` filter pointed below root, prepend nothing — the
    // model still gets a usable relative path. Absolute paths get
    // root-stripped.
    if let Ok(abs) = Path::new(p).strip_prefix(root) {
        abs.to_string_lossy().replace('\\', "/")
    } else {
        stripped.replace('\\', "/")
    }
}

// ---- shared FS helpers -----------------------------------------------------

/// Gitignore-aware list of `(rel, abs, size)` for every tracked file.
fn list_files(root: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut out = Vec::new();
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .follow_links(false);
    for dent in walker.build().flatten() {
        if !dent.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs = dent.path().to_path_buf();
        let Ok(rel) = abs.strip_prefix(root) else {
            continue;
        };
        let size = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
        out.push((rel.to_string_lossy().replace('\\', "/"), abs, size));
    }
    out
}

fn count_lines(abs: &Path) -> usize {
    match std::fs::read(abs) {
        Ok(b) if !b.contains(&0u8) => bytecount(&b),
        _ => 0,
    }
}

fn bytecount(b: &[u8]) -> usize {
    if b.is_empty() {
        return 0;
    }
    let nl = b.iter().filter(|&&c| c == b'\n').count();
    // Count a trailing partial line.
    if b.last() == Some(&b'\n') { nl } else { nl + 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::common::test_ctx;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    #[tokio::test]
    async fn outline_unknown_language_uses_regex_fallback_without_erroring() {
        let tmp = tempfile::tempdir().unwrap();
        // `.foo` is an unknown extension; give it def-like lines.
        write(
            tmp.path(),
            "weird.foo",
            "function alpha() {}\nclass Beta {}\n",
        );
        let ctx = test_ctx(tmp.path());
        let args = serde_json::json!({ "path": "weird.foo" });
        let out = OutlineTool.call(args, &ctx).await.unwrap();
        assert!(
            out.content.contains("unknown language"),
            "got: {}",
            out.content
        );
        assert!(out.content.contains("alpha"));
        assert!(out.content.contains("Beta"));
    }

    #[tokio::test]
    async fn tree_and_hot_list_unknown_language_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn k() {}\n");
        write(tmp.path(), "notes.foo", "anything\n");
        let ctx = test_ctx(tmp.path());

        let tree = TreeTool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(tree.content.contains("src/lib.rs"));
        assert!(tree.content.contains("notes.foo"));
        // The unknown file is visible but flagged not-indexed.
        assert!(tree.content.contains("notes.foo  unknown"));

        let hot = HotTool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(hot.content.contains("notes.foo"));
        assert!(hot.content.contains("src/lib.rs"));
    }

    #[tokio::test]
    async fn symbol_find_and_word_round_trip_through_call() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "m.rs",
            "pub fn target_fn() { let target_fn = 1; }\n",
        );
        let ctx = test_ctx(tmp.path());

        let sf = SymbolFindTool
            .call(
                serde_json::json!({ "name": "target_fn", "exact": true }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(sf.content.contains("m.rs"));
        assert!(sf.content.contains("target_fn"));

        let w = WordTool
            .call(serde_json::json!({ "token": "target_fn" }), &ctx)
            .await
            .unwrap();
        assert!(w.content.contains("m.rs"));
    }

    #[test]
    fn tarjan_finds_simple_cycle() {
        // 0 -> 1 -> 2 -> 0, and 3 isolated.
        let adj = vec![vec![1], vec![2], vec![0], vec![]];
        let sccs = tarjan_scc(&adj);
        let cyc: Vec<_> = sccs.iter().filter(|c| c.len() > 1).collect();
        assert_eq!(cyc.len(), 1);
        assert_eq!(cyc[0].len(), 3);
    }

    #[test]
    fn tarjan_no_cycle() {
        let adj = vec![vec![1], vec![2], vec![]];
        let sccs = tarjan_scc(&adj);
        assert!(sccs.iter().all(|c| c.len() == 1));
    }

    #[test]
    fn bfs_respects_hop_limit() {
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        adj.insert("a", vec!["b"]);
        adj.insert("b", vec!["c"]);
        adj.insert("c", vec!["d"]);
        let one = bfs(&adj, "a", 1);
        assert_eq!(one, vec![(1, "b".to_string())]);
        let two = bfs(&adj, "a", 2);
        assert_eq!(two, vec![(1, "b".to_string()), (2, "c".to_string())]);
    }

    #[test]
    fn bytecount_counts_lines() {
        assert_eq!(bytecount(b""), 0);
        assert_eq!(bytecount(b"a\n"), 1);
        assert_eq!(bytecount(b"a\nb"), 2);
        assert_eq!(bytecount(b"a\nb\n"), 2);
    }
}
