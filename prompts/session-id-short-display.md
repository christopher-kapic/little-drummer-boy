# Show the short session id in the TUI banner, not the full UUID

## Goal

The TUI startup banner prints the full session UUID. It should print
the 6-char Crockford base32 `short_id` instead — short ids are the
ergonomic, user-facing identifier (the CLI already accepts a `short_id`
or a UUID for export/resume; the long UUID in the banner is a
regression).

## Current behavior

`src/tui/banner_box.rs` (~line 93) renders the session id with
`session_id.to_string()`, where `LaunchInfo.session_id` is an
`Option<Uuid>` (`src/welcome.rs`). That prints the full 36-char UUID
after the version on the title line.

The `short_id` is already produced and already reaches the TUI: the
daemon's `Attached` response carries `short_id: String`
(`src/daemon/proto.rs`), and the TUI's agent runner already captures it
(`src/tui/agent_runner.rs` — `short_id` field on the runner). It is
simply never propagated into `LaunchInfo`, so the banner falls back to
the UUID. `LaunchInfo.session_id` is assigned from `runner.session_id`
in three spots in `src/tui/app/mod.rs` (~lines 1729, 1782, 1905); the
adjacent `runner.short_id` is dropped.

## Desired behavior

Display the `short_id` in the banner instead of the UUID.

- Plumb the short id into `LaunchInfo` (e.g. an
  `Option<String>` field) and set it everywhere the UUID
  `session_id` is currently set on `LaunchInfo`, sourcing it from the
  runner's existing `short_id`.
- The banner renders the short id, in the same grey, in the same
  position (right after the version).
- Keep the existing `session_id: Option<Uuid>` field for whatever
  internal/routing use it still has — this change only alters what is
  *displayed*. Do not show the UUID to the user in the banner anymore.

## Edge cases & UX decisions

- `short_id` is always set on a live session, so the common path always
  has it. If the short id is somehow absent when the banner renders
  (id not yet assigned), show nothing in that slot — never fall back to
  printing the UUID.
- This is display-only. Do not change session creation, the DB schema,
  the wire protocol, or how sessions are resolved from CLI input.

## Acceptance

- Launching the TUI shows the 6-char short id (e.g. `k3m7qz`) after the
  version, not a UUID.
- The `banner_box` tests are updated to assert the short id is rendered
  (the existing `session_id_shows_after_version*` test currently feeds a
  UUID — update it to the new field/behavior).
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, and
  `cargo fmt --check` all pass.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths. Update the doc comment on the
  `LaunchInfo` session field (it currently documents the UUID being
  shown) to match the new behavior.
- For any new package use the latest stable release unless this prompt
  says otherwise (none expected here), and verify correct
  API/dependency usage with `kcl ask <package> "<question>"` before
  wiring it in.
