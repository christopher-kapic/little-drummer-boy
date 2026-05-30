# Add `/rename` slash command; remove the `/session` stub

## Goal

Replace the dead `/session` subcommand-router stub with a first-class
`/rename` command that renames the current session.

## Current behavior

- `/session` is registered but unimplemented — dispatch returns a stub
  message ("subcommand router not wired yet") at
  `src/tui/app/mod.rs:3260-3262`. Its only ever-intended subcommand was
  `rename`; nothing else routes through it.
- The daemon already has the RPC: `Request::RenameSession { session_id,
  title }` (`src/daemon/.../proto.rs:230`).
- The `sessions` table already has `title TEXT` and `user_renamed
  INTEGER` columns (migration `0011`), where `user_renamed` exists to
  lock out auto-titling once the user has manually named a session.

## Desired behavior

- **Remove `/session` entirely** — drop it from the slash-command list
  (`src/tui/app/mod.rs:192-241`) and remove its dispatch arm. No alias,
  no deprecation shim; it was never functional.
- **Add `/rename <title>`** — renames the *current* session:
  - Sends `RenameSession { session_id: <current>, title }` over the
    daemon RPC.
  - Sets `user_renamed = 1` so auto-titling (`auto_title.rs`) stops
    overriding the name. Confirm the RPC path already does this; if it
    doesn't, make the rename set the flag.
  - The new title should be reflected wherever the session title is
    shown (banner / sessions browser) without requiring a restart.
- **Bare `/rename` (no argument)** shows usage only — print `` Usage:
  `/rename <title>` `` (or the project's standard usage-message style)
  and do not change anything. Do **not** open a dialog.

## Edge cases

- Title is the full remainder of the command line after `/rename `
  (allow spaces); trim surrounding whitespace. If, after trimming, the
  title is empty, treat it as the bare/no-arg case (show usage).
- Renaming applies to the current session only.

## Acceptance

- `/session` no longer appears in the slash menu or dispatches.
- `/rename my new name` renames the active session, the new name is
  visible immediately, and auto-titling never overrides it afterward.
- `/rename` alone prints usage and changes nothing.

## Constraints

Implement without incurring tech debt — no shortcuts, no TODO-for-later,
no half-finished paths. For any new package use the latest stable
release unless this prompt says otherwise, and verify correct
API/dependency usage with `kcl ask <package> "<question>"` before wiring
it in. Slash-command descriptions are one sentence (token economy,
CLAUDE.md).
