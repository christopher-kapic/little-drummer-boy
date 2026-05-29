# Skills scan-dirs: seed defaults as real entries, drop the implicit fallback

## Goal

Make the skills scan-directory list contain its defaults as real,
visible, removable entries instead of being empty with an implicit
"empty = defaults" fallback. An empty list must genuinely mean empty
(no skill directories scanned).

## Current behavior

- `SkillsConfig.scan_dirs: Vec<String>` (`src/config/extended.rs`)
  holds the configured scan directories. New users have no config on
  disk; the field defaults to an empty `Vec`.
- `src/skills/mod.rs::resolve_scan_dirs` falls back to
  `default_scan_dirs(cwd)` whenever `scan_dirs` is empty.
- `default_scan_dirs` is not a fixed list: it returns
  `~/.agents/skills` **plus** `./.agents/skills` at cwd **and every
  ancestor directory up to the git worktree root**.
- The settings TUI (`src/tui/settings/skills_page.rs`) renders an
  empty list with the help text `Empty list = defaults
  (~/.agents/skills + ./.agents/skills)` and a `[+ add directory]`
  row. The list editor already supports add / edit / delete and
  resolves entries via `resolve_dir_entry` (handles `~`, `$VAR`, and
  relative-against-cwd paths).

## Desired behavior

1. **Remove the implicit fallback.** `resolve_scan_dirs` no longer
   substitutes defaults when `scan_dirs` is empty. An empty list
   resolves to zero directories — no skills are discovered.

2. **Seed the defaults as concrete entries for fresh installs.** A
   user who has never written an extended-config sees the default
   entries in the list, as ordinary editable/removable rows:
   - `~/.agents/skills`
   - `./.agents/skills`

   Use the same string forms the existing `resolve_dir_entry`
   understands (`~` home, relative-against-cwd). The seeded list is
   exactly these two entries — **do not** seed the broader documented
   set (`.cockpit/skills`, `.claude/skills`, home variants); that
   discovery expansion is out of scope for this task.

3. **Add a boolean "ancestor walk" setting** to `SkillsConfig`,
   default `false`. It controls how *relative* scan-dir entries
   resolve:
   - `false` (default): relative entries resolve against cwd only —
     the existing `resolve_dir_entry` behavior.
   - `true`: each relative entry expands at resolve time to cwd **plus
     every ancestor directory up to and including the git worktree
     root** — i.e. the old `default_scan_dirs` ancestor-walk behavior,
     now opt-in and applied generally to relative entries.

   Expose it in the settings TUI as a toggle row alongside the
   existing `auto-! commands` toggle on the skills page.

## Edge cases & UX decisions (settled — implement as written)

- **Empty means empty.** Deleting every entry leaves the list empty
  and scans no skill directories. No warning, no re-seed.
- **Clean break for existing configs.** A config already on disk whose
  `scan_dirs` is empty or absent resolves to **no** directories. Do
  **not** inject defaults at resolve time for existing configs, and do
  **not** auto-migrate them. Defaults are materialized only for a
  genuinely fresh install (no extended-config yet) so new users see
  them in the list. Accept that existing users relying on the old
  implicit defaults will see no skills until they re-add the entries.
- **Settings page reflects reality.** Update the skills-page help text
  — the `Empty list = defaults (...)` line is no longer true. State
  that the list ships pre-seeded, that an empty list scans nothing,
  and mention the ancestor-walk toggle. Update the doc-comment on
  `SkillsConfig.scan_dirs` (it currently documents the "when empty,
  the defaults apply" behavior) to match.
- **Ancestor-walk toggle default off** — a fresh skills page shows the
  toggle in the off state.

## Serde caution (defensive — get this right)

`scan_dirs` carries `#[serde(default)]`, which fills a *missing* field
using the field type's `Default` (an empty `Vec`), **not** the struct's
`Default` impl. So seeding via `SkillsConfig::default()` will not, on
its own, distinguish "field absent in an existing on-disk config"
(must stay empty — clean break) from "fresh install with no config
file" (must show seeded defaults). Choose a mechanism that keeps these
two cases distinct and matches the settled contract above; verify both
paths behave correctly (existing empty config → no dirs; fresh install
→ two seeded entries shown in settings).

## Expected UX / acceptance

- A fresh user opens `/settings` → skills page and sees two entries
  (`~/.agents/skills`, `./.agents/skills`), an ancestor-walk toggle
  (off), and the `[+ add directory]` row. The entries can be edited
  and deleted.
- Deleting all entries and saving results in no skill directories
  being scanned.
- Toggling ancestor walk on makes relative entries scan cwd plus
  ancestors up to the worktree root; off restricts them to cwd.
- An existing config with empty/absent `scan_dirs` scans no skill
  directories.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, and
  `cargo fmt --check` all pass. Add/adjust tests covering
  `resolve_scan_dirs` for: empty list → no dirs; relative entry with
  ancestor walk off vs. on.

## Out of scope

- Expanding skill discovery to the documented `.cockpit/skills`,
  `.claude/skills`, or home-directory variants. If you notice the
  doc/impl gap (GOALS §5 / CLAUDE.md describe a broader set than the
  code implements), leave a brief note flagging it rather than
  widening this change.

## Constraints (non-negotiable)

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this
  prompt says otherwise (no new packages are anticipated for this
  task).
- Before wiring in any dependency, verify correct API/dependency usage
  with `kcl ask <package> "<question>"`.
