# Providers dialog: "refetch all" button at the top

## Goal

Add a button at the top of the /settings → Providers list page that
refetches `/models` from all configured providers at once, surfacing
the existing all-providers fetch flow as a first-class affordance.

## Current behavior

- The Providers list page (`src/tui/settings/providers.rs`,
  `ProvidersPage::List`) shows configured providers and an "add new"
  affordance. Per-provider refetch exists only on the Edit page (`r`).
- An all-providers fetch already exists: the `/fetch-models` slash
  command drives `ProvidersPage::FetchAll(FetchAllState)`, which spawns
  one async `FetchHandle` per provider (`src/tui/settings/auth.rs`),
  polled on `tick()`, continuing past per-provider failures and
  surfacing per-provider status.
- There is **no** button in the providers UI itself to trigger the
  all-providers fetch.

## Desired behavior

Add an actionable item/button at the **top of the Providers list page**
(e.g. `[refetch all models]`) with a key binding. Activating it
triggers a fetch of `/models` for **all** configured providers,
reusing the existing all-providers fetch machinery
(`FetchAll`/`FetchHandle`) rather than reimplementing it.

- Fetch runs **asynchronously in the background** (existing
  `FetchHandle` pattern); the UI stays responsive and shows progress.
- **Continue on per-provider error:** one provider failing (missing
  env/credentials, HTTP error) does not abort the others. Per-provider
  outcomes (success count / failure reason) are surfaced the same way
  the existing flow does.
- Results persist into each provider's `models` list exactly as the
  current fetch paths do.

## Edge cases & UX decisions (settled)

- **No providers configured:** the button is shown but activating it is
  a no-op with a brief "no providers configured" status; do not error.
- **Fetch already in flight:** activating again while a refetch-all is
  running should not spawn a duplicate run — ignore or restart cleanly
  using the existing in-flight tracking, never stack concurrent
  all-fetches.
- Reuse the existing `FetchAll` flow/state and its rendering of
  progress and per-provider results; do not build a parallel
  fetch-all implementation.

## Expected UX / acceptance

- The Providers list page shows a clearly labeled refetch-all button at
  the top with a key hint.
- Activating it fetches all providers' models in the background, shows
  progress, and reports per-provider success/failure without blocking
  the UI; one failure doesn't stop the rest.
- The same behavior remains reachable via the existing `/fetch-models`
  command (no regression).
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` pass.

## Constraints (non-negotiable)

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths. Reuse the existing fetch-all
  path; do not duplicate it.
- For any new package, use the latest stable release unless this
  prompt says otherwise (none are anticipated).
- Before wiring in any dependency, verify correct API/dependency usage
  with `kcl ask <package> "<question>"`.
