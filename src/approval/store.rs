//! Approval-decision store (sandboxing part 1, §2).
//!
//! Records grants so a future access skips the prompt. Two grant kinds —
//! command-key (the §1 `argv[0]`+subcommand key) and path (an absolute
//! path or prefix, for part 2's native confinement) — across four
//! scopes:
//!
//! - [`Once`](Scope::Once) — never stored.
//! - [`Session`](Scope::Session) — session DB (`approval_grants`,
//!   migration 0011); survives for the session's lifetime.
//! - [`Project`](Scope::Project) — nearest project `.cockpit/`, in
//!   `approvals.json`; survives daemon restarts; applies to any session
//!   whose cwd resolves into the same project root.
//! - [`Global`](Scope::Global) — user-level cockpit config dir, in
//!   `approvals.json`; survives restarts; applies everywhere.
//!
//! Persistence honors cockpit's existing config discovery
//! ([`crate::config::dirs`], [`crate::git::find_worktree_root`]) — no new
//! location scheme. Project/Global are plain JSON files written
//! atomically (temp + rename); Session lives in SQLite.
//!
//! ## Wrappers are never persisted (priority #1)
//!
//! A wrapper/eval command (§1) carries dynamic behavior the classifier
//! can't bound, so [`record_command`] **rejects** any attempt to store
//! one at a non-`Once` scope with [`StoreError::WrapperNotPersistable`].
//! Wrappers re-prompt every run.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::approval::classify::ApprovalKey;
use crate::db::Db;

/// The four approval scopes the user chose. Ordered narrowest→widest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// This invocation only; never stored.
    Once,
    /// All invocations in the current session (session DB).
    Session,
    /// All sessions whose cwd resolves into this project (project
    /// `.cockpit/`).
    Project,
    /// All sessions in all projects (user-level config dir).
    Global,
}

/// A persisted loop-guard verdict for an exact call signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopVerdict {
    /// Always run the repeat without prompting.
    Accept,
    /// Always block the repeat (guidance error) without prompting.
    Reject,
}

impl LoopVerdict {
    fn as_str(self) -> &'static str {
        match self {
            LoopVerdict::Accept => "accept",
            LoopVerdict::Reject => "reject",
        }
    }
}

/// What kind of thing a grant covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKind {
    /// A shell command, keyed by `argv[0]`+subcommand.
    Command,
    /// A filesystem path (absolute path or prefix).
    Path,
}

impl GrantKind {
    fn as_str(self) -> &'static str {
        match self {
            GrantKind::Command => "command",
            GrantKind::Path => "path",
        }
    }
}

/// Errors the store surfaces to callers.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Attempted to persist a wrapper/eval command at a non-`Once` scope.
    /// Wrappers can only ever be approved `Once` (§2, priority #1).
    #[error("wrapper command `{0}` cannot be remembered; only one-time approval is allowed")]
    WrapperNotPersistable(String),
    /// `Scope::Once` was passed to a record call. `Once` is never stored;
    /// the caller should simply not record it.
    #[error("`Once` scope is never persisted")]
    OnceNotPersistable,
    /// No project root could be resolved for a `Project`-scope grant
    /// (the cwd isn't inside a git worktree).
    #[error("no project root for the current directory; cannot store a project grant")]
    NoProjectRoot,
    /// An I/O / serialization failure while reading or writing a grant.
    #[error(transparent)]
    Io(#[from] anyhow::Error),
}

/// On-disk shape of a project/global `approvals.json`. Sorted sets keep
/// the file stable (no spurious diffs) and dedup automatically.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ApprovalsFile {
    /// Command-key grants, as storage strings (`"gh pr"`, `"ls"`).
    #[serde(default)]
    commands: BTreeSet<String>,
    /// Path grants, as absolute path / prefix strings.
    #[serde(default)]
    paths: BTreeSet<String>,
    /// Loop-guard always-accept rules, keyed by call signature (a hash of
    /// tool name + canonical `wire_input`; see [`GrantStore::loop_signature`]).
    /// A signature here auto-accepts a back-to-back repeat of that exact
    /// call without re-prompting.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    loop_accept: BTreeSet<String>,
    /// Loop-guard always-reject rules, keyed by the same call signature.
    /// A signature here auto-rejects the repeat with the guidance error.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    loop_reject: BTreeSet<String>,
}

/// The grant store. Holds the session DB handle (for Session scope) and
/// the resolved cwd + project root + global config dir (for Project /
/// Global scope). Cheap to build per query; the DB handle is an `Arc`
/// clone.
pub struct GrantStore {
    db: Db,
    session_id: uuid::Uuid,
    /// Resolved project root for the session cwd, if any. `Project`-scope
    /// reads and writes target `<root>/.cockpit/approvals.json`.
    project_root: Option<PathBuf>,
    /// User-level cockpit config dir for `Global`-scope grants. Resolved
    /// once; `None` only if no home/data dir can be located.
    global_dir: Option<PathBuf>,
}

impl GrantStore {
    /// Build a store for a session at `cwd`. Resolves the project root
    /// (via [`crate::git::find_worktree_root`], the same resolution the
    /// rest of the app uses) and the global config dir up front. `cwd` is
    /// consumed for that resolution; the absolutization part 2 needs is
    /// done against the `ToolCtx` cwd at the call site, so the store
    /// doesn't retain it.
    pub fn new(db: Db, session_id: uuid::Uuid, cwd: PathBuf) -> Self {
        let project_root = crate::git::find_worktree_root(&cwd);
        let global_dir = global_approvals_dir();
        Self {
            db,
            session_id,
            project_root,
            global_dir,
        }
    }

    /// Whether a command key is already granted at *any* scope that
    /// applies to this session (Session, Project, or Global). `Once`
    /// grants are never stored, so they never show up here.
    pub fn is_command_granted(&self, key: &ApprovalKey) -> bool {
        let s = key.as_storage_str();
        self.session_has(GrantKind::Command, &s)
            || self.project_file().is_some_and(|f| f.commands.contains(&s))
            || self.global_file().is_some_and(|f| f.commands.contains(&s))
    }

    /// Whether a path is already granted. A grant covers the path itself
    /// and anything under it (prefix match) — a directory grant covers
    /// its descendants, the natural confinement semantics part 2 wants.
    pub fn is_path_granted(&self, path: &Path) -> bool {
        let candidate = normalize_path(path);
        let matches = |stored: &str| path_covers(stored, &candidate);
        self.session_path_granted(&candidate, matches)
            || self
                .project_file()
                .is_some_and(|f| f.paths.iter().any(|p| matches(p)))
            || self
                .global_file()
                .is_some_and(|f| f.paths.iter().any(|p| matches(p)))
    }

    /// Record a command-key grant at `scope`. Rejects wrappers at any
    /// non-`Once` scope (priority #1). `Once` is a no-op error — the
    /// caller shouldn't record it, but rejecting loudly catches misuse.
    pub fn record_command(
        &self,
        info: &crate::approval::classify::SimpleCommandInfo,
        scope: Scope,
    ) -> Result<(), StoreError> {
        if scope == Scope::Once {
            return Err(StoreError::OnceNotPersistable);
        }
        if info.wrapper {
            return Err(StoreError::WrapperNotPersistable(info.key.as_storage_str()));
        }
        self.record(GrantKind::Command, &info.key.as_storage_str(), scope)
    }

    /// Record a path grant at `scope`. Paths are never wrappers, so the
    /// only rejection is `Once`. The path is normalized (absolutized
    /// against cwd) before storage so later prefix checks are stable.
    pub fn record_path(&self, path: &Path, scope: Scope) -> Result<(), StoreError> {
        if scope == Scope::Once {
            return Err(StoreError::OnceNotPersistable);
        }
        self.record(GrantKind::Path, &normalize_path(path), scope)
    }

    // ---- loop-guard rules -------------------------------------------------

    /// Stable signature for a loop-guard rule: a hash of the tool name and
    /// the call's canonical `wire_input`. Two calls share a signature iff
    /// the tool name and the (serialized) wire input are byte-identical —
    /// the exact-match semantics the loop guard requires. Hashing bounds
    /// the storage key regardless of input size.
    ///
    /// The `wire_input` is serialized with [`canonical_json`] so that
    /// object key ordering can't make two semantically-identical inputs
    /// hash differently (serde_json preserves insertion order; the model
    /// may emit keys in any order).
    pub fn loop_signature(tool: &str, wire_input: &serde_json::Value) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(tool.as_bytes());
        h.update([0u8]); // separator so `tool` + `input` can't collide across a boundary
        h.update(canonical_json(wire_input).as_bytes());
        let out = h.finalize();
        let mut hex = String::with_capacity(64);
        for byte in out.iter() {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    }

    /// The recorded verdict for `signature`, or `None` if no rule applies.
    ///
    /// ## Precedence (session wins over project/global)
    ///
    /// A signature can carry rules at more than one scope (e.g. the user
    /// chose "always accept for this project" in an earlier session, then
    /// "always reject for this session" now). The **session** rule wins:
    /// it is the most recent, most specific expression of intent and is
    /// the only one the user can have set *in the current session*, so it
    /// must be able to override a standing project/global rule for the
    /// life of the session. Project and global are both persistent; among
    /// them, a project rule (nearer the work) wins over a global one.
    ///
    /// Order checked: session → project → global. Within a scope a
    /// `reject` and an `accept` cannot coexist (recording one clears the
    /// other), so the first scope with *any* rule decides.
    pub fn loop_rule(&self, signature: &str) -> Option<LoopVerdict> {
        if let Some(v) = self.session_loop_rule(signature) {
            return Some(v);
        }
        if let Some(v) = self
            .project_file()
            .and_then(|f| file_loop_rule(&f, signature))
        {
            return Some(v);
        }
        self.global_file()
            .and_then(|f| file_loop_rule(&f, signature))
    }

    /// Record a loop-guard rule for `signature` at `scope`. Recording one
    /// verdict at a scope clears the opposite verdict at the same scope so
    /// a signature never carries contradictory rules within one scope.
    /// `Once` is rejected (it is never persisted — the caller acts on a
    /// one-off decision directly).
    pub fn record_loop_rule(
        &self,
        signature: &str,
        verdict: LoopVerdict,
        scope: Scope,
    ) -> Result<(), StoreError> {
        match scope {
            Scope::Once => Err(StoreError::OnceNotPersistable),
            Scope::Session => self
                .session_record_loop_rule(signature, verdict)
                .map_err(StoreError::Io),
            Scope::Project => {
                let root = self
                    .project_root
                    .as_ref()
                    .ok_or(StoreError::NoProjectRoot)?;
                let dir = root.join(".cockpit");
                self.file_record_loop_rule(&dir, signature, verdict)
                    .map_err(StoreError::Io)
            }
            Scope::Global => {
                let dir = self
                    .global_dir
                    .clone()
                    .context("no global config dir available")
                    .map_err(StoreError::Io)?;
                self.file_record_loop_rule(&dir, signature, verdict)
                    .map_err(StoreError::Io)
            }
        }
    }

    fn session_loop_rule(&self, signature: &str) -> Option<LoopVerdict> {
        self.db
            .with_conn(|conn| {
                let verdict: Option<String> = conn
                    .query_row(
                        "SELECT rule_verdict FROM loop_guard_rules \
                         WHERE session_id = ?1 AND signature = ?2",
                        rusqlite::params![self.session_id.to_string(), signature],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(verdict)
            })
            .ok()
            .flatten()
            .and_then(|s| parse_verdict(&s))
    }

    fn session_record_loop_rule(&self, signature: &str, verdict: LoopVerdict) -> Result<()> {
        self.db.with_conn(|conn| {
            // `INSERT OR REPLACE` on the (session_id, signature) primary
            // key flips an existing opposite verdict in place — no
            // contradictory pair can persist.
            conn.execute(
                "INSERT OR REPLACE INTO loop_guard_rules (session_id, signature, rule_verdict) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![self.session_id.to_string(), signature, verdict.as_str()],
            )
            .context("inserting loop_guard_rule")?;
            Ok(())
        })
    }

    fn file_record_loop_rule(
        &self,
        dir: &Path,
        signature: &str,
        verdict: LoopVerdict,
    ) -> Result<()> {
        let mut file = load_approvals(dir).unwrap_or_default();
        // Clear the opposite verdict so the file never carries a
        // contradictory pair for one signature.
        match verdict {
            LoopVerdict::Accept => {
                file.loop_reject.remove(signature);
                file.loop_accept.insert(signature.to_string());
            }
            LoopVerdict::Reject => {
                file.loop_accept.remove(signature);
                file.loop_reject.insert(signature.to_string());
            }
        }
        store_approvals(dir, &file)
    }

    // ---- internals --------------------------------------------------------

    fn record(&self, kind: GrantKind, key: &str, scope: Scope) -> Result<(), StoreError> {
        match scope {
            Scope::Once => Err(StoreError::OnceNotPersistable),
            Scope::Session => self.session_insert(kind, key).map_err(StoreError::Io),
            Scope::Project => {
                let root = self
                    .project_root
                    .as_ref()
                    .ok_or(StoreError::NoProjectRoot)?;
                let dir = root.join(".cockpit");
                self.file_insert(&dir, kind, key).map_err(StoreError::Io)
            }
            Scope::Global => {
                let dir = self
                    .global_dir
                    .clone()
                    .context("no global config dir available")
                    .map_err(StoreError::Io)?;
                self.file_insert(&dir, kind, key).map_err(StoreError::Io)
            }
        }
    }

    // ---- session scope (SQLite) ------------------------------------------

    fn session_has(&self, kind: GrantKind, key: &str) -> bool {
        self.db
            .with_conn(|conn| {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM approval_grants \
                     WHERE session_id = ?1 AND grant_kind = ?2 AND grant_key = ?3",
                    rusqlite::params![self.session_id.to_string(), kind.as_str(), key],
                    |row| row.get(0),
                )?;
                Ok(n > 0)
            })
            .unwrap_or(false)
    }

    /// Path grants need prefix matching, so we read all session path
    /// grants and test each. (The set is tiny — one session's manual
    /// approvals — so a full scan is cheaper than clever SQL.)
    fn session_path_granted(&self, _candidate: &str, matches: impl Fn(&str) -> bool) -> bool {
        self.db
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT grant_key FROM approval_grants \
                     WHERE session_id = ?1 AND grant_kind = 'path'",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![self.session_id.to_string()], |row| {
                        row.get::<_, String>(0)
                    })?;
                for key in rows {
                    if matches(&key?) {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .unwrap_or(false)
    }

    fn session_insert(&self, kind: GrantKind, key: &str) -> Result<()> {
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO approval_grants (session_id, grant_kind, grant_key) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![self.session_id.to_string(), kind.as_str(), key],
            )
            .context("inserting session approval grant")?;
            Ok(())
        })
    }

    // ---- project / global scope (JSON files) ------------------------------

    fn project_file(&self) -> Option<ApprovalsFile> {
        let root = self.project_root.as_ref()?;
        load_approvals(&root.join(".cockpit"))
    }

    fn global_file(&self) -> Option<ApprovalsFile> {
        let dir = self.global_dir.as_ref()?;
        load_approvals(dir)
    }

    fn file_insert(&self, dir: &Path, kind: GrantKind, key: &str) -> Result<()> {
        let mut file = load_approvals(dir).unwrap_or_default();
        let set = match kind {
            GrantKind::Command => &mut file.commands,
            GrantKind::Path => &mut file.paths,
        };
        set.insert(key.to_string());
        store_approvals(dir, &file)
    }
}

/// `<global config dir>` for approvals. We prefer `~/.config/cockpit`
/// (XDG-canonical), the same home-scoped layer config discovery treats
/// as the user-level config root.
fn global_approvals_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".config/cockpit"))
}

/// File name for the per-scope approvals store inside a `.cockpit/` dir.
const APPROVALS_FILE: &str = "approvals.json";

fn load_approvals(dir: &Path) -> Option<ApprovalsFile> {
    let path = dir.join(APPROVALS_FILE);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write `file` to `<dir>/approvals.json` atomically (temp + rename) so a
/// crash mid-write can't corrupt the store. Creates `dir` if needed.
fn store_approvals(dir: &Path, file: &ApprovalsFile) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(APPROVALS_FILE);
    let tmp = dir.join(format!("{APPROVALS_FILE}.tmp"));
    let json = serde_json::to_vec_pretty(file).context("serializing approvals")?;
    std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Absolutize + lexically normalize a path to a stable storage string.
/// We don't canonicalize (the path may not exist yet — part 2 grants
/// access before creation), but we do resolve `.`/`..` lexically and
/// join relative paths onto the current dir so prefix checks are sound.
fn normalize_path(path: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    };
    lexical_normalize(&abs).to_string_lossy().into_owned()
}

/// Resolve `.` and `..` components lexically without touching the
/// filesystem. A leading `..` (path escaping root) is kept as-is.
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether a stored path grant `stored` covers `candidate`: equal, or
/// `candidate` is a descendant of `stored` (prefix match on path
/// components, not raw string prefix — so `/a/bc` is not covered by
/// `/a/b`).
fn path_covers(stored: &str, candidate: &str) -> bool {
    let stored = Path::new(stored);
    let candidate = Path::new(candidate);
    candidate == stored || candidate.starts_with(stored)
}

/// Parse a stored verdict string. An unrecognized value (corrupt row /
/// hand-edited file) reads as `None` — no rule applies, so the guard
/// falls back to prompting, the safe default.
fn parse_verdict(s: &str) -> Option<LoopVerdict> {
    match s {
        "accept" => Some(LoopVerdict::Accept),
        "reject" => Some(LoopVerdict::Reject),
        _ => None,
    }
}

/// Loop-guard verdict for `signature` from a loaded approvals file.
/// `reject` is checked first so a hand-edited file that somehow lists a
/// signature in both sets resolves to the safe (blocking) verdict.
fn file_loop_rule(file: &ApprovalsFile, signature: &str) -> Option<LoopVerdict> {
    if file.loop_reject.contains(signature) {
        Some(LoopVerdict::Reject)
    } else if file.loop_accept.contains(signature) {
        Some(LoopVerdict::Accept)
    } else {
        None
    }
}

/// Serialize a JSON value with object keys sorted recursively, so two
/// semantically-identical inputs that differ only in key order produce
/// the same string (and thus the same loop signature).
fn canonical_json(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // The key itself is JSON-escaped via serde so embedded
                // quotes/control chars can't break the framing.
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push(':');
                out.push_str(&canonical_json(&map[*k]));
            }
            out.push('}');
            out
        }
        Value::Array(items) => {
            let mut out = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_json(item));
            }
            out.push(']');
            out
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::classify::SimpleCommandInfo;

    fn cmd_info(program: &str, sub: Option<&str>, wrapper: bool) -> SimpleCommandInfo {
        let key = ApprovalKey {
            program: program.to_string(),
            subcommand: sub.map(str::to_string),
        };
        SimpleCommandInfo {
            program: program.to_string(),
            subcommand: sub.map(str::to_string),
            key,
            wrapper,
        }
    }

    /// Build a store backed by an in-memory DB, with project root + global
    /// dir pointed at temp dirs so file scopes are exercised hermetically.
    fn test_store(project: &Path, global: PathBuf) -> (GrantStore, uuid::Uuid) {
        let db = Db::open_in_memory().unwrap();
        let session =
            crate::session::Session::create(db.clone(), project.to_path_buf(), "coder").unwrap();
        let sid = session.id;
        let mut store = GrantStore::new(db, sid, project.to_path_buf());
        // Force deterministic scopes regardless of the test host's git
        // state: the temp project IS the root, global is a temp dir.
        store.project_root = Some(project.to_path_buf());
        store.global_dir = Some(global);
        (store, sid)
    }

    #[test]
    fn session_grant_then_granted() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let info = cmd_info("gh", Some("pr"), false);
        assert!(!store.is_command_granted(&info.key));
        store.record_command(&info, Scope::Session).unwrap();
        assert!(store.is_command_granted(&info.key));
        // A different subcommand still prompts.
        let other = cmd_info("gh", Some("repo"), false);
        assert!(!store.is_command_granted(&other.key));
    }

    #[test]
    fn project_grant_covers_subcommand_args_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, sid) = test_store(tmp.path(), global.path().to_path_buf());
        let info = cmd_info("gh", Some("pr"), false);
        store.record_command(&info, Scope::Project).unwrap();

        // `gh pr create ...` derives the same key → granted, no prompt.
        let create = cmd_info("gh", Some("pr"), false);
        assert!(store.is_command_granted(&create.key));
        // `gh repo ...` is a different key → still prompts.
        let repo = cmd_info("gh", Some("repo"), false);
        assert!(!store.is_command_granted(&repo.key));

        // Survives reload: a fresh store over the same DB + dirs sees it.
        let db2 = store.db.clone();
        let mut reloaded = GrantStore::new(db2, sid, tmp.path().to_path_buf());
        reloaded.project_root = Some(tmp.path().to_path_buf());
        reloaded.global_dir = Some(global.path().to_path_buf());
        assert!(reloaded.is_command_granted(&info.key));
    }

    #[test]
    fn global_grant_persists_and_applies() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let info = cmd_info("cargo", Some("build"), false);
        store.record_command(&info, Scope::Global).unwrap();

        // A *different* project (different root) still sees the global
        // grant, because global applies everywhere.
        let other_project = tempfile::tempdir().unwrap();
        let db2 = store.db.clone();
        let mut elsewhere =
            GrantStore::new(db2, store.session_id, other_project.path().to_path_buf());
        elsewhere.project_root = Some(other_project.path().to_path_buf());
        elsewhere.global_dir = Some(global.path().to_path_buf());
        assert!(elsewhere.is_command_granted(&info.key));
    }

    #[test]
    fn wrapper_rejected_at_every_non_once_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let wrapper = cmd_info("bash", None, true);
        for scope in [Scope::Session, Scope::Project, Scope::Global] {
            let err = store.record_command(&wrapper, scope).unwrap_err();
            assert!(
                matches!(err, StoreError::WrapperNotPersistable(_)),
                "scope {scope:?} should reject wrapper, got {err:?}"
            );
        }
        // And nothing was written.
        assert!(!store.is_command_granted(&wrapper.key));
    }

    #[test]
    fn once_scope_is_never_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let info = cmd_info("ls", None, false);
        assert!(matches!(
            store.record_command(&info, Scope::Once),
            Err(StoreError::OnceNotPersistable)
        ));
        assert!(!store.is_command_granted(&info.key));
    }

    #[test]
    fn path_grant_prefix_match() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let dir = tmp.path().join("src");
        store.record_path(&dir, Scope::Project).unwrap();
        // A file under the granted dir is covered.
        assert!(store.is_path_granted(&dir.join("main.rs")));
        // A sibling that shares a string prefix but not a path prefix is
        // NOT covered.
        let sibling = tmp.path().join("src-gen").join("x.rs");
        assert!(!store.is_path_granted(&sibling));
    }

    #[test]
    fn path_grant_session_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let file = tmp.path().join("a/b/c.txt");
        assert!(!store.is_path_granted(&file));
        store.record_path(&file, Scope::Session).unwrap();
        assert!(store.is_path_granted(&file));
    }

    #[test]
    fn unparseable_or_empty_keys_are_just_not_granted() {
        // The store only answers about keys it's given; an empty/garbage
        // command never produces a key, so the classifier returns no
        // simple commands and the store is never asked → not granted.
        // (Classifier-side behavior is tested in classify.rs.) Here we
        // assert the store treats an unknown key as not-granted.
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let unknown = ApprovalKey {
            program: "nevergranted".into(),
            subcommand: None,
        };
        assert!(!store.is_command_granted(&unknown));
    }

    // ---- loop-guard rules ------------------------------------------------

    #[test]
    fn loop_signature_keys_on_tool_and_wire_input() {
        use serde_json::json;
        // Same tool + identical input → identical signature.
        let a = GrantStore::loop_signature("read", &json!({"path": "src/main.rs"}));
        let b = GrantStore::loop_signature("read", &json!({"path": "src/main.rs"}));
        assert_eq!(a, b);
        // A different tool with the same input → different signature.
        let c = GrantStore::loop_signature("bash", &json!({"path": "src/main.rs"}));
        assert_ne!(a, c);
        // A different input under the same tool → different signature.
        let d = GrantStore::loop_signature("read", &json!({"path": "src/lib.rs"}));
        assert_ne!(a, d);
    }

    #[test]
    fn loop_signature_is_object_key_order_independent() {
        use serde_json::json;
        // The model may emit object keys in any order; semantically
        // identical inputs must share a signature.
        let a = GrantStore::loop_signature("edit", &json!({"path": "a", "old": "x", "new": "y"}));
        let b = GrantStore::loop_signature("edit", &json!({"new": "y", "path": "a", "old": "x"}));
        assert_eq!(a, b);
    }

    #[test]
    fn loop_rule_session_record_and_read_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let sig = GrantStore::loop_signature("read", &serde_json::json!({"path": "x"}));
        assert!(store.loop_rule(&sig).is_none());
        store
            .record_loop_rule(&sig, LoopVerdict::Reject, Scope::Session)
            .unwrap();
        assert_eq!(store.loop_rule(&sig), Some(LoopVerdict::Reject));
        // Recording the opposite verdict at the same scope flips it (no
        // contradictory pair persists).
        store
            .record_loop_rule(&sig, LoopVerdict::Accept, Scope::Session)
            .unwrap();
        assert_eq!(store.loop_rule(&sig), Some(LoopVerdict::Accept));
    }

    #[test]
    fn loop_rule_project_persists_across_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, sid) = test_store(tmp.path(), global.path().to_path_buf());
        let sig = GrantStore::loop_signature("bash", &serde_json::json!({"command": "ls"}));
        store
            .record_loop_rule(&sig, LoopVerdict::Accept, Scope::Project)
            .unwrap();
        // A fresh store over the same project dir (a later session) reads
        // the persisted project rule back.
        let db2 = store.db.clone();
        let mut reloaded = GrantStore::new(db2, sid, tmp.path().to_path_buf());
        reloaded.project_root = Some(tmp.path().to_path_buf());
        reloaded.global_dir = Some(global.path().to_path_buf());
        assert_eq!(reloaded.loop_rule(&sig), Some(LoopVerdict::Accept));
    }

    #[test]
    fn loop_rule_session_takes_precedence_over_project() {
        // A session rule and a project rule for the SAME signature resolve
        // to the session verdict (documented precedence: session > project
        // > global).
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let sig = GrantStore::loop_signature("read", &serde_json::json!({"path": "z"}));
        store
            .record_loop_rule(&sig, LoopVerdict::Accept, Scope::Project)
            .unwrap();
        store
            .record_loop_rule(&sig, LoopVerdict::Reject, Scope::Session)
            .unwrap();
        // Session (reject) wins over project (accept).
        assert_eq!(store.loop_rule(&sig), Some(LoopVerdict::Reject));
    }

    #[test]
    fn loop_rule_project_takes_precedence_over_global() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let sig = GrantStore::loop_signature("read", &serde_json::json!({"path": "q"}));
        store
            .record_loop_rule(&sig, LoopVerdict::Reject, Scope::Global)
            .unwrap();
        store
            .record_loop_rule(&sig, LoopVerdict::Accept, Scope::Project)
            .unwrap();
        // Project (accept) wins over global (reject).
        assert_eq!(store.loop_rule(&sig), Some(LoopVerdict::Accept));
    }

    #[test]
    fn loop_rule_once_scope_is_never_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let sig = GrantStore::loop_signature("read", &serde_json::json!({"path": "x"}));
        assert!(matches!(
            store.record_loop_rule(&sig, LoopVerdict::Accept, Scope::Once),
            Err(StoreError::OnceNotPersistable)
        ));
        assert!(store.loop_rule(&sig).is_none());
    }

    #[test]
    fn loop_rule_keys_on_exact_signature_not_tool_name() {
        // A rule for one call must NOT cover a different call of the same
        // tool with different args.
        let tmp = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let (store, _) = test_store(tmp.path(), global.path().to_path_buf());
        let sig_a = GrantStore::loop_signature("read", &serde_json::json!({"path": "a"}));
        let sig_b = GrantStore::loop_signature("read", &serde_json::json!({"path": "b"}));
        store
            .record_loop_rule(&sig_a, LoopVerdict::Accept, Scope::Session)
            .unwrap();
        assert_eq!(store.loop_rule(&sig_a), Some(LoopVerdict::Accept));
        assert!(store.loop_rule(&sig_b).is_none());
    }
}
