# Richer TUI display for noninteractive subagent delegations

## Goal

Make a noninteractive subagent delegation (explore, and any other
delegation surfaced via the subagent spawn/report events) render as a
live, informative block in the TUI scrollback instead of two terse
lines. While the subagent runs, show a live elapsed timer with animated
ellipses; when it returns, show how long it worked plus its response.

## Current behavior

In `src/tui/app/mod.rs` (~lines 2160-2175):

- `TurnEvent::SubagentSpawned` pushes a plain line:
  `[{parent} → {child}]: {prompt-preview}`.
- `TurnEvent::SubagentReport` pushes a plain line:
  `{agent} returned to caller.`

No elapsed time, no live animation, no response body, no color.

## Desired behavior

**While the subagent is running** — a single live line:

```
{parent} delegated to {child}... (1m 0s)
```

- The `...` is animated (ellipsis animation), and the `(elapsed)` timer
  ticks live, reusing the same animation/tick mechanism the main
  agent's "working" span already uses.
- The child (subagent) name is rendered in **orange**.
- **Drop the delegation prompt preview** — the running line shows only
  "{parent} delegated to {child}", not the prompt text.

**Once the subagent reports** — the running line is replaced by a
header plus the response:

```
{child} worked for 2m 10s
| Based on my exploration of this project...
| ...and more stuff
```

- Header: `{child} worked for {duration}`, child name in **orange**.
- The response body renders through the existing markdown renderer,
  **tinted light grey** (use the existing grey used elsewhere in the
  chrome/banner). It sits in a quoted/indented block with a left `|`
  bar.
- The response is **truncated to a few leading lines with an expand
  affordance** (`… (expand)` or equivalent), consistent with how other
  collapsible tool output already works in the history view. Expanding
  reveals the full report.

### Agent-name casing & color

Render each agent's name **verbatim as registered**, honoring the
project casing convention (primary agents Capitalized — `Build`,
`Plan`; subagents lowercase — `explore`, `docs`). So a real line reads
`Build delegated to explore... (1m 0s)` and `explore worked for 2m 10s`.
The mock above capitalized "Explore" only illustratively — use the
actual lowercase subagent name. Only the **subagent (child)** name is
orange; the parent name uses the default style.

### Duration format

Compact: `2m 10s`, and `45s` when under a minute. Match the `1m 0s` /
`2m 10s` shape in the mock.

## Edge cases & UX decisions

- **Elapsed timing** is measured from the spawn event to the report
  event; the final "worked for" duration is that total.
- **Failure / error:** if the delegation ends in error rather than a
  normal report, replace the running line with an analogous failure
  header (`{child} failed after {duration}`, child name orange) instead
  of leaving a dangling animated "delegated…" line. Keep the duration.
- **Empty response:** if the report body is empty, show the
  `{child} worked for {duration}` header with no quoted block.
- This applies to **all** noninteractive subagent delegations surfaced
  via the subagent spawn/report events, not explore alone.

## Acceptance

- During a delegation, the scrollback shows one live line with animated
  ellipses and a ticking timer, subagent name in orange.
- After the subagent returns, that line becomes
  `{child} worked for {duration}` (orange name) followed by the
  light-grey, markdown-rendered, left-bar-quoted response, truncated
  with a working expand affordance.
- The old `[parent → child]: prompt` and `agent returned to caller.`
  lines are gone.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, and
  `cargo fmt --check` all pass.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths. If an orange color isn't
  already defined in the theme, add one named constant in the theme
  module rather than scattering a raw color index at the call site.
- For any new package use the latest stable release unless this prompt
  says otherwise (none expected here — ratatui/pulldown-cmark are
  already in the stack), and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
