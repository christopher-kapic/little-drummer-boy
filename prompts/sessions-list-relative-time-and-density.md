# `/sessions`: relative "time ago" + tighter list density

## Goal

In the `/sessions` browser, make it easier to gauge session recency and
fit more sessions on screen: (1) show a relative "X ago" string alongside
the existing absolute timestamp, and (2) remove the blank gap between
session entries.

## Current behavior

- Each session is a bordered multi-row card; a blank line separates the
  cards (`src/tui/sessions_pane.rs`, `body_lines` ~656 pushes a blank
  `Line`).
- The timestamp is absolute only: `fmt_time` formats `last_active_at` as
  `%Y-%m-%d %H:%M` (`src/tui/sessions_pane.rs` ~856).
- No relative-time helper exists in the codebase. `chrono` 0.4 is already
  a dependency; `chrono-humanize` is not.

## Desired behavior

### Density

Remove the blank separator line between session cards so entries sit
directly adjacent. Keep the existing bordered card layout — do **not**
collapse to a one-line-per-session redesign. No other structural change to
the card.

### Relative time (in addition to the absolute datetime)

Show the relative string **and** the existing absolute datetime, relative
first, e.g.:

```
5 minutes ago · 2026-05-29 14:32
```

(Use a middle dot `·` or similar separator.) Keep `last_active_at` as the
source field, same as today.

### Relative bucketing — hand-rolled, exact spec

Do **not** use `chrono-humanize`; its bucketing doesn't match this spec
(it would, e.g., turn 47h into "2 days"). Compute from elapsed duration
`d = now - last_active_at` using `chrono` (already present). Buckets:

- `d < 1 minute` → `just now`
- `1–59 minutes` → `N minute(s) ago`
- `1–47 hours` → `N hour(s) ago` (hours run up to 47; switch to days at
  48h — do NOT switch to days at 24h)
- `2–29 days` (i.e. `48h ≤ d < 30 days`) → `N day(s) ago`
- `1–11 months` (`30 days ≤ d < 365 days`) → `N month(s) ago`, using
  30-day months (`floor(days / 30)`)
- `d ≥ 365 days` → `N year(s) ago`, using 365-day years

Rules:
- Correct singular/plural ("1 minute ago" vs "2 minutes ago").
- These are coarse approximations (30-day months, 365-day years) — exact
  calendar math is not required; keep it simple.
- Clamp future timestamps (clock skew, `d < 0`) to `just now`.

Apply this everywhere a session timestamp is shown in the browser.

## Acceptance

- The list is denser — no blank line between entries.
- Each session shows `<relative> · <absolute datetime>`.
- Bucket boundaries match the spec: minutes up to 59, hours up to 47, days
  up to a month, months up to a year, then years.

## Constraints

- Implement without incurring tech debt: no shortcuts, no TODO-for-later,
  no half-finished paths.
- No new dependency — hand-roll the formatter using the existing `chrono`.
  (If you believe a crate is warranted, it must use its latest stable
  release and you must verify API usage with `kcl ask <package>
  "<question>"` first — but the expectation is no new dep.)
- Gates must pass: `cargo build`, `cargo test`, `cargo clippy -- -D
  warnings`, `cargo fmt --check`.
