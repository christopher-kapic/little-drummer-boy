//! Codebase-intelligence index (GOALS §21, Phase 1).
//!
//! A tree-sitter-backed outline cache living in cockpit's SQLite DB
//! (`intel_*` tables, migration 0005). The single on-demand chokepoint
//! is [`Index::ensure_fresh`]: every index-backed tool calls it first,
//! it re-stats the gitignore-walked file set, drops removed files (FK
//! cascade purges their children), and re-indexes new/stale ones —
//! parallel parse via rayon, serial chunked write through one
//! connection. No file watcher (the §M5 decision); a watcher's
//! silent-staleness failure mode loses to priority #1.
//!
//! Invalidation is cheap: `mtime_ns + size` first, SHA-256 only as a
//! tiebreaker when those moved (tolerates a touched-but-identical file).

pub mod budget;
pub mod lang;
pub mod resolve;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use rayon::prelude::*;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::intel::lang::{Extraction, Language};

/// Files at/above this size are recorded in `tree` but skipped for
/// parsing (no symbols/imports) — large generated files blow parse time
/// for no navigational value.
const LARGE_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Parse + write are batched in chunks of this many files to bound peak
/// memory (the kcl-proven size).
const CHUNK: usize = 200;

/// When the to-index set reaches this size, emit a one-shot cold-index
/// log so the first call doesn't look hung (the TUI shows a spinner on
/// ToolStart; this is the Phase-1 progress signal).
const COLD_THRESHOLD: usize = 100;

/// Project-scoped intelligence index over `root`.
pub struct Index {
    db: Db,
    root: PathBuf,
    /// Absolute `root` as the string stored in the `root` column.
    root_key: String,
}

/// A file as found on disk during the freshness scan.
struct DiskFile {
    /// Relative, forward-slash path (the `path` column).
    rel: String,
    abs: PathBuf,
    language: Language,
    mtime_ns: i64,
    size: i64,
}

/// The parsed result for one file, ready for serial write.
struct ParsedFile {
    rel: String,
    language: Language,
    mtime_ns: i64,
    size: i64,
    content_hash: String,
    extraction: Extraction,
}

/// One symbol row for `outline` / `symbol_find`.
#[derive(Debug, Clone)]
pub struct SymbolRow {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub line: i64,
    pub end_line: i64,
    pub parent: Option<String>,
    pub visibility: Option<String>,
    pub signature: Option<String>,
}

/// Result of [`Index::outline_rows`]: a file's symbols, its `(target,
/// line)` imports, and its language label.
pub type OutlineData = (Vec<SymbolRow>, Vec<(String, i64)>, String);

/// A dependency edge for `deps` / `circular`.
#[derive(Debug, Clone)]
pub struct DepEdge {
    pub importer: String,
    pub importee: Option<String>,
    pub raw_target: String,
    pub line: i64,
}

impl Index {
    /// Build an index handle for `root`.
    pub fn new(db: Db, root: PathBuf) -> Self {
        let root_key = root.to_string_lossy().into_owned();
        Self { db, root, root_key }
    }

    /// The single on-demand chokepoint. Re-stats the gitignore-walked
    /// file set, deletes removed files in one tx (cascade purges
    /// children), then re-indexes new/stale files (parallel parse,
    /// serial chunked write). Runs entirely on a blocking thread.
    pub async fn ensure_fresh(&self) -> Result<()> {
        let root = self.root.clone();
        let root_key = self.root_key.clone();
        self.db
            .run_blocking(move |conn| ensure_fresh_blocking(conn, &root, &root_key))
            .await
    }

    // ---- query methods (each assumes ensure_fresh already ran) --------

    /// All known files for `tree`, ordered by path. `symbol_count` is a
    /// LEFT JOIN so indexed-empty (0) differs from not-indexed (None via
    /// the language/size heuristic at call sites — here every row in
    /// `intel_files` has been indexed, so count is always `Some`).
    pub fn tree_rows(&self) -> Result<Vec<(String, String, i64, i64)>> {
        let root_key = self.root_key.clone();
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT path, language, size, \
                 (SELECT COUNT(*) FROM intel_symbols s WHERE s.root = f.root AND s.path = f.path) \
                 FROM intel_files f WHERE root = ?1 ORDER BY path",
            )?;
            let rows = stmt
                .query_map([&root_key], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Symbols + imports for one file, ordered by line (for `outline`).
    pub fn outline_rows(&self, rel: &str) -> Result<OutlineData> {
        let root_key = self.root_key.clone();
        let rel_owned = rel.to_string();
        self.db.with_conn(|conn| {
            let language: Option<String> = conn
                .query_row(
                    "SELECT language FROM intel_files WHERE root = ?1 AND path = ?2",
                    rusqlite::params![root_key, rel_owned],
                    |r| r.get(0),
                )
                .ok();
            let symbols = query_symbols(
                conn,
                &root_key,
                "SELECT path, name, kind, line, end_line, parent, visibility, signature \
                 FROM intel_symbols WHERE root = ?1 AND path = ?2 ORDER BY line",
                rusqlite::params![root_key, rel_owned],
            )?;
            let mut stmt = conn.prepare(
                "SELECT target, line FROM intel_imports WHERE root = ?1 AND path = ?2 ORDER BY line",
            )?;
            let imports = stmt
                .query_map(rusqlite::params![root_key, rel_owned], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((symbols, imports, language.unwrap_or_default()))
        })
    }

    /// Find symbols by name. `exact` toggles `=` vs prefix `LIKE`;
    /// optional `kind` filters by symbol kind.
    pub fn symbol_find(
        &self,
        name: &str,
        exact: bool,
        kind: Option<&str>,
    ) -> Result<Vec<SymbolRow>> {
        let root_key = self.root_key.clone();
        let name = name.to_string();
        let kind = kind.map(|s| s.to_string());
        self.db.with_conn(|conn| {
            let base = "SELECT path, name, kind, line, end_line, parent, visibility, signature \
                 FROM intel_symbols WHERE root = ?1 AND ";
            if exact {
                let sql = format!(
                    "{base} name = ?2 {} ORDER BY path, line",
                    kind_clause(&kind, 3)
                );
                let rows = run_symbol_query(conn, &sql, &root_key, &name, kind.as_deref())?;
                Ok(rows)
            } else {
                // Prefix match; escape LIKE metacharacters.
                let pattern = format!("{}%", escape_like(&name));
                let sql = format!(
                    "{base} name LIKE ?2 ESCAPE '\\' {} ORDER BY path, line",
                    kind_clause(&kind, 3)
                );
                let rows = run_symbol_query(conn, &sql, &root_key, &pattern, kind.as_deref())?;
                Ok(rows)
            }
        })
    }

    /// Identifier occurrences for `word`, grouped by file. `case_insensitive`
    /// matches with `COLLATE NOCASE`.
    pub fn word_hits(
        &self,
        token: &str,
        case_insensitive: bool,
    ) -> Result<Vec<(String, Vec<i64>)>> {
        let root_key = self.root_key.clone();
        let token = token.to_string();
        self.db.with_conn(|conn| {
            let sql = if case_insensitive {
                "SELECT path, line FROM intel_identifiers \
                 WHERE root = ?1 AND token = ?2 COLLATE NOCASE ORDER BY path, line"
            } else {
                "SELECT path, line FROM intel_identifiers \
                 WHERE root = ?1 AND token = ?2 ORDER BY path, line"
            };
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt
                .query_map(rusqlite::params![root_key, token], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut grouped: Vec<(String, Vec<i64>)> = Vec::new();
            for (path, line) in rows {
                match grouped.last_mut() {
                    Some((p, lines)) if *p == path => lines.push(line),
                    _ => grouped.push((path, vec![line])),
                }
            }
            Ok(grouped)
        })
    }

    /// All dependency edges for the project (`deps` / `circular`).
    pub fn dep_edges(&self) -> Result<Vec<DepEdge>> {
        let root_key = self.root_key.clone();
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT importer, importee, raw_target, line FROM intel_deps \
                 WHERE root = ?1 ORDER BY importer, line",
            )?;
            let rows = stmt
                .query_map([&root_key], |r| {
                    Ok(DepEdge {
                        importer: r.get(0)?,
                        importee: r.get(1)?,
                        raw_target: r.get(2)?,
                        line: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

fn kind_clause(kind: &Option<String>, idx: usize) -> String {
    if kind.is_some() {
        format!("AND kind = ?{idx}")
    } else {
        String::new()
    }
}

fn run_symbol_query(
    conn: &Connection,
    sql: &str,
    root_key: &str,
    name_or_pattern: &str,
    kind: Option<&str>,
) -> rusqlite::Result<Vec<SymbolRow>> {
    if let Some(k) = kind {
        query_symbols(
            conn,
            root_key,
            sql,
            rusqlite::params![root_key, name_or_pattern, k],
        )
    } else {
        query_symbols(
            conn,
            root_key,
            sql,
            rusqlite::params![root_key, name_or_pattern],
        )
    }
}

fn query_symbols(
    conn: &Connection,
    _root_key: &str,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<SymbolRow>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params, |r| {
            Ok(SymbolRow {
                path: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                line: r.get(3)?,
                end_line: r.get(4)?,
                parent: r.get(5)?,
                visibility: r.get(6)?,
                signature: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Escape `%`, `_` and `\` for a `LIKE … ESCAPE '\'` prefix match.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ---- the freshness chokepoint ---------------------------------------------

fn ensure_fresh_blocking(conn: &Connection, root: &Path, root_key: &str) -> Result<()> {
    // 1. Build the on-disk set via the gitignore-aware walk.
    let disk = scan_disk(root)?;
    let disk_paths: HashSet<String> = disk.iter().map(|d| d.rel.clone()).collect();

    // 2. Load current index state (path → (mtime_ns, size, hash)).
    let indexed = load_indexed(conn, root_key)?;

    // 3. Removed files: in the index but absent from disk → delete in
    //    ONE tx BEFORE the parse pass so the cascade purges their
    //    children. This is the deleted-file regression kcl hit.
    let removed: Vec<&String> = indexed
        .keys()
        .filter(|p| !disk_paths.contains(*p))
        .collect();
    if !removed.is_empty() {
        let tx = conn.unchecked_transaction()?;
        {
            let mut del = tx.prepare("DELETE FROM intel_files WHERE root = ?1 AND path = ?2")?;
            for path in &removed {
                del.execute(rusqlite::params![root_key, path])?;
            }
        }
        tx.commit()?;
    }

    // 4. Determine the to-index set: new files + stale files
    //    (mtime/size changed, confirmed by hash tiebreaker).
    let mut to_index: Vec<DiskFile> = Vec::new();
    for f in disk {
        match indexed.get(&f.rel) {
            None => to_index.push(f),
            Some((mtime, size, hash)) => {
                if *mtime == f.mtime_ns && *size == f.size {
                    continue; // fast-path: unchanged.
                }
                // Tiebreaker: hash the file; if it matches, refresh just
                // the stat columns (cheap) and skip re-parsing.
                match hash_file(&f.abs) {
                    Ok(h) if &h == hash => {
                        conn.execute(
                            "UPDATE intel_files SET mtime_ns = ?3, size = ?4 \
                             WHERE root = ?1 AND path = ?2",
                            rusqlite::params![root_key, f.rel, f.mtime_ns, f.size],
                        )?;
                    }
                    _ => to_index.push(f),
                }
            }
        }
    }

    if to_index.is_empty() {
        return Ok(());
    }
    if to_index.len() >= COLD_THRESHOLD {
        tracing::info!(files = to_index.len(), "intel: cold-indexing");
    }

    // Go module prefix (empty if no go.mod) for the resolver.
    let module_prefix = go_module_prefix(root);

    // 5. Parse in rayon chunks, write each chunk serially in one tx.
    let now = now_secs();
    for chunk in to_index.chunks(CHUNK) {
        let parsed: Vec<ParsedFile> = chunk
            .par_iter()
            .filter_map(|f| parse_one(f).ok().flatten())
            .collect();
        write_chunk(conn, root_key, &disk_paths, &module_prefix, &parsed, now)?;
    }
    Ok(())
}

/// Walk `root` gitignore-aware and stat every regular file.
fn scan_disk(root: &Path) -> Result<Vec<DiskFile>> {
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .follow_links(false);
    let mut out = Vec::new();
    for dent in walker.build().flatten() {
        if !dent.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs = dent.path().to_path_buf();
        let Ok(rel) = abs.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(DiskFile {
            rel,
            language: Language::from_path(&abs),
            mtime_ns: mtime_ns(&meta),
            size: meta.len() as i64,
            abs,
        });
    }
    Ok(out)
}

type IndexedMap = HashMap<String, (i64, i64, String)>;

fn load_indexed(conn: &Connection, root_key: &str) -> Result<IndexedMap> {
    let mut stmt =
        conn.prepare("SELECT path, mtime_ns, size, content_hash FROM intel_files WHERE root = ?1")?;
    let rows = stmt
        .query_map([root_key], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ),
            ))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

/// Read + parse one file off the executor (rayon worker). Returns
/// `Ok(None)` for binary files (skipped). Large files are still recorded
/// (`tree` visibility) but parsed to an empty extraction.
fn parse_one(f: &DiskFile) -> Result<Option<ParsedFile>> {
    let bytes = std::fs::read(&f.abs).with_context(|| format!("reading {}", f.abs.display()))?;
    // Binary files: skip entirely (no index row) — `tree` reads the FS
    // for those via the same gitignore walk in the tool, and `read`
    // already detects binaries.
    if looks_binary(&bytes) {
        return Ok(None);
    }
    let content_hash = hash_bytes(&bytes);
    let extraction = if f.size as u64 >= LARGE_FILE_BYTES {
        Extraction::default()
    } else {
        lang::extract(f.language, &bytes).unwrap_or_default()
    };
    Ok(Some(ParsedFile {
        rel: f.rel.clone(),
        language: f.language,
        mtime_ns: f.mtime_ns,
        size: f.size,
        content_hash,
        extraction,
    }))
}

/// Serial write of one parsed chunk in a single transaction. Replaces
/// each file's rows (delete-then-insert) so a re-index is idempotent;
/// the parent delete cascades children, then we re-insert everything.
fn write_chunk(
    conn: &Connection,
    root_key: &str,
    existing: &HashSet<String>,
    module_prefix: &str,
    parsed: &[ParsedFile],
    now: i64,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut del = tx.prepare("DELETE FROM intel_files WHERE root = ?1 AND path = ?2")?;
        let mut ins_file = tx.prepare(
            "INSERT INTO intel_files (root, path, language, mtime_ns, size, content_hash, indexed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let mut ins_sym = tx.prepare(
            "INSERT INTO intel_symbols (root, path, name, kind, line, end_line, parent, visibility, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        let mut ins_imp = tx.prepare(
            "INSERT INTO intel_imports (root, path, target, line) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut ins_id = tx.prepare(
            "INSERT INTO intel_identifiers (root, path, token, line) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut ins_dep = tx.prepare(
            "INSERT INTO intel_deps (root, importer, importee, raw_target, line) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut ins_call = tx.prepare(
            "INSERT INTO intel_callsites (root, caller_file, caller_line, caller_symbol, callee_name, callee_kind) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;

        for p in parsed {
            del.execute(rusqlite::params![root_key, p.rel])?;
            ins_file.execute(rusqlite::params![
                root_key,
                p.rel,
                p.language.as_str(),
                p.mtime_ns,
                p.size,
                p.content_hash,
                now
            ])?;
            for s in &p.extraction.symbols {
                ins_sym.execute(rusqlite::params![
                    root_key,
                    p.rel,
                    s.name,
                    s.kind,
                    s.line,
                    s.end_line,
                    s.parent,
                    s.visibility,
                    s.signature
                ])?;
            }
            for imp in &p.extraction.imports {
                ins_imp.execute(rusqlite::params![root_key, p.rel, imp.target, imp.line])?;
                let importee =
                    resolve::resolve(p.language, &p.rel, &imp.target, existing, module_prefix);
                ins_dep.execute(rusqlite::params![
                    root_key, p.rel, importee, imp.target, imp.line
                ])?;
            }
            for id in &p.extraction.identifiers {
                ins_id.execute(rusqlite::params![root_key, p.rel, id.token, id.line])?;
            }
            for cs in &p.extraction.callsites {
                ins_call.execute(rusqlite::params![
                    root_key,
                    p.rel,
                    cs.caller_line,
                    cs.caller_symbol,
                    cs.callee_name,
                    cs.callee_kind
                ])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

// ---- small helpers ---------------------------------------------------------

fn looks_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    head.contains(&0u8)
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(hash_bytes(&bytes))
}

/// Lowercase hex of a byte slice (no `hex` crate dependency).
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read the `module` line out of `go.mod` at the project root, if any.
fn go_module_prefix(root: &Path) -> String {
    let gomod = root.join("go.mod");
    let Ok(text) = std::fs::read_to_string(&gomod) else {
        return String::new();
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("module ") {
            return rest.trim().to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn count_rows(db: &Db, table: &str, root_key: &str, path: &str) -> i64 {
        db.with_conn(|conn| {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE root = ?1 AND path = ?2");
            Ok(conn.query_row(&sql, rusqlite::params![root_key, path], |r| r.get(0))?)
        })
        .unwrap()
    }

    #[tokio::test]
    async fn indexes_two_languages() {
        let db = Db::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_file(&root, "src/lib.rs", "pub struct Foo;\npub fn bar() {}\n");
        write_file(
            &root,
            "app.py",
            "def baz():\n    pass\nclass Qux:\n    pass\n",
        );

        let index = Index::new(db.clone(), root.clone());
        index.ensure_fresh().await.unwrap();

        let rust = index.symbol_find("Foo", true, None).unwrap();
        assert_eq!(rust.len(), 1, "expected Rust struct Foo");
        let py = index.symbol_find("Qux", true, None).unwrap();
        assert_eq!(py.len(), 1, "expected Python class Qux");

        let tree = index.tree_rows().unwrap();
        assert!(tree.iter().any(|(p, _, _, _)| p == "src/lib.rs"));
        assert!(tree.iter().any(|(p, _, _, _)| p == "app.py"));
    }

    #[tokio::test]
    async fn deleted_file_leaves_no_stale_rows() {
        let db = Db::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let root_key = root.to_string_lossy().into_owned();
        write_file(&root, "a.rs", "pub fn alpha() {}\n");
        write_file(&root, "b.rs", "pub fn beta() {}\n");

        let index = Index::new(db.clone(), root.clone());
        index.ensure_fresh().await.unwrap();
        assert_eq!(count_rows(&db, "intel_symbols", &root_key, "a.rs"), 1);

        // Edit a.rs (add a symbol) then DELETE b.rs.
        write_file(&root, "a.rs", "pub fn alpha() {}\npub fn alpha2() {}\n");
        std::fs::remove_file(root.join("b.rs")).unwrap();
        index.ensure_fresh().await.unwrap();

        // b.rs: no stale file or symbol rows.
        assert_eq!(count_rows(&db, "intel_files", &root_key, "b.rs"), 0);
        assert_eq!(count_rows(&db, "intel_symbols", &root_key, "b.rs"), 0);
        // a.rs: re-indexed to 2 symbols.
        assert_eq!(count_rows(&db, "intel_symbols", &root_key, "a.rs"), 2);
    }

    #[tokio::test]
    async fn unchanged_file_is_a_cache_hit() {
        let db = Db::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_file(&root, "x.rs", "pub fn x() {}\n");
        let index = Index::new(db.clone(), root.clone());
        index.ensure_fresh().await.unwrap();
        // Second pass with no changes must not error or duplicate rows.
        index.ensure_fresh().await.unwrap();
        let hits = index.symbol_find("x", true, None).unwrap();
        assert_eq!(hits.len(), 1);
    }
}
