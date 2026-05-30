# Add `/copy` slash command — copy the last response to the clipboard

## Goal

Add a `/copy [format]` command that copies the last assistant response
to the clipboard in a chosen format.

## Current behavior

- Clipboard support already exists (`src/clipboard/mod.rs`, crate
  `arboard` + manual OSC52 for SSH):
  - `copy_plain(text)` — OSC52 first (works over SSH), falls back to
    arboard locally.
  - `copy_rich(plain, html)` — arboard multi-format; returns
    `UnsupportedOverSsh` when rich copy can't work.
  - `markdown_to_html(md)` / `markdown_to_plain(md)` — convert markdown
    (via `pulldown_cmark`).
  - `is_ssh()` for SSH detection.
- The TUI context menu already exposes "Copy as rich text" / "Copy as
  plain"; there is no `/copy` slash command.

## Desired behavior

- Register a `/copy [format]` slash command that copies the **last
  assistant response** (the message text; exclude tool-call chrome).
- Format argument and aliases:
  - `markdown` — **default** (bare `/copy` == `/copy markdown`). Copies
    the raw response text *with* its markdown notation, verbatim. Uses
    `copy_plain`.
  - `plain` / `plaintext` — markdown stripped to plain text via
    `markdown_to_plain`, then `copy_plain`.
  - `rich` / `richtext` — rich/HTML copy via `markdown_to_html` +
    `copy_rich`.
- After copying, show a brief confirmation (e.g. "Copied last response
  (markdown)").

## Edge cases & UX decisions

- **Rich over SSH:** `copy_rich` returns `UnsupportedOverSsh`. In that
  case, fall back to copying plain text and tell the user rich copy
  isn't available over SSH (so `/copy rich` never silently does
  nothing).
- **No response yet:** if there is no assistant response to copy, show a
  message and do nothing.
- **Unknown format arg:** show usage listing the valid formats; copy
  nothing.

## Acceptance

- `/copy` copies the last response as raw markdown; `/copy plain` copies
  stripped text; `/copy rich` copies rich text (falling back to plain
  with a notice over SSH); each shows a confirmation.

## Constraints

Implement without incurring tech debt — no shortcuts, no TODO-for-later,
no half-finished paths. For any new package use the latest stable
release unless this prompt says otherwise, and verify correct
API/dependency usage with `kcl ask <package> "<question>"` before wiring
it in. Slash-command descriptions are one sentence (token economy,
CLAUDE.md).
