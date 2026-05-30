# `/export` — export current session to `.cockpit/exports/`

## Goal

Add an `/export` slash command to the TUI with two modes:

- `/export` (default, **visible** in the slash menu) — export the
  **current conversation** as a JSON transcript to
  `{cwd}/.cockpit/exports/<short_id>.json`.
- `/export debug` (**hidden** option) — export the full debug bundle
  (the exact `.zip` the CLI `cockpit export` produces) to
  `{cwd}/.cockpit/exports/<short_id>.zip`.

## Current behavior

`cockpit export <session>` (CLI, `src/commands/export.rs`) bundles a
target session plus every descendant fork and `/compact` successor into
a self-contained `.zip` (`manifest.json` + unified `events.json` +
`inference_requests/`), naming it `cockpit-session-<short_id>.zip` in
the cwd and refusing to clobber without `--force`. There is no in-TUI
way to trigger any export.

## Desired behavior

### Common

- Both modes write into `{cwd}/.cockpit/exports/`, where `<cwd>` is the
  literal current working directory (not the walked-up `.cockpit/`
  config root). Create the directory if it doesn't exist.
- `<short_id>` is the current session's short id, falling back to the
  full UUID when no short id is set (matching the CLI's
  `default_output_path`).
- **Collision: overwrite.** If the target file already exists, replace
  it with the fresh export. No force flag, no refusal.
- On success, show a TUI confirmation with the written path; surface
  any failure (write error, no current session) as a TUI error message,
  not a panic.

### `/export` (default — conversation JSON)

- Exports the **current live transcript** of the session open in the
  TUI: the conversation as it currently stands (post-`/compact`,
  post-fork live state) — only the current session, not the fork tree
  or compaction predecessors.
- Output is a single JSON file: an ordered array of conversation turns.
  Include the **full visible transcript** — user and assistant messages
  **and** tool calls / results in their **user-facing form** (the
  `original_input` + recovery view per the wire-vs-user split, GOALS
  §14 — never the wire form). Mirror what the TUI actually renders.
- Path: `{cwd}/.cockpit/exports/<short_id>.json`.
- Visible/selectable in the slash menu.

### `/export debug` (hidden — full bundle)

- Produces the exact same archive the CLI `cockpit export` produces for
  the current session: the full bundle (target session + descendant
  forks + `/compact` successors), `manifest.json` + unified
  `events.json` + `inference_requests/`.
- **Reuse the existing archive builder** — share `collect_bundle` /
  `build_zip` from `commands/export.rs` rather than reimplementing zip
  assembly. Refactor those into a shared location if needed so the CLI
  and the TUI debug export call exactly one implementation. The shared
  writer needs an unconditional-overwrite mode (the TUI can't pass
  `--force`) that does not weaken the CLI's existing
  no-clobber-without-`--force` guarantee.
- Reads the DB directly the same way the CLI does, so it works
  regardless of daemon state.
- Path: `{cwd}/.cockpit/exports/<short_id>.zip`.
- **Hidden:** `/export` is the only entry shown in the slash menu; the
  `debug` argument works when typed but is not listed or advertised.

## Edge cases & UX decisions

- The two modes never collide on disk — different extensions
  (`.json` vs `.zip`) under the same directory.
- `<short_id>.json` and `<short_id>.zip` each overwrite their own prior
  file on re-export.
- If there is no current session (shouldn't normally happen from the
  TUI), show an error rather than writing an empty file.

## Expected UX / acceptance

- Running `/export` writes `{cwd}/.cockpit/exports/<short_id>.json`
  containing the current conversation (messages + user-facing tool
  calls/results) and shows a confirmation with the path.
- Running `/export debug` writes `{cwd}/.cockpit/exports/<short_id>.zip`
  identical in structure to `cockpit export`'s output for the session.
- `/export` appears in the slash menu; `debug` does not appear but works
  when typed.
- Re-running either mode overwrites its prior file.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths. For the debug bundle, factor
  the shared export logic so there is exactly one zip-assembly
  implementation behind both the CLI and the slash command — do not
  copy-paste it.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (No new
  dependency is expected — `zip` and `serde_json` are already in use.)

## Notes

- Settled per the user: two modes (`/export` visible, `/export debug`
  hidden); default exports the current live transcript as JSON
  including the full user-facing transcript (messages + tool
  calls/results); debug exports the full CLI bundle `.zip`; both go to
  `{cwd}/.cockpit/exports/` named `<short_id>` with overwrite on
  collision.
