# Utility-model selection: picker over available models, free-text fallback

## Goal

Replace the free-text utility-model field on the /settings → UI page
with a picker that suggests models from the user's configured
providers, while still allowing a custom `provider:model-id` to be
typed for models not in the fetched list.

## Current behavior

- The utility model (used for auto-titling, etc.) is stored as
  `ExtendedConfig.utility_model: Option<String>` in the form
  `provider:model-id` (`src/config/extended.rs`).
- On the UI page (`src/tui/settings/ui_page.rs`), it is edited as
  **free text** — the user types `provider:model-id` directly, with no
  awareness of which providers/models are actually configured.
- Configured providers and their fetched models live in
  `config.json` as `ProviderEntry.models: Vec<ModelEntry>`
  (`src/config/providers.rs`); each `ModelEntry` has `id`, `name`,
  `context_length`, etc. Models are fetched per-provider via the
  `/models` fetcher.

## Desired behavior

Activating the utility-model field opens a **picker** listing every
model across all configured providers, each shown as
`provider:model-id` (include the human `name` if helpful). Selecting an
entry sets `utility_model` to that `provider:model-id`.

- **Free-text fallback:** the picker must offer a way to type a custom
  `provider:model-id` that is not in the fetched list, and accept it as
  the value. A model not present in any provider's fetched list is
  still a valid choice.
- **Ordering:** plain list grouped by provider, in each provider's
  natural model order. No ranking/curation heuristic.
- **Current value:** if a utility model is already set, preselect /
  highlight it in the picker (or show it as the current value).

## Edge cases & UX decisions (settled)

- **No models available** (no providers configured, or none fetched
  yet): the picker has nothing to list — fall back to free-text entry
  and show a brief hint that models can be fetched from the Providers
  page. Do not block setting a value.
- **Plain ordering** — grouped by provider, natural order; do not sort
  or rank by size/cost/name.
- **Clearing the value** — allow clearing back to unset
  (`utility_model = None`) so the default utility-model behavior
  applies.
- Selection persists via the UI page's existing save path and reflects
  saved status like other UI-page edits.

## Expected UX / acceptance

- Opening the utility-model field shows a picker of `provider:model-id`
  entries grouped by provider, with the current value indicated.
- Selecting an entry sets and saves it; a custom id can be typed and
  accepted via the free-text fallback; the value can be cleared.
- With no fetched models, the field still works via free-text entry and
  hints where to fetch.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` pass.

## Constraints (non-negotiable)

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this
  prompt says otherwise (none are anticipated; reuse existing
  picker/list UI patterns in the settings TUI).
- Before wiring in any dependency, verify correct API/dependency usage
  with `kcl ask <package> "<question>"`.
