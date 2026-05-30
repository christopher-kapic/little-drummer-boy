# Add `/skills` slash command — list available skills

## Goal

Add a `/skills` slash command that shows the user every skill available
in the current project, in a read-only overlay.

## Current behavior

- Skills are discovered by `skills::discover(cwd, cfg)`
  (`src/skills/mod.rs:65`), scanning the configured `scan_dirs` for
  `<dir>/<name>/SKILL.md`, de-duplicated by name (first occurrence
  wins).
- Per-skill data: `Skill { frontmatter: { name, description, model },
  source: PathBuf }`.
- The daemon exposes `Request::ListSkills` → `Response::Skills { skills:
  Vec<SkillSummary> }` where `SkillSummary { name, description, source }`
  (`proto.rs` ~246 / ~423 / ~874).
- There is **no** user-facing command to list skills today.

## Desired behavior

- Register a `/skills` slash command (`src/tui/app/mod.rs:192-241` +
  dispatch).
- It opens a **read-only** scrollable overlay listing all discovered
  skills. For each skill show: name, its one-line description, and its
  source path (so the user can tell which scan-dir / which copy won when
  names collide).
- Pull the list via the existing `ListSkills` daemon RPC — do not
  re-implement discovery in the TUI.
- The overlay is informational only: no selecting, no invoking, no
  editing. Esc / the standard close key dismisses it.
- If no skills are found, show an empty-state line (e.g. "No skills
  found in the configured scan directories.") rather than an empty box.

## Acceptance

- `/skills` opens an overlay listing every discovered skill with
  name + description + source, scrollable if long, dismissible, and
  read-only.

## Constraints

Implement without incurring tech debt — no shortcuts, no TODO-for-later,
no half-finished paths. For any new package use the latest stable
release unless this prompt says otherwise, and verify correct
API/dependency usage with `kcl ask <package> "<question>"` before wiring
it in. Slash-command descriptions are one sentence (token economy,
CLAUDE.md).
