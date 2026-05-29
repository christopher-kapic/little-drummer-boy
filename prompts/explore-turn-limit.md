# Raise noninteractive agent turn limits (explore 64, docs answerer 64, docs resolver 24)

## Goal

Replace the single shared turn cap on noninteractive subagents with
per-role limits. Explore is hitting the current ceiling of 12 turns
too early; 12 is far too few for real exploration work.

## Current behavior

`src/engine/driver.rs` defines one constant,
`MAX_NONINTERACTIVE_TURNS: usize = 12`, used as the loop bound inside
`run_noninteractive`. That single function (and therefore that single
cap) serves three distinct roles:

- **explore** — spawned via the `SpawnNoninteractive` path in the
  driver (`run_noninteractive` call near `driver.rs:1535`).
- **docs resolver** (Docs.1) — `run_noninteractive` call in
  `src/engine/docs_pipeline.rs` (~line 71).
- **docs answerer** (Docs.2) — `run_noninteractive` call in
  `src/engine/docs_pipeline.rs` (~line 113).

All three are currently bounded at 12. The over-limit error message
(`driver.rs:1819`) interpolates the shared constant.

## Desired behavior

Give each role its own fixed turn limit:

| Role          | New limit |
|---------------|-----------|
| explore       | 64        |
| docs answerer | 64        |
| docs resolver | 24        |

The limit must be passed per invocation rather than read from one
global constant. Thread a turn-limit parameter into
`run_noninteractive` and have each call site pass its role's value.
Define the three values as named constants (no magic numbers at the
call sites). The over-limit error message must report the *actual*
limit that was exceeded, not a hardcoded one.

## Edge cases & UX decisions

- **Fixed constants, no config surface.** Do not add a config key,
  setting, or CLI flag — these are compile-time constants. (Settled
  decision; do not add configurability.)
- The change is purely the loop bound and its error message. Do not
  alter what happens when the limit *is* hit (same termination /
  error behavior as today, just with the corrected number).
- Any other caller that reaches `run_noninteractive` and isn't one of
  the three roles above must still get an explicit, sensible limit
  passed in — no call site may rely on a removed global default.
  Choose the value that matches the role's intent and name the
  constant accordingly; if a genuinely generic fallback is needed,
  document it in a code comment.

## Acceptance

- `run_noninteractive` no longer reads a single shared
  `MAX_NONINTERACTIVE_TURNS` for its loop bound; the bound is a
  parameter.
- explore runs up to 64 turns; docs answerer up to 64; docs resolver
  up to 24.
- The over-limit error message prints the limit actually in force for
  that invocation.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, and
  `cargo fmt --check` all pass.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths. Update the doc comment on
  `run_noninteractive` (it currently cites the shared constant) so it
  matches the new per-role design.
- For any new package use the latest stable release unless this prompt
  says otherwise (none expected here), and verify correct
  API/dependency usage with `kcl ask <package> "<question>"` before
  wiring it in.
