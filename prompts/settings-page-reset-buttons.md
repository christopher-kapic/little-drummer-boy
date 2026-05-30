# Add per-page "reset to defaults" buttons in /settings

## Goal

Add a "reset this page to defaults" action to the Tools, Skills, and UI
settings pages (and design it so the Agents page can adopt it once that
page is built). Each reset restores that page's settings to their
default values, guarded by a confirm step.

## Current behavior

- Settings pages live in `src/tui/settings/` with a `Page` enum and
  per-page key handlers (`tools_page.rs`, `skills_page.rs`,
  `ui_page.rs`; `providers.rs`; Agents is a stub).
- The Tools page already has a **per-tool** reset (`r` restores one
  tool template via `default_template_for(name)`). There is no
  **page-level** reset anywhere.
- A confirm pattern already exists in the providers List page
  (`delete_pending` — press again to confirm).
- Page config lives in `ExtendedConfig` (`src/config/extended.rs`):
  the tools map, `TuiConfig` (display toggles), `utility_model`,
  `instructions`, display name, packages dir, and `SkillsConfig`.

## Desired behavior

Add a reset action (a labeled button/row, e.g. `[reset to defaults]`,
with a key binding) to each of these pages:

- **Tools page** — reset restores all built-in tool templates
  (webfetch, websearch, …) to their `default_template_for` defaults
  and removes any user-added/custom tool entries, returning the tools
  map to its default state.
- **Skills page** — reset restores `SkillsConfig` to its default: the
  seeded default scan-dir entries and the ancestor-walk toggle off.
  (This depends on the skills-scan-dirs change — see Notes.)
- **UI page** — reset restores **only the display toggles** to
  `TuiConfig::default()`: vim mode, thinking display, agent/user
  markdown rendering, mouse capture, rich-text copy, emojis, and
  caffeinate-display-awake. It must **preserve** the utility model,
  custom instructions, display name, and packages dir — those are not
  cleared by a UI-page reset.

Every reset is **guarded by a confirm step**: the first activation arms
a "press again to confirm" state (reuse the existing
`delete_pending`-style pattern), the second performs the reset and
persists via the page's existing save path. Any navigation away or a
cancel key disarms the pending reset.

## Edge cases & UX decisions (settled)

- **Confirm required** — single keypress arms, second confirms. Show a
  clear pending-confirm indicator while armed.
- **UI reset = display toggles only** — do not touch utility model,
  instructions, name, or packages dir.
- **Agents page deferred** — it is currently a stub. Do **not** add a
  reset there now, but factor the reset affordance (the
  arm/confirm/apply + render of the button) so it can be reused by the
  Agents page when that page is implemented, without copy-paste.
- Reset persists immediately on confirm, using each page's existing
  save mechanism (e.g. `save_extended`), and reflects the saved status
  the same way other edits do.

## Expected UX / acceptance

- Each of Tools, Skills, UI shows a reset affordance with a visible
  key hint.
- Activating it once shows a confirm-pending state; activating again
  resets the page and saves; navigating away cancels the pending
  reset.
- Tools reset → only built-in defaults remain. Skills reset → default
  scan dirs seeded, ancestor walk off. UI reset → display toggles at
  defaults, utility model / instructions / name / packages dir
  unchanged.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` pass.

## Notes

- **Depends on `prompts/skills-scan-dirs-seeded-defaults.md`.** Skills
  "default" means whatever that change establishes as the seeded
  default list plus the ancestor-walk-off state. If that prompt has
  not landed yet, reset to the `SkillsConfig` default as it then
  exists; do not hardcode a divergent default here.

## Constraints (non-negotiable)

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this
  prompt says otherwise (none are anticipated).
- Before wiring in any dependency, verify correct API/dependency usage
  with `kcl ask <package> "<question>"`.
