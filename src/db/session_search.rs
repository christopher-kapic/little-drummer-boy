//! Cross-session full-text recall query layer (`session_search` /
//! `session_read`, prompt `search-old-sessions.md`).
//!
//! Backed by the `session_fts` FTS5 virtual table (migration 0013). The
//! engine is BM25 ranking with a `last_active_at` recency tiebreaker; no
//! embeddings in v1. The candidate-pool seam ([`search_candidates`]
//! returns more rows than the caller's display budget) is where a future
//! embedding ranker would re-rank without changing either tool's schema.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::db::Db;

/// One FTS5 hit, resolved back to its thread + in-thread location. The
/// snippet is FTS5's `snippet()` excerpt with matched terms wrapped in
/// the highlight delimiters.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub session_id: Uuid,
    pub short_id: Option<String>,
    pub title: Option<String>,
    /// `last_active_at`, epoch seconds — the human-date source + recency
    /// tiebreaker.
    pub last_active_at: i64,
    /// Best snippet for this thread (matched terms highlighted).
    pub snippet: String,
    /// BM25 relevance (lower = more relevant, FTS5 convention). Kept on
    /// the hit so a future re-ranker can blend it with other signals.
    pub bm25: f64,
}

/// A message turn read back from a thread (`session_read`).
#[derive(Debug, Clone)]
pub struct ThreadTurn {
    pub seq: i64,
    /// `user` or `assistant`.
    pub role: String,
    pub text: String,
}

impl Db {
    /// One-off probe that the bundled SQLite actually has FTS5 compiled
    /// in. Creates a throwaway in-`temp` FTS5 table and selects against
    /// it. Returns `Ok(())` when FTS5 is usable; an explanatory error
    /// otherwise. The feature must never silently degrade to LIKE
    /// (prompt decision), so callers surface this and stop.
    pub fn fts5_available(&self) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute_batch(
                "CREATE VIRTUAL TABLE temp.__cockpit_fts5_probe USING fts5(x);
                 INSERT INTO temp.__cockpit_fts5_probe (x) VALUES ('cockpit');
                 DROP TABLE temp.__cockpit_fts5_probe;",
            )
            .context(
                "FTS5 is not available in this SQLite build; \
                 session_search/session_read require it and there is no LIKE fallback",
            )?;
            Ok(())
        })
    }

    /// Rank FTS5 candidates for `query`, one row per matching thread
    /// (the best-ranking snippet per session). Ordered by BM25 relevance
    /// then `last_active_at` recency. This is the candidate pool: callers
    /// pass a `pool` larger than their display budget so a later ranking
    /// pass (today identity; a future embedding re-ranker tomorrow) has
    /// room to reorder.
    ///
    /// Scope rules:
    ///   * `project_id = Some(p)` confines to that project; `None` is
    ///     global recall across every project.
    ///   * `exclude_session` drops the current live thread.
    ///   * archived threads (`archived_at IS NOT NULL`) are always
    ///     excluded — search never surfaces a soft-deleted thread.
    ///   * `since` (epoch seconds) keeps only threads active at/after it.
    pub fn search_candidates(
        &self,
        query: &str,
        project_id: Option<&str>,
        exclude_session: Option<Uuid>,
        since: Option<i64>,
        pool: u32,
    ) -> Result<Vec<SearchHit>> {
        self.with_conn(|conn| {
            search_candidates_inner(conn, query, project_id, exclude_session, since, pool)
        })
    }

    /// All `user_message` / `assistant_message` turns of a thread,
    /// ordered by `seq` (oldest first). Powers `session_read`'s
    /// windowing — the tool slices this in Rust per the `read`-tool
    /// pagination conventions. Non-message events are skipped.
    pub fn thread_turns(&self, session_id: Uuid) -> Result<Vec<ThreadTurn>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT seq, type, json_extract(data_json, '$.text') AS text
                       FROM session_events
                      WHERE session_id = ?1
                        AND type IN ('user_message', 'assistant_message')
                      ORDER BY seq ASC",
                )
                .context("preparing thread_turns")?;
            let rows = stmt
                .query_map([session_id.to_string()], |row| {
                    let kind: String = row.get("type")?;
                    let role = match kind.as_str() {
                        "assistant_message" => "assistant",
                        _ => "user",
                    }
                    .to_string();
                    let text: Option<String> = row.get("text")?;
                    Ok(ThreadTurn {
                        seq: row.get("seq")?,
                        role,
                        text: text.unwrap_or_default(),
                    })
                })
                .context("querying thread_turns")?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.context("decoding thread turn")?);
            }
            Ok(out)
        })
    }

    /// `seq`s within a thread whose message text matches `query` (FTS5),
    /// oldest first. `session_read` centers its window on these. Empty
    /// when the thread has no textual match.
    pub fn thread_match_seqs(&self, session_id: Uuid, query: &str) -> Result<Vec<i64>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT seq FROM session_fts
                      WHERE session_fts MATCH ?1
                        AND row_kind = 'message'
                        AND session_id = ?2
                      ORDER BY seq ASC",
                )
                .context("preparing thread_match_seqs")?;
            let rows = stmt
                .query_map(params![query, session_id.to_string()], |row| {
                    row.get::<_, i64>("seq")
                })
                .context("querying thread_match_seqs")?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.context("decoding match seq")?);
            }
            Ok(out)
        })
    }
}

fn search_candidates_inner(
    conn: &Connection,
    query: &str,
    project_id: Option<&str>,
    exclude_session: Option<Uuid>,
    since: Option<i64>,
    pool: u32,
) -> Result<Vec<SearchHit>> {
    // Pull every matching FTS row joined to its session, ranked by BM25.
    // We over-fetch (rows, not threads) and collapse to one hit per
    // thread in Rust, keeping each thread's best-ranking snippet. The
    // SQL filters scope/archive/current/recency up front so the row set
    // stays small. `snippet(session_fts, 3, …)`: column 3 is `body`
    // (the only indexed column); ~16 tokens ≈ the ~150-char budget; the
    // `…` ellipsis marks elided text on either side.
    let mut stmt = conn
        .prepare(
            "SELECT f.session_id AS session_id,
                    s.short_id    AS short_id,
                    s.title       AS title,
                    s.last_active_at AS last_active_at,
                    snippet(session_fts, 3, '[', ']', '…', 16) AS snip,
                    bm25(session_fts) AS rank
               FROM session_fts AS f
               JOIN sessions    AS s ON s.session_id = f.session_id
              WHERE session_fts MATCH ?1
                AND s.archived_at IS NULL
                AND (?2 IS NULL OR s.project_id = ?2)
                AND (?3 IS NULL OR s.session_id <> ?3)
                AND (?4 IS NULL OR s.last_active_at >= ?4)
              ORDER BY rank ASC, s.last_active_at DESC",
        )
        .context("preparing search_candidates")?;

    let exclude = exclude_session.map(|u| u.to_string());
    let rows = stmt
        .query_map(params![query, project_id, exclude, since], |row| {
            let sid: String = row.get("session_id")?;
            Ok((
                sid,
                row.get::<_, Option<String>>("short_id")?,
                row.get::<_, Option<String>>("title")?,
                row.get::<_, i64>("last_active_at")?,
                row.get::<_, String>("snip")?,
                row.get::<_, f64>("rank")?,
            ))
        })
        .context("querying search_candidates")?;

    // Collapse to one hit per thread, keeping the first (best-ranking)
    // snippet seen — the rows arrive in BM25-then-recency order, so the
    // first occurrence of a session is already its strongest hit.
    let mut order: Vec<Uuid> = Vec::new();
    let mut by_session: std::collections::HashMap<Uuid, SearchHit> =
        std::collections::HashMap::new();
    for r in rows {
        let (sid, short_id, title, last_active_at, snippet, bm25) =
            r.context("decoding search hit")?;
        let session_id = Uuid::parse_str(&sid).with_context(|| format!("session_id `{sid}`"))?;
        if by_session.contains_key(&session_id) {
            continue;
        }
        order.push(session_id);
        by_session.insert(
            session_id,
            SearchHit {
                session_id,
                short_id,
                title,
                last_active_at,
                snippet,
                bm25,
            },
        );
        if order.len() as u32 >= pool {
            break;
        }
    }

    Ok(rank_candidates(
        order
            .into_iter()
            .map(|id| by_session.remove(&id).unwrap())
            .collect(),
    ))
}

/// Final ranking pass over the FTS candidate pool. **Seam for a future
/// embedding re-ranker** (prompt: "leave a clean seam where a future
/// embedding ranker could re-rank FTS candidates"). Today the score is
/// the raw FTS5 BM25 relevance (`hit.bm25`, lower = better), so this is
/// the SQL order made explicit; a re-ranker swaps the key for a blended
/// semantic score without touching either tool's schema or the DB query
/// surface. The sort is stable, so the SQL `last_active_at` recency
/// tiebreaker survives ties.
fn rank_candidates(mut candidates: Vec<SearchHit>) -> Vec<SearchHit> {
    candidates.sort_by(|a, b| {
        a.bm25
            .partial_cmp(&b.bm25)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::session_log::SessionEventKind;
    use serde_json::json;

    /// Insert a message event and return its seq.
    fn msg(db: &Db, session_id: Uuid, kind: SessionEventKind, text: &str) -> i64 {
        db.insert_session_event(session_id, kind, None, None, &json!({ "text": text }))
            .unwrap()
    }

    #[test]
    fn fts5_is_available_in_bundled_build() {
        let db = Db::open_in_memory().unwrap();
        db.fts5_available()
            .expect("bundled rusqlite must ship FTS5");
    }

    #[test]
    fn search_ranks_and_scopes_by_project() {
        let db = Db::open_in_memory().unwrap();
        let a = db.create_session("projA", "/a", "Build").unwrap();
        let b = db.create_session("projB", "/b", "Build").unwrap();
        msg(
            &db,
            a.session_id,
            SessionEventKind::UserMessage,
            "let us discuss widget calibration",
        );
        msg(
            &db,
            b.session_id,
            SessionEventKind::UserMessage,
            "totally unrelated gardening notes",
        );

        // Default scope = projA only.
        let hits = db
            .search_candidates("widget", Some("projA"), None, None, 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, a.session_id);
        assert!(
            hits[0].snippet.contains('['),
            "snippet must highlight: {}",
            hits[0].snippet
        );

        // projB has no widget match.
        let none = db
            .search_candidates("widget", Some("projB"), None, None, 10)
            .unwrap();
        assert!(none.is_empty());

        // Global recall still finds it.
        let global = db
            .search_candidates("widget", None, None, None, 10)
            .unwrap();
        assert_eq!(global.len(), 1);
    }

    #[test]
    fn search_excludes_archived_and_current_session() {
        let db = Db::open_in_memory().unwrap();
        let live = db.create_session("p", "/x", "Build").unwrap();
        let archived = db.create_session("p", "/x", "Build").unwrap();
        let current = db.create_session("p", "/x", "Build").unwrap();
        for s in [&live, &archived, &current] {
            msg(
                &db,
                s.session_id,
                SessionEventKind::UserMessage,
                "shared keyword apricot",
            );
        }
        db.archive_session(archived.session_id, false).unwrap();

        let hits = db
            .search_candidates("apricot", Some("p"), Some(current.session_id), None, 10)
            .unwrap();
        let ids: Vec<Uuid> = hits.iter().map(|h| h.session_id).collect();
        assert!(ids.contains(&live.session_id));
        assert!(
            !ids.contains(&archived.session_id),
            "archived must be excluded"
        );
        assert!(
            !ids.contains(&current.session_id),
            "current must be excluded"
        );
    }

    #[test]
    fn search_indexes_titles() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "Build").unwrap();
        db.set_auto_title(s.session_id, "refactor the lock manager")
            .unwrap();
        let hits = db
            .search_candidates("refactor", Some("p"), None, None, 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, s.session_id);
    }

    #[test]
    fn search_honors_since_filter() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "Build").unwrap();
        msg(
            &db,
            s.session_id,
            SessionEventKind::UserMessage,
            "banana split recipe",
        );
        let active = db
            .get_session(s.session_id)
            .unwrap()
            .unwrap()
            .last_active_at;
        // since in the future → filtered out.
        let later = db
            .search_candidates("banana", Some("p"), None, Some(active + 10_000), 10)
            .unwrap();
        assert!(later.is_empty());
        // since in the past → included.
        let earlier = db
            .search_candidates("banana", Some("p"), None, Some(active - 10_000), 10)
            .unwrap();
        assert_eq!(earlier.len(), 1);
    }

    #[test]
    fn no_match_is_empty_not_error() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "Build").unwrap();
        msg(
            &db,
            s.session_id,
            SessionEventKind::UserMessage,
            "hello world",
        );
        let hits = db
            .search_candidates("nonexistentterm", Some("p"), None, None, 10)
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn backfill_indexes_preexisting_rows() {
        // Simulate pre-migration data: insert events with the FTS triggers
        // dropped, then re-run the backfill statements and confirm the
        // rows become searchable. We mimic this by inserting directly with
        // triggers in place (the live path) AND verifying a row inserted
        // before any search is found — the migration's backfill is what
        // makes Db::open_in_memory()'s already-applied schema index the
        // create_session title path; message backfill is covered by the
        // trigger path. To exercise the literal backfill SQL, insert an
        // event row by hand bypassing nothing (triggers fire) — then drop
        // and rebuild the FTS table from the backfill statements.
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "Build").unwrap();
        msg(
            &db,
            s.session_id,
            SessionEventKind::AssistantMessage,
            "the quokka is a marsupial",
        );

        // Drop the FTS contents and re-run the backfill to prove the
        // backfill SQL (not just the triggers) reconstructs the index.
        db.with_conn(|conn| {
            conn.execute_batch("DELETE FROM session_fts;")?;
            conn.execute_batch(
                "INSERT INTO session_fts (row_kind, session_id, seq, body)
                 SELECT 'message', session_id, seq, json_extract(data_json, '$.text')
                 FROM session_events
                 WHERE type IN ('user_message','assistant_message')
                   AND json_extract(data_json, '$.text') IS NOT NULL;",
            )?;
            Ok(())
        })
        .unwrap();

        let hits = db
            .search_candidates("quokka", Some("p"), None, None, 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, s.session_id);
    }

    #[test]
    fn thread_turns_and_match_seqs() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "Build").unwrap();
        let s1 = msg(
            &db,
            s.session_id,
            SessionEventKind::UserMessage,
            "what is a kestrel",
        );
        let _s2 = msg(
            &db,
            s.session_id,
            SessionEventKind::AssistantMessage,
            "a small falcon",
        );
        let s3 = msg(
            &db,
            s.session_id,
            SessionEventKind::UserMessage,
            "and the kestrel diet",
        );

        let turns = db.thread_turns(s.session_id).unwrap();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].role, "assistant");

        let seqs = db.thread_match_seqs(s.session_id, "kestrel").unwrap();
        assert_eq!(seqs, vec![s1, s3]);
    }
}
