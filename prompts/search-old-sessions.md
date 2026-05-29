# Cross-session recall: `session_search` + `session_read`

## Goal

Let a user ask the active agent something like *"in another session
recently we talked about X — can you remember what we discussed?"* and
have the agent deterministically find the relevant past thread(s) and
read back what was said. Two new tools, token-efficient, no vector
embeddings in v1 (BM25 ranking only; embeddings may slot in later as a
second ranker without changing the tool surface).

## Current behavior

There is **no** session search of any kind today — no FTS, no LIKE scan.
The substrate that exists:

- Conversation text is **not** in a `messages` table. It lives in
  `session_events` rows where `type='user_message'` or
  `'assistant_message'`, with the text inside `data_json` as
  `{"text": ...}` (see `src/db/session_log.rs`).
- `sessions` carries `session_id` (UUID), `short_id` (6-char Crockford
  base32, user-facing), `title`, `started_at`, `last_active_at`,
  `project_id`, and `archived_at` (soft-delete).
- Tools run in the daemon with DB access via `ctx.session.db`. Tools
  implement the `Tool` trait (`src/engine/tool.rs`): `name`,
  `description`, `parameters` (JSON Schema), `call`. Distinct
  precise-schema tools are the house style for a fixed tool set (the
  `jobs` meta-tool pattern is only for mid-conversation growth — do not
  use it here).

## Desired behavior

Two tools.

### `session_search`
Returns a ranked list of past threads matching a query.

- **Engine: SQLite FTS5.** Add a migration creating an FTS5 virtual
  table that indexes session **titles** plus the **text of
  `user_message` and `assistant_message` events**. Do **not** index tool
  outputs, tool-call args, or raw inference payloads. Each FTS row maps
  back to `session_id` (+ `seq` for message rows) so hits resolve to a
  thread and an in-thread location.
- Keep the FTS table in sync with `session_events` via triggers
  (insert/update/delete) and with `sessions.title` changes. The
  migration must **backfill** all existing sessions' content into the
  FTS table, not just index events created after the migration.
- Rank by FTS5 **BM25 relevance**, with `last_active_at` recency as the
  tiebreaker.
- **Default scope is the current `project_id`.** A boolean param widens
  to all projects (global recall across repos). Default `false`.
- **Exclude archived sessions** (`archived_at IS NOT NULL`) and **exclude
  the current live session** from results by default.
- **Result budget:** top ~10 threads. Each result shows `short_id`,
  `title`, a human date (derived from `last_active_at`), and **one**
  ~150-char snippet with the matched terms highlighted (use FTS5
  `snippet()`). The agent can widen via an optional `limit` param.
- Support an optional date-range filter (e.g. `since`) so "recently" can
  be honored, since the motivating query is recency-flavored.

### `session_read`
Reads back the content of a chosen thread.

- Accepts a session identifier (the `short_id` shown by
  `session_search`; also accept the full UUID).
- Optional `query` param. **When a query is given, window the output
  around matching messages** — center on the matches with a few turns of
  surrounding context. **With no query, start from the first message.**
  Either way, paginate (follow the conventions of the existing `read`
  tool — paginated + range-addressable, message rows addressed by
  `seq`) so a long thread never dumps in full.
- Output is ordered user/assistant turns with light role labeling,
  trimmed to the pagination window.

## Edge cases & UX decisions (settled — implement as written)

- No FTS match → return an explicit empty result, not an error.
- Unknown / ambiguous `short_id` in `session_read` → clear error naming
  the id; if a `short_id` somehow collides, disambiguate by `project_id`
  (it is unique per project).
- `session_search` and `session_read` results become tool output and
  therefore pass back through the redaction chokepoint (`redact::scrub`)
  on the next outbound prompt like any other tool output — do not add a
  bypass, and do not pre-redact a second time.
- Archived and current-session exclusion is the default for search;
  reading an archived session by explicit `short_id` is allowed.

## Availability

Register both tools on **every agent that runs in an interactive
session** (the user-facing agents — at minimum `orchestrator-build` and
`orchestrator-plan`, and any other agent surfaced directly to the user
in interactive mode). Do **not** add them to non-interactive / one-shot
contexts or to the `docs` two-stage pipeline leaf agents. Determine how
"interactive mode" is detected from the existing codebase — do not
invent a mechanism. `explore` and `coder` are interactive-mode agents
only when they are the active user-facing agent; follow the same
interactive-mode gate rather than hard-coding a per-agent list.

## Expected UX / acceptance

- User asks the active interactive agent to recall a past discussion;
  the agent calls `session_search`, gets a short ranked list of threads
  with snippets, picks the likely one, calls `session_read` with the
  topic as `query`, and answers from the windowed transcript.
- Search defaults to the current project; a global flag finds threads in
  other repos.
- New `user_message`/`assistant_message` events and title changes are
  immediately searchable (triggers), and pre-existing sessions are
  searchable after the migration (backfill).
- Results are ranked, snippet-bounded, and the default search stays well
  within a tight token budget.

## Suggested packages

- **None required.** `rusqlite` is already a dependency and its
  `bundled` SQLite amalgamation compiles with FTS5 enabled — verify this
  build actually has FTS5 available before relying on it (a one-off
  `SELECT` against an FTS5 table, or checking the `bundled`/feature
  setup). If FTS5 turns out not to be compiled in, surface that and stop
  rather than silently falling back to LIKE.

## Constraints (non-negotiable)

- Implement this **without incurring tech debt** — no shortcuts, no
  `TODO`-for-later, no half-finished paths. The LIKE fallback was
  explicitly rejected; FTS5 is the v1 engine.
- For any new package, use its **latest stable release** unless this
  prompt says otherwise.
- Verify correct API / dependency usage with
  `kcl ask <package> "<question>"` before wiring it in — in particular
  `kcl ask rusqlite "..."` for FTS5 virtual-table creation, triggers,
  `bm25()` ranking, and `snippet()` usage from rusqlite.

## Notes

- Two tools, not one tool with a mode (fixed tool set → precise
  schemas).
- Deterministic only in v1; leave a clean seam where a future embedding
  ranker could re-rank FTS candidates without changing either tool's
  schema.
- Token economy is a hard requirement: tool descriptions one sentence,
  parameter descriptions noun-phrases, ranked + snippet-bounded output.
