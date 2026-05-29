# Make the exit session-id line match the welcome-box short id

## Goal

When the user exits the TUI, the printed session line must show the same
6-character short id that the welcome box shows at startup. Today the box
shows the short id and the exit line prints the full UUID, so they look
like two unrelated identifiers.

## Current behavior

- The welcome banner shows the daemon-assigned **short id** (6-char
  Crockford base32), set on attach. See
  `src/tui/banner_box.rs` (`content_lines`, the `session_short_id` block)
  and `src/tui/app/mod.rs` `ensure_agent_runner` (populates
  `launch.session_id` + `launch.session_short_id` from the attach
  response).
- On exit, the TUI prints `session {session_id}` using the **full UUID**,
  gated by `current_session_persisted`. See `src/tui/app/mod.rs` around
  the exit/teardown path (`if self.current_session_persisted && let
  Some(session_id) = self.launch.session_id { println!("session
  {session_id}"); }`).

The two are the *same* session — the short id and UUID are generated
together for one deferred session in
`Db::new_session_row` / `Session::create_deferred`
(`src/db/sessions.rs`, `src/session/mod.rs`). The only problem is that the
two display points render different forms.

## Desired behavior

The exit line prints the **short id** instead of the full UUID, so it
matches the welcome box exactly. Keep the existing gating: only print when
`current_session_persisted` is true (an opened-but-never-messaged session
left no DB row, so still print nothing).

- Use `self.launch.session_short_id` for the exit print.
- If `session_short_id` is somehow absent but the session was persisted,
  fall back to the full UUID rather than printing nothing (defensive — the
  short id should always be present once attached, but don't silently drop
  the line).
- Keep the line prefix `session ` (i.e. `session <short_id>`).

## Already-implemented (do NOT rebuild)

The second half of the original request — "generate the session id before
the first message, but only persist to the DB after the first message" —
is **already implemented** under the `session-id-display-and-lazy-persist`
work:

- `ensure_session_for_display()` runs each event-loop tick and attaches a
  **deferred** session (`Session::create_deferred`) so the short id exists
  and renders in the banner before any message is sent.
- No `sessions` DB row is written until the first user message commits it
  (`session_worker.rs` `persist_if_needed`; `current_session_persisted`
  flips true on first submit in `src/tui/app/input.rs`).

Verify this still holds, but do not re-implement it. The actual fix is the
single display change above.

## Verify before finishing

- The short id printed at exit is the **same session** shown in the
  welcome box for a normal start → send a message → exit flow.
- The printed short id remains usable to look the session back up
  (`cockpit session export <short_id>` resolves via
  `find_sessions_by_short_id_global`) — don't change that lookup; just
  confirm the printed value still works with it.
- A start → exit-without-sending flow still prints nothing.
- `/new` and `/compact` / resume paths (which set
  `current_session_persisted = true` and update `session_short_id`) still
  print the correct short id on a later exit.

## Constraints

- Implement without incurring tech debt: no shortcuts, no TODO-for-later,
  no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (No new dependency
  is expected here.)
- Gates must pass: `cargo build`, `cargo test`, `cargo clippy -- -D
  warnings`, `cargo fmt --check`.
