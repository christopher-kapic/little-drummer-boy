# Per-model & per-provider settings dialogs + `/model-settings` command

## Goal

Give the user an editable settings sub-dialog for the currently-active
model and for its provider, covering context-management thresholds,
cache TTL, cache mode, shrink strategy, and a per-scope defensive/normal
**mode** override. Add a `/model-settings` slash command (with a hidden
`/modelsettings` alias) that jumps straight to the active model's
settings. **Model-level settings override provider-level; provider-level
overrides the global fallback.**

## Current behavior

- `/settings → Providers → <providerId> → Edit` exposes URL, Headers,
  Models, Favorite, Refetch, Delete, Back
  (`src/tui/settings/providers.rs`, `EDIT_MENU_LEN = 7`).
- The Models sub-page (`ModelEditor`, `src/tui/settings/providers.rs`)
  lists models; **`Enter` on a manual model begins an id/name/context
  rename**, `a` adds, `d` deletes. Fetched (non-manual) models show
  "fetched models can't be edited".
- `ProviderEntry` (`src/config/providers.rs`) already carries
  `cache: CacheConfig` and `shrink: ShrinkConfig`; `ModelEntry` carries
  optional `cache: Option<CacheConfig>` and `shrink: Option<ShrinkConfig>`
  overrides. `CacheConfig` = `{ mode: none|ephemeral, ttl_secs (default
  300) }`. `ShrinkConfig` = `{ strategy: prune|compact, margin_secs
  (default 30) }`. **Neither has any UI today — they're JSON-only.**
- `ProvidersConfig::resolve_cache(provider, model)` /
  `resolve_shrink(provider, model)` already implement the
  model→provider override resolution; the driver calls them via
  `resolve_cache_config` / `resolve_shrink_config`
  (`src/engine/driver.rs`).
- Auto-prune (`maybe_auto_prune`, `src/engine/driver.rs`) fires before an
  inference call **only when the cache-cold predicate holds**
  (`prune::cache_state`, `src/engine/prune.rs`) and there's something
  prunable. There is **no auto-compact** today; `/compact` is manual.
- The live context figures (`ctx X% → Y% prunable`) already exist:
  `ContextProjection` carries `prunable_tokens`
  (`src/engine/agent.rs`), and ctx% is derived against the model's
  `context_length`.
- `LlmMode` (`Defensive` default | `Normal`, `src/config/extended.rs`)
  is a **global, persisted** setting edited in `/settings → UI`. It
  drives per-mode tool-description verbosity (`task`, `jobs`, `bash`)
  and the per-mode prompts. `/llm-mode` is a **live, session-only**
  toggle (`set_llm_mode`, `src/engine/driver.rs`) that rebuilds the root
  frame and does **not** write to disk.
- Slash commands are registered in the static array in
  `src/tui/app/mod.rs` (~L280–384) and dispatched in the match at
  ~L4416. `/settings` opens `Dialog::open(&cwd)`. The active model is
  `ProvidersConfig::active_model: Option<ActiveModelRef { provider,
  model, thinking_mode }>` in `config.json`.

## Desired behavior

### Navigation & commands

- In the Models sub-page, **rebind keys**: `r` renames the model
  (the current id/name/context editor); `Enter` / `l` / `→` opens the
  new **model settings** sub-dialog for the highlighted model.
- Add a **provider settings** entry to the provider Edit menu (e.g. a
  "Settings" row alongside Headers/Models) that opens the new
  **provider settings** sub-dialog.
- Add slash command `/model-settings` (description one sentence,
  token-economy §10) that opens the settings dialog navigated directly
  to the **active** model's model-settings sub-dialog. Register
  `/modelsettings` as a hidden alias resolving to the same handler (not
  shown in the slash menu). If no model is active, open to the providers
  list with an inline status explaining no model is selected.

### Settings (both dialogs edit the same field set)

Model-settings edits the `Option<…>` overrides on `ModelEntry`;
provider-settings edits the concrete values on `ProviderEntry`. The
fields:

1. **Auto-compact ctx %** — default **80**.
2. **Auto-prune ctx %** — default **50**.
3. **Auto-prune prunable %** — default **30**.
4. **Cache time (seconds)** — the existing `CacheConfig::ttl_secs`,
   default **300**.
5. **Cache mode** — the existing `CacheConfig::mode` (`none` |
   `ephemeral`). *(Previously JSON-only; now surfaced here.)*
6. **Shrink strategy** — the existing `ShrinkConfig::strategy` (`prune`
   | `compact`). *(Previously JSON-only; now surfaced here.)*
7. **Mode** — `defensive` | `normal` | `undefined` (default
   `undefined`).

The three new percentages (1–3) are **new config**. Add them to the
cache/shrink config layer in `src/config/providers.rs`: concrete values
on `ProviderEntry` (with the stated defaults) and `Option<…>` overrides
on `ModelEntry`, plus `resolve_*`-style accessors matching the existing
`resolve_cache`/`resolve_shrink` pattern (model override → provider
value → built-in default). Decide the exact struct grouping (e.g. a new
`ContextConfig`) yourself; keep serde defaults so older configs load.

### Override / resolution semantics

- **Percentages, cache, shrink:** effective value = model override →
  provider value → built-in default. (Cache/shrink resolution already
  exists; extend the same pattern to the percentages.)
- **Mode:** effective mode = model `mode` override → provider `mode`
  override → **the persisted global `llm_mode`** (extended-config).
  `undefined` at a scope means "inherit". There is **no new global
  setting** — the global fallback is the existing `llm_mode`, which
  stays editable in `/settings → UI` (persisted) and toggleable live via
  `/llm-mode` (session-only, unchanged).
- When the active model changes (session start or `/model`), re-resolve
  the effective mode from the override chain and apply it via the
  existing `set_llm_mode` machinery, so a model/provider that pins a
  mode takes effect. A live `/llm-mode` override applies to the current
  session context and is superseded when the active model changes.

### Auto-prune / auto-compact trigger behavior

Wire the new percentages into the inference-boundary logic
(`src/engine/driver.rs`), reusing the already-computed ctx% and
prunable% figures:

- **Auto-prune** fires when **either** the existing cache-cold predicate
  holds (unchanged — free pruning) **OR** (`ctx% > auto-prune ctx %`
  **AND** `prunable% > auto-prune prunable %`). The threshold branch may
  prune even when the cache is warm (accepting the cache bust to reclaim
  context); surface the same cache-break warning path the manual prune
  uses when that happens.
- **Auto-compact** fires when `ctx% ≥ auto-compact ctx %`. At that
  threshold, **always compact** (run the `/compact` summarization
  machinery automatically) — do not attempt a prune-first step for the
  compact trigger; the prune threshold (50–80 band) handles the cheaper
  reclaim below the compact line. Auto-compact is new wiring around the
  existing `/compact` brief machinery; fire it at the same safe
  inference boundary as auto-prune, and respect the same
  `at_safe_boundary` / watermark guards so it can't loop.

## Edge cases & UX decisions

- **Settings apply to fetched (non-manual) models too** — these are
  overrides, not edits to fetched data, so the model-settings dialog
  opens on any model. Only *renaming* (`r`) keeps its current
  manual-only restriction; `Enter`/`l`/`→` to settings works on every
  model.
- **No `context_length` known** for the active model ⇒ ctx% is
  uncomputable, so the ctx%-based threshold triggers (auto-compact and
  the threshold branch of auto-prune) are inert; the cache-cold
  auto-prune branch still works. Don't error — just skip the
  ctx%-gated paths.
- **Percentage validation:** clamp/validate to 0–100 on commit; reject
  non-numeric input inline (match the existing field-editor error UX).
  No cross-field constraint is enforced between the prune ctx% and
  compact ctx% — store whatever the user sets.
- **`mode` cycling:** the Mode field cycles `defensive → normal →
  undefined`; `undefined` serializes as absent (skip-if-none), so
  configs that never set it stay clean.
- Persist via the existing `save_config()` path; preserve unknown JSON
  fields (the `ConfigDoc` round-trip already does this — don't drop
  `extra`).

## Expected UX / acceptance

- `/model-settings` (and `/modelsettings`) opens directly on the active
  model's settings; editing a field and leaving the dialog persists it
  to `config.json`.
- In Providers → model list, `r` renames and `Enter`/`l`/`→` opens
  settings; provider Edit menu has a Settings row opening provider-level
  settings.
- A model override visibly takes precedence over the provider value,
  which takes precedence over the default, for every field including
  `mode` (verify mode resolution falls through to the persisted global
  `llm_mode` when both scopes are `undefined`).
- With a warm cache and ctx% > 50 & prunable% > 30, auto-prune fires
  (with the cache-break warning); at ctx% ≥ 80, auto-compact fires.
- Tests cover the resolution chain for the new percentages and `mode`,
  and the new trigger predicates (extend the existing
  `maybe_auto_prune` tests in `src/engine/driver.rs`).

## Notes

- The broader **defensive/normal-mode expansion** (runtime-computed tool
  descriptions, A/B harness) remains **deferred**
  (`deferred-prompts/defensive-normal-mode-expansion.md`). This prompt
  only adds the per-scope `mode` *override + resolution* on top of the
  existing `LlmMode` plumbing — do **not** build the runtime-description
  mechanism or A/B harness here.
- Follow the sub-page borrow pattern already used by Headers/Models
  (`Box<EditState>` parent, `Nav` enum for deferred navigation,
  `src/tui/settings/mod.rs`) rather than writing `self.page` directly.

## Constraints (mandatory)

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths. Every code path the feature
  introduces is complete and tested.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (No new
  dependency is anticipated — this is internal config + TUI work.)
- Honor the project token-economy rules (GOALS §10): one-sentence tool/
  command descriptions, noun-phrase param descriptions.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, and
  `cargo fmt --check` must all pass.
