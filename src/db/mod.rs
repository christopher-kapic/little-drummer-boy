//! SQLite persistence layer.
//!
//! Single connection, wrapped in `Arc<Mutex<>>`. Reads and writes are
//! cheap enough (point lookups, single-row inserts) to hold the lock
//! synchronously from tokio tasks; the multi-threaded runtime keeps
//! other tasks moving while one is in a critical section. Aggregate
//! queries that scan many rows go through [`Db::run_blocking`] so the
//! executor thread isn't pinned.
//!
//! Layout:
//!
//! - [`migrate`] — schema versioning over `schema_version`. Forward-only.
//! - [`sessions`] — session CRUD.
//! - [`tool_calls`] — `tool_call_events` writes + history reads.
//! - [`inference_calls`] — token / cost rows (GOALS §15b).
//! - [`locks`] — crash-recovery mirror of the in-memory `LockManager`.
//! - [`needs_attention`] — interrupt queue (GOALS §3b).
//! - [`lang`] — file-extension → language attribution (§15c).
//!
//! Database path: `~/.local/share/cockpit/cockpit.db`
//! (XDG-canonical via [`crate::config::resolve::cockpit_data_dir`]).

pub mod inference_calls;
pub mod lang;
pub mod locks;
pub mod needs_attention;
pub mod sessions;
pub mod tokenizer_calibration;
pub mod tool_calls;
pub mod usage_events;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Wrapper around a single `rusqlite::Connection`. Cheap to clone
/// (everything is behind `Arc<Mutex<>>`).
#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<Connection>>,
    /// `None` for in-memory databases (tests).
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Db {
    /// Open the canonical cockpit database, creating parent directories
    /// as needed. Runs every pending migration before returning.
    pub fn open_default() -> Result<Self> {
        let dir = crate::config::resolve::cockpit_data_dir()?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Self::open(&dir.join("cockpit.db"))
    }

    /// Open a database at an arbitrary path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        apply_connection_pragmas(&conn, true)
            .with_context(|| format!("setting pragmas on {}", path.display()))?;
        let db = Self {
            inner: Arc::new(Mutex::new(conn)),
            path: Some(path.to_path_buf()),
        };
        db.migrate()?;
        Ok(db)
    }

    /// In-memory database. Used by tests; not exposed for production
    /// because every restart would lose state.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory sqlite")?;
        apply_connection_pragmas(&conn, false).context("setting pragmas on in-memory db")?;
        let db = Self {
            inner: Arc::new(Mutex::new(conn)),
            path: None,
        };
        db.migrate()?;
        Ok(db)
    }

    /// File path the database is backed by, or `None` for in-memory.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Run an idempotent closure against the connection synchronously.
    /// Holds the connection lock for the duration of `f`. Use for cheap
    /// queries (single-row reads, inserts, schema metadata).
    pub fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let guard = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db mutex poisoned"))?;
        f(&guard)
    }

    /// Async variant that runs the closure on a blocking thread. Use for
    /// queries that scan many rows (`/stats`, exports). The connection
    /// lock is still per-call so writes serialize correctly.
    pub async fn run_blocking<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .map_err(|_| anyhow::anyhow!("db mutex poisoned"))?;
            f(&guard)
        })
        .await
        .context("db worker thread joined")?
    }

    /// Apply every pending migration. Forward-only; downgrades are not
    /// supported. Each migration runs in its own transaction so a
    /// failure halfway through leaves the previous version intact.
    fn migrate(&self) -> Result<()> {
        self.with_conn(|conn| migrate(conn))
    }
}

/// Configure per-connection PRAGMAs. Called once at connection open.
///
/// - `foreign_keys = ON`: SQLite-default-off; we rely on the
///   CASCADE relationships in 0001_initial.sql.
/// - `journal_mode = WAL` (file DBs only): durable + better
///   concurrent-reader story. WAL doesn't apply to in-memory DBs
///   (SQLite ignores it).
///
/// These can't live in migration SQL because `journal_mode = WAL`
/// fails when invoked inside a transaction, and migrations run inside
/// a `BEGIN; ... COMMIT;` block for atomic apply.
fn apply_connection_pragmas(conn: &Connection, on_disk: bool) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .context("enabling foreign_keys")?;
    if on_disk {
        // `pragma_update` doesn't accept the kind of literal that
        // `journal_mode = WAL` needs; the query-row form does. The
        // return value is the resolved mode — we don't use it but a
        // non-`wal` result on a file DB would mean WAL is unavailable
        // (older SQLite, exotic FS), which is fine to silently fall
        // back to.
        let _: String = conn
            .query_row("PRAGMA journal_mode = WAL;", [], |row| row.get(0))
            .context("enabling WAL")?;
    }
    Ok(())
}

// ---- migration runner ------------------------------------------------------

/// All schema migrations, in order. Adding one: append `include_str!`
/// for the new file and bump nothing else — the index in this slice
/// is the version number.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_initial.sql"),
    include_str!("migrations/0002_sessions_fork.sql"),
    include_str!("migrations/0003_usage_events.sql"),
    include_str!("migrations/0004_tokenizer_calibration.sql"),
];

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);")
        .context("creating schema_version table")?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .context("reading current schema version")?;

    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = (i as i64) + 1;
        if version <= current {
            continue;
        }
        // `execute_batch` doesn't open a transaction by itself; wrap so
        // a half-applied migration rolls back cleanly.
        conn.execute_batch("BEGIN;")
            .with_context(|| format!("opening transaction for migration {version}"))?;
        let apply = (|| -> Result<()> {
            conn.execute_batch(sql)
                .with_context(|| format!("applying migration {version}"))?;
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [version],
            )
            .with_context(|| format!("recording migration {version}"))?;
            conn.execute_batch("COMMIT;")
                .with_context(|| format!("committing migration {version}"))?;
            Ok(())
        })();
        if let Err(e) = apply {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_idempotent() {
        let db = Db::open_in_memory().unwrap();
        // Second migrate call is a no-op.
        db.with_conn(|conn| migrate(conn)).unwrap();
        let v: i64 = db
            .with_conn(|conn| {
                Ok(
                    conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }

    #[test]
    fn essential_tables_exist() {
        let db = Db::open_in_memory().unwrap();
        for table in [
            "sessions",
            "tool_call_events",
            "inference_calls",
            "lock_state",
            "lock_reads",
            "needs_attention",
        ] {
            let count: i64 = db
                .with_conn(|conn| {
                    Ok(conn.query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                        [table],
                        |row| row.get(0),
                    )?)
                })
                .unwrap();
            assert_eq!(count, 1, "table `{table}` missing");
        }
        // And the view.
        let view_count: i64 = db
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name='tool_call_stats'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(view_count, 1);
    }
}
