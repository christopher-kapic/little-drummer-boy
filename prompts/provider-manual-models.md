# Provider edit page: manually add models (for providers without /models)

## Goal

Let the user add arbitrary model entries by hand on a provider's edit
page, so providers that don't support the `/models` fetch endpoint can
still have usable models. Manual entries survive a later refetch.

## Current behavior

- A provider's models live in `ProviderEntry.models: Vec<ModelEntry>`
  in `config.json` (`src/config/providers.rs`). `ModelEntry` has `id`,
  `name`, `context_length`, `thinking_modes`, `favorite`, `cache`,
  `shrink`, `extra`.
- The models list is populated only by the `/models` fetcher
  (per-provider refetch `r` on the Edit page, or the all-providers
  flow). There is **no** way to add a model by hand.
- The provider Edit page (`src/tui/settings/providers.rs`,
  `EditState`) offers actions like Refetch /models, Headers, Favorite,
  Delete, Back.

## Desired behavior

Add an "add model" action to the provider Edit page that lets the user
enter a model manually. The entry form collects:

- **id** (required) — the model id sent to the provider.
- **display name** (optional) — falls back to the id if blank.
- **context length** (optional) — numeric.

All other `ModelEntry` fields take their defaults.

Manual entries are **marked as manual** (add a flagged field to
`ModelEntry`, serialized with a serde default for backward compat) so
they can be distinguished from fetched ones.

### Refetch interaction (preserve manual entries)

A `/models` refetch must **merge, not clobber**: fetched models refresh
the fetched portion of the list while manually-added entries are
retained. Dedupe by `id` — if a refetch returns an id that matches a
manual entry, the manual entry is authoritative and is kept (no
duplicate row). This applies to both the per-provider refetch and the
all-providers fetch.

### Management

- Manual entries can be **edited** (id / name / context) and
  **deleted**.
- Fetched entries can be **deleted** too (they reappear on the next
  refetch). Editing fetched entries is **out of scope**.

## Edge cases & UX decisions (settled)

- **id required, name/context optional** — blank name falls back to id.
- **Manual entries survive refetch** via the manual flag + id-dedupe
  merge described above; manual wins on id collision.
- **Validation:** reject an empty id and a duplicate id within the same
  provider with a clear status message; do not silently add.
- Manual models flow into every place fetched models are used (default
  model selection, the utility-model picker, etc.) because they live in
  the same `models` list — no extra wiring should be needed, but verify
  they appear there.
- Adds/edits/deletes persist via the existing provider-config save
  path and reflect saved status like other edits.

## Expected UX / acceptance

- The provider Edit page has an "add model" action; entering id (+
  optional name/context) appends a manual entry.
- Manual entries can be edited and deleted; fetched entries can be
  deleted.
- After adding a manual model, a `/models` refetch keeps it; an id
  collision keeps the manual one without duplicating.
- A manually-added model is selectable wherever provider models are
  offered.
- Empty/duplicate ids are rejected with a clear message.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` pass. Add a test for the merge/dedupe behavior
  (manual entry survives a refetch; manual wins on id collision).

## Constraints (non-negotiable)

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this
  prompt says otherwise (none are anticipated).
- Before wiring in any dependency, verify correct API/dependency usage
  with `kcl ask <package> "<question>"`.
