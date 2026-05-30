# `/context` — visual context-window usage overlay

## Goal

Add a `/context` slash command that opens a dismissable TUI overlay
showing how the current context window is filled, broken down by
category, with a single color-segmented bar and a legend. The point is
to let the user *see with color* where their context budget is going.

## Current behavior

The TUI chrome already carries a fixed context indicator (GOALS §1a).
There is no detailed/expanded view of what makes up the context.

## Desired behavior

- New slash command `/context` (registered in the slash menu) opens an
  overlay dialog over the TUI, dismissable like other dialogs (Esc /
  the existing dialog-close path).
- The overlay renders, for the **current live context** at the moment
  it is invoked (a snapshot, not live-updating):
  1. A header line with total usage: `<used> / <window> (<pct>%)`,
     where counts use compact k-notation (e.g. `89.2k / 1M (9%)`).
  2. **One full-width horizontal bar** split into colored segments,
     each segment sized proportionally to a category's token share of
     the **whole window** (so unused budget shows as a trailing
     free-space segment).
  3. A **legend** below the bar: one entry per category, showing the
     category's color swatch/glyph, its name, and its token count.

Reference look (final glyph/color choices are yours, see Notes):

```
Context  ████████▓▓▓▓▓▒▒▒░░░░░░░░░░░░░░░░░░░░  89.2k / 1M  (9%)

  █ system 17.3k   ▓ tools 14.0k   ▒ msgs 63.1k   ░ free 910.8k
```

### Categories

Show **category-level breakdown only** — do not itemize individual
skills / MCP tools / memory files. Derive the authoritative category
list from wherever cockpit actually assembles the outgoing context;
the expected set is roughly:

- system prompt (base prompt)
- tool schemas
- cached system block (sysinfo / git / cwd block)
- skills
- MCP catalog (lazy-discovery catalog text, if any)
- memory / guidance files (CLAUDE.md, MEMORY.md, etc.)
- messages (conversation history)
- free space (remainder of the window)

Map these to the real context-assembly code rather than hardcoding a
guessed list — if cockpit composes context differently, follow the
code and use those real buckets.

### Token counting & window size

- Count tokens the same way cockpit counts them elsewhere: use the
  provider's own counter when available, else the `tiktoken-rs`
  cl100k_base fallback (`tokens.rs`).
- The window size is the active model's context limit from its provider
  config.

### Color

Color is the feature — each category segment gets a **distinct color
from the active TUI theme palette**, and the legend maps color →
category. Free space renders in a muted/dim color. Don't rely on glyph
shape alone to distinguish categories; the color carries it (the bar
may use a single solid block glyph colored per segment, with the legend
swatch matching).

## Edge cases & UX decisions

- **Bar fill is exact.** Round per-segment widths so the segments
  always sum to the full bar width (largest-remainder / equivalent) —
  no gap or overflow at the right edge.
- **Zero-token categories** are omitted from the legend (don't clutter)
  but still counted toward totals.
- **Unknown window size** (model with no known context limit): show
  absolute used-token counts and omit the percentage and the free-space
  segment rather than faking a denominator.
- **Narrow terminals:** the bar adapts to the available overlay width;
  pick a sane minimum width and degrade gracefully (don't panic or
  overflow).

## Expected UX / acceptance

- Typing `/context` opens an overlay with the segmented colored bar,
  the header total, and the per-category legend with token counts.
- Segment sizes and colors visibly reflect the real current context
  composition; the bar fills the full width exactly.
- The overlay dismisses cleanly and returns focus to the composer.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (A new dep is
  likely unnecessary here — ratatui + the existing dialog/theme
  infrastructure should suffice.)
- Honor the token-economy and fixed-chrome design rules: this is an
  on-demand overlay, not added to the always-on chrome.

## Notes

- Visual style, invocation (slash command + overlay), detail level
  (category-only), and the command name (`/context`) are all settled
  per the user.
- Final glyph and exact theme-color assignments per category are left
  to you, subject to the "distinct color per category, muted free
  space" rule above.
