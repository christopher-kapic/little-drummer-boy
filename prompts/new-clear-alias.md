# Alias `/new` to `/clear`

## Goal

Add a `/new` slash command that does exactly what `/clear` does (start
a fresh/empty session), so users can reach the same action under either
name.

## Current behavior

`/clear` exists in the slash menu and clears/starts a new session.
There is no `/new`.

## Desired behavior

- `/new` invokes the identical behavior as `/clear` — same handler, no
  divergence.
- **Both `/clear` and `/new` appear as selectable entries in the slash
  menu**, each with its own listing, both dispatching to the same
  action.

## Edge cases & UX decisions

- The two entries must stay in sync: route both names to the single
  existing handler rather than duplicating logic, so future changes to
  the clear behavior apply to both automatically.

## Expected UX / acceptance

- Typing `/new` (or selecting it from the slash menu) clears the
  session exactly as `/clear` does.
- Both `/clear` and `/new` are visible and selectable in the slash
  menu.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths. Use the existing command
  dispatch/alias mechanism if one exists rather than copy-pasting the
  handler.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (No new
  dependency is expected for this.)

## Notes

- Slash-menu visibility (show both) is settled per the user.
