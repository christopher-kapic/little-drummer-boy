# Add `/permissions` slash command — delete-only approvals manager

## Goal

Add a `/permissions` slash command that opens a TUI for **viewing and
deleting** persisted tool approvals, so a user can undo an accidental
"allow" grant.

## Current behavior

- Approvals are persisted in three scopes (`src/approval/{mod,store}.rs`):
  - **Session** — SQLite `approval_grants` table, keyed
    `(session_id, grant_kind, grant_key)`; dies with the session.
  - **Project** — `.cockpit/approvals.json`
    (`{ commands, paths, loop_accept, loop_reject }`).
  - **Global** — `~/.config/cockpit/approvals.json` (same shape).
- The only existing UI is the prompt-on-demand approval dialog
  (`src/tui/dialog/approval.rs`). There is **no** way to review or
  revoke grants after the fact.

## Desired behavior

- Register a `/permissions` slash command that opens a management
  overlay/pane.
- **Scope coverage: project and global only.** Show the persisted
  `.cockpit/approvals.json` (project) and `~/.config/cockpit/...`
  (global) grants. **Do not** show session-scope grants (they expire
  with the session anyway).
- Group the listing by scope (Project / Global), and within each scope
  by grant kind (commands, paths, and the loop accept/reject entries),
  so the user can see exactly what was granted where.
- **Delete-only.** The only mutating action is removing a grant. There
  is no add, no edit, no scope-change in this UI. Selecting a row and
  pressing the delete/remove key removes that single grant and rewrites
  the corresponding JSON file. Removal takes effect for future tool
  calls (re-read on next approval check); no restart required.
- Deleting must be safe against concurrent edits — rewrite the file
  from the in-memory store the same way the existing approval store
  writes it; don't blindly clobber.
- Empty state: if a scope has no grants, show that explicitly rather
  than an empty section.

## Edge cases & UX decisions

- **Read-only safety:** there is no "delete all" bulk action in v1 —
  deletion is per-grant, to avoid accidental mass-revocation. (A
  confirmation on individual delete is optional; per-row delete is low
  blast-radius.)
- Wrapper/eval commands are never persisted (only ever approved
  "Once"), so they will not appear here — that's correct, no special
  handling needed.

## Acceptance

- `/permissions` opens a pane listing project + global grants grouped by
  scope and kind; deleting a row removes that grant from the backing
  JSON file and it no longer counts as pre-approved on the next tool
  call.

## Constraints

Implement without incurring tech debt — no shortcuts, no TODO-for-later,
no half-finished paths. For any new package use the latest stable
release unless this prompt says otherwise, and verify correct
API/dependency usage with `kcl ask <package> "<question>"` before wiring
it in. Slash-command descriptions are one sentence (token economy,
CLAUDE.md).
