# Session ID display + lazy session persistence

## Goal

Two related changes to session lifecycle:

1. Show the current session ID in the TUI startup graphic, immediately
   after the cockpit version string.
2. Persist a session to the database only after its **first user
   message** — a session that is opened but never used leaves no DB row.
3. On TUI exit, print the last opened session ID — but only if that
   session was actually persisted.

## Current behavior

- The startup graphic's title line is built in
  `src/welcome.rs::header_lines` as
  `format!("{BOLD}{APP_NAME}{RESET} {GREY}v{}{RESET}", info.version)`
  (e.g. `cockpit v0.1.0`). `LaunchInfo` (same file) carries the fields
  shown in the header.
- Sessions are created and written to the SQLite `sessions` table
  (see `src/db/`, `src/session/`) at session start. Session/lock/
  inference state lives in the daemon, not the TUI process
  (daemon-first; the TUI is a client). A session opened with no input
  still produces a DB row today.
- Nothing is printed about the session on TUI exit.

## Desired behavior

### 1. Session ID in startup graphic (TUI only)

- Append the session ID to the title line, right after the version,
  in the same grey/secondary style as the version (e.g.
  `cockpit v0.1.0  <session-id>`). Match the existing
  spacing/styling idiom in `header_lines`; pick a clean separator
  consistent with the surrounding chrome.
- This is **TUI only** — the headless `run` command and other
  session-creating entry points are unchanged for now.
- The full session ID is shown (not a shortened form).
- The session ID must therefore exist in memory at startup, before the
  session is persisted (see §2). Generate/assign the ID up front; defer
  only the DB write.

### 2. Lazy persistence — persist on first user message

- When a new session is created, hold it in memory (in the daemon,
  consistent with daemon-owned session state) without writing the
  `sessions` row.
- Write the `sessions` row when the user submits their **first
  message**. "First event" = first user message specifically — not the
  first inference call, tool call, or any non-user activity.
- The first-message persistence must flush the `sessions` row **before**
  any rows that reference it (tool_calls, inference_calls, locks, etc.)
  so foreign-key / ordering invariants hold. Ensure no dependent write
  can occur for an unpersisted session.
- A session that is opened and then closed without any user message is
  never written to the DB and never appears in `session list` / resume
  listings.

### 3. Print last session ID on exit (TUI only)

- After the user exits the TUI, print the last opened session ID to the
  terminal.
- Print it **only if the session was persisted** (i.e. it had at least
  one user message and thus has a DB row). If the session was never
  persisted (opened but unused), print nothing about it.
- Use the same full session ID shown at startup.

## Edge cases & decisions (settled)

- **ID before persistence:** the session ID is generated at session
  creation and displayed at startup even though the DB row is deferred.
- **First event definition:** strictly the first *user message*. Model
  or tool activity alone does not persist a session (in normal flow a
  user message precedes any activity, so this is the natural trigger).
- **Empty session on exit:** unpersisted (unused) session → no DB row,
  not listed, and nothing printed on exit.
- **Persisted session on exit:** print the full session ID.
- **Scope:** all three behaviors are TUI-only. Do not change headless
  `run` or other entry points.

## Expected UX / acceptance

- Launching the TUI shows `cockpit v<version>  <session-id>` in the
  startup graphic header.
- Opening the TUI and quitting without typing anything: no new row in
  `sessions`, the session does not appear in `session list`, and nothing
  is printed on exit.
- Opening the TUI, sending one message, then quitting: a `sessions` row
  exists, the session appears in listings, and the session ID is printed
  on exit. The printed ID matches the one shown at startup.

## Constraints (non-negotiable)

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths. The change must be complete
  and correct across the TUI/daemon split.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (No new
  dependency is anticipated for this task.)
- Respect the daemon-first design: session state is owned by the daemon,
  not the TUI process — deferred persistence and the in-memory session
  ID must live where session state already lives.
- Add/adjust tests so the lazy-persistence trigger and the exit-print
  conditions are covered.
