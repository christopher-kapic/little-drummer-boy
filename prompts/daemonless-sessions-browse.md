# Daemonless `/sessions`: browse from the DB without a daemon

## Goal

`/sessions` should open and list sessions even when no daemon is connected
("continue without daemon" mode), by reading the session DB directly
read-only. Today it shows a "daemon unavailable" error instead.

## Current behavior

`SessionsPane` fetches the list exclusively through the daemon:
`agent_runner::list_sessions_blocking()` → `Request::ListSessions` RPC
(`src/tui/agent_runner.rs` ~372). With no daemon the call returns
`Err("daemon not running")` and the pane renders
`daemon unavailable: …` (`src/tui/sessions_pane.rs` ~632). No session data
is shown daemonlessly.

Precedent for daemonless DB reads already exists: the `/stats` pane opens
the DB directly read-only via `Db::open_default()`
(`src/tui/stats_pane.rs` ~111). The DB is WAL (concurrent readers
allowed), so a direct read is safe.

## Desired behavior

- **List daemonlessly via direct DB read.** When no daemon is connected,
  `/sessions` reads the DB directly (read-only) and lists sessions instead
  of erroring. Use the daemon's `ListSessions` handler as the reference
  for fields, ordering, project scoping, and parent/child (fork) grouping.
  **Reuse the query logic, don't duplicate SQL**: if the daemon handler
  already calls a `Db` method, call that same method from the TUI; if the
  logic is inline in the daemon handler, factor it into a `Db` method both
  the daemon and the TUI call.
- **When a daemon IS connected, keep the existing RPC path** so live
  status (e.g. `SessionLiveStatus`, live-activity unread detection) still
  works. The direct-DB read is a fallback for the daemonless case only.
- **Graceful degradation of live-only data.** Daemonless, anything that
  needs a live daemon (live running/active indicators) is simply absent —
  fall back to DB-derived fields (`started_at`, `last_active_at`,
  `last_viewed_at`, `latest_activity_at` are on the row) for tiers/unread.
  Missing live data must never produce an error state.
- **Resume stays disabled daemonless (decided).** Running a session needs
  the daemon (agent loop, file locks, single-writer all live there), so in
  daemonless mode selecting/resuming a session must NOT auto-spawn a
  daemon. Instead show a clear, non-error status line: resuming needs a
  daemon (and how to start one). The daemon-backed resume path is
  unchanged when a daemon is connected.
- **Archive / unarchive / delete stay daemon-only (decided).** Do not add
  a direct-DB write path. Daemonless, these actions are disabled with a
  clear message.

## Edge cases

- DB missing or unopenable → clear message in the pane, no crash.
- Daemonless ⟺ no daemon running (the startup probe gates this), so the
  direct read won't contend with a daemon writer; still open the DB
  read-only.

## Acceptance

- No daemon → `/sessions` opens and lists sessions from the DB.
- Daemonless resume attempt → informative status line, no crash, no daemon
  spawned.
- Daemonless archive/delete → disabled with a clear message.
- Daemon connected → `/sessions` behavior unchanged, live status intact.

## Constraints

- Implement without incurring tech debt: no shortcuts, no TODO-for-later,
  no half-finished paths. In particular, factor shared list-query logic
  into one place rather than copy-pasting the daemon's SQL into the TUI.
- No new dependency is expected. If you add one, use its latest stable
  release and verify API usage with `kcl ask <package> "<question>"`.
- Gates must pass: `cargo build`, `cargo test`, `cargo clippy -- -D
  warnings`, `cargo fmt --check`.
