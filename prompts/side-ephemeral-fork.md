# Add `/side` slash command — ephemeral side conversation in a fork

## Goal

Add a `/side` command that opens a throwaway side conversation forked
from the current point, lets the user chat in it without polluting the
main session, then discards it and returns to the main session
unchanged.

## Current behavior

- The daemon already supports forking: `Request::ForkSession {
  parent_session_id, fork_point_turn_id }` → `Response::Forked {
  session_id, short_id, parent_session_id, fork_point_turn_id }`
  (`proto.rs` ~222 / ~410). The `sessions` table carries
  `parent_session_id` and `fork_point_turn_id` (migration `0002`).
- The existing `/fork` command is a **stub** ("ForkSession RPC is live;
  TUI re-attach flow ships in a later cut", `src/tui/app/mod.rs`
  ~3256-3259). `/side` is a *different* feature: `/fork` is meant to be
  a persisted branch you re-attach to later; `/side` is ephemeral.
- An "ephemeral" notion exists at the daemon level (`ephemeral: bool`,
  `COCKPIT_EPHEMERAL_DAEMON_SOCKET`) but there is no ephemeral-fork TUI
  flow.
- The sessions browser (`src/tui/sessions_pane.rs`) already understands
  fork trees and can drill into forks.

## Desired behavior

- Register a `/side` slash command.
- Running `/side` forks the current session at the current turn
  (reusing `ForkSession`) and switches the TUI into that fork as a
  **side conversation**: the user sees the full prior history and can
  send messages, but it is marked ephemeral.
- The TUI chrome must make it visually obvious the user is in a side
  conversation (e.g. an indicator alongside the fixed chrome slots),
  so it's never confused with the main session.
- **Ending the side conversation discards it.** Provide a clear exit —
  `/side end` (and the standard cancel/Esc affordance) — that:
  - returns the user to the **main** (parent) session exactly where
    they left off, untouched, and
  - **discards the ephemeral fork**: it must not persist and must
    **not** appear in `/sessions` / the sessions browser.
- Because the fork is ephemeral, ensure it is never auto-titled, never
  surfaced as resumable, and its storage is cleaned up on end (and also
  cleaned up if the process exits while a side conversation is open —
  no orphaned ephemeral sessions accumulating in the DB).

## Edge cases & UX decisions

- **Discard is unconditional** — there is no "keep this fork?" prompt.
  If the user wants a persisted branch, that's `/fork`, not `/side`.
- `/side` while already in a side conversation: either no-op with a
  message, or nest — pick the simpler correct behavior (a flat, no-op
  "already in a side conversation" is acceptable) and make it
  deterministic; don't leave a half-defined nesting path.
- If forking fails (daemon error), report it and stay in the main
  session.
- Background jobs/loops: a side conversation should not adopt or stop
  the parent's jobs; scope job ownership to the session that started
  them.

## Acceptance

- `/side` enters an ephemeral fork showing prior history, clearly
  marked; the user can converse freely; `/side end` (or Esc) returns to
  the unchanged main session and the fork is gone — absent from
  `/sessions` and from the DB.

## Notes

This interacts with in-flight ephemeral/daemonless lifecycle work
(`prompts/daemonless-tui-ephemeral-lifecycle.md`,
`prompts/daemonless-sessions-browse.md`). Reuse the existing ephemeral
mechanism rather than inventing a parallel one; if the cleanup hook
belongs in that lifecycle code, put it there.

## Constraints

Implement without incurring tech debt — no shortcuts, no TODO-for-later,
no half-finished paths. For any new package use the latest stable
release unless this prompt says otherwise, and verify correct
API/dependency usage with `kcl ask <package> "<question>"` before wiring
it in. Slash-command descriptions are one sentence (token economy,
CLAUDE.md).
