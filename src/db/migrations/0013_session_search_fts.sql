-- 0013_session_search_fts.sql — cross-session full-text recall
-- (`session_search` / `session_read`, prompt `search-old-sessions.md`).
--
-- A single FTS5 virtual table indexes the *searchable* surface of every
-- session: the session TITLE plus the text of `user_message` /
-- `assistant_message` events. Tool outputs, tool-call args, and raw
-- inference payloads are deliberately NOT indexed — they're noise for
-- recall and a token/privacy hazard.
--
-- Layout choice: a contentless-style FTS5 table (`content=''`) with one
-- indexed text column plus UNINDEXED mapping columns. We do NOT use the
-- `content=<table>` external-content mode because the searchable text is
-- spread across two base tables (sessions.title + session_events.data_json)
-- and lives inside a JSON blob in the events case — there is no single
-- column FTS5 could shadow. Carrying our own UNINDEXED columns lets every
-- hit resolve back to a thread (`session_id`) and, for message rows, an
-- in-thread location (`seq`); title rows store `seq = NULL`.
--
--   row_kind   — 'title' | 'message'. Distinguishes a title hit from a
--                message hit so `session_read` can window correctly.
--   session_id — the owning session (UUID text). Always present.
--   seq        — session_events.seq for a message row; NULL for a title.
--   body       — the indexed text (title text, or the message's
--                data_json `text` field).
--
-- Sync is trigger-driven (see below) AND backfilled here for all
-- pre-existing rows, so old sessions are searchable immediately after
-- migration — not just events created afterward.

CREATE VIRTUAL TABLE session_fts USING fts5(
    row_kind   UNINDEXED,
    session_id UNINDEXED,
    seq        UNINDEXED,
    body
);

-- ---- message-event sync -----------------------------------------------------
-- Only `user_message` / `assistant_message` rows carry conversational
-- text; every other event type is skipped at the trigger so the index
-- stays clean. The text lives at data_json.'$.text'. Because the FTS rows
-- are contentless, UPDATE/DELETE use the standard "match the old mapping
-- columns, delete, re-insert" pattern (we can't rely on rowid alignment
-- with session_events).

CREATE TRIGGER session_fts_events_ai AFTER INSERT ON session_events
WHEN new.type IN ('user_message', 'assistant_message')
     AND json_extract(new.data_json, '$.text') IS NOT NULL
BEGIN
    INSERT INTO session_fts (row_kind, session_id, seq, body)
    VALUES ('message', new.session_id, new.seq,
            json_extract(new.data_json, '$.text'));
END;

CREATE TRIGGER session_fts_events_ad AFTER DELETE ON session_events
WHEN old.type IN ('user_message', 'assistant_message')
BEGIN
    DELETE FROM session_fts
    WHERE row_kind = 'message' AND seq = old.seq;
END;

CREATE TRIGGER session_fts_events_au AFTER UPDATE ON session_events
WHEN old.type IN ('user_message', 'assistant_message')
     OR new.type IN ('user_message', 'assistant_message')
BEGIN
    DELETE FROM session_fts
    WHERE row_kind = 'message' AND seq = old.seq;
    INSERT INTO session_fts (row_kind, session_id, seq, body)
    SELECT 'message', new.session_id, new.seq,
           json_extract(new.data_json, '$.text')
    WHERE new.type IN ('user_message', 'assistant_message')
      AND json_extract(new.data_json, '$.text') IS NOT NULL;
END;

-- ---- title sync -------------------------------------------------------------
-- A session's title is searchable too. Titles change via UPDATE (set /
-- auto-title / rename) and arrive NULL on insert, so we cover insert +
-- update and reconcile the single title row per session.

CREATE TRIGGER session_fts_title_ai AFTER INSERT ON sessions
WHEN new.title IS NOT NULL AND new.title <> ''
BEGIN
    INSERT INTO session_fts (row_kind, session_id, seq, body)
    VALUES ('title', new.session_id, NULL, new.title);
END;

CREATE TRIGGER session_fts_title_au AFTER UPDATE OF title ON sessions
BEGIN
    DELETE FROM session_fts
    WHERE row_kind = 'title' AND session_id = old.session_id;
    INSERT INTO session_fts (row_kind, session_id, seq, body)
    SELECT 'title', new.session_id, NULL, new.title
    WHERE new.title IS NOT NULL AND new.title <> '';
END;

CREATE TRIGGER session_fts_sessions_ad AFTER DELETE ON sessions
BEGIN
    DELETE FROM session_fts WHERE session_id = old.session_id;
END;

-- ---- backfill ---------------------------------------------------------------
-- Index every pre-existing session's title + message events so old
-- threads are searchable the moment this migration lands.

INSERT INTO session_fts (row_kind, session_id, seq, body)
SELECT 'message', session_id, seq, json_extract(data_json, '$.text')
FROM session_events
WHERE type IN ('user_message', 'assistant_message')
  AND json_extract(data_json, '$.text') IS NOT NULL;

INSERT INTO session_fts (row_kind, session_id, seq, body)
SELECT 'title', session_id, NULL, title
FROM sessions
WHERE title IS NOT NULL AND title <> '';
