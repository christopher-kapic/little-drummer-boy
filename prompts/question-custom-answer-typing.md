# Question dialog: custom-answer typing UX fixes

## Goal

Fix three issues with the "type your own answer" affordance in the TUI
question dialog (`src/tui/dialog/question.rs` rendering, key handling in
`src/tui/dialog/mod.rs`): the option label, the terminal cursor
position, and Esc behavior while typing. Plus a fourth, separate change:
how an *already-answered* question renders in the chat transcript after
the dialog closes (see "Answered-question transcript rendering" below).

## Current behavior

For select / multiselect questions with `allow_freetext`, the dialog
renders a custom-answer row below the options:

- **Label.** Empty → `Type your own answer`. Non-empty → `Type your own
  answer: <typed>` (the placeholder stays prefixed in front of what the
  user typed).
- **Cursor.** When typing in the custom field, the real terminal cursor
  is parked using a byte-length-based prefix calculation
  (`2 + marker.len() + CUSTOM_LABEL.len() + ": ".len() + cursor_col`).
  The marker glyph (`✎ `) and the hover glyph (`▸ `) are multi-byte
  UTF-8, so `.len()` overcounts and the cursor lands to the right of the
  actual text.
- **Esc.** `Esc` always returns `DialogOutcome::Cancel`, dismissing the
  whole dialog — even mid-typing in the custom field.

## Desired behavior

1. **Label is replaced by the typed text.** While the user is typing a
   custom answer, the row shows only what they typed (with the edit
   marker), not the `Type your own answer:` prefix. When the field is
   empty, fall back to the `Type your own answer` placeholder. I.e. the
   placeholder and the typed text are mutually exclusive — never shown
   together.

2. **Cursor lines up with the typed text.** The parked terminal cursor
   must sit exactly at the caret position within the typed text.
   Compute the prefix in **display columns** (not bytes) — account for
   the hover/cursor glyph and the marker by their rendered width, and
   add the caret's column within the typed string (also measured in
   display columns, so multi-byte / wide input stays aligned). The
   cursor must visually coincide with the character the user is about to
   edit.

3. **Esc exits typing mode before cancelling (uniform rule).** When the
   user is in typing mode, `Esc` leaves typing mode and keeps the dialog
   open (`DialogOutcome::Continue`); it does **not** cancel. When the
   user is *not* in typing mode, `Esc` cancels the dialog as it does
   today. This is a single escalation rule applied everywhere:
   - On a select / multiselect custom field: first `Esc` defocuses the
     field (back to navigating options), a second `Esc` cancels.
   - On a pure freetext-only question (which opens directly in typing
     mode): first `Esc` exits typing mode, a second `Esc` cancels. The
     field re-focuses on the next text-affecting keystroke (this matches
     the existing "any key resumes editing" path for text pages).

## Answered-question transcript rendering (separate change)

After a question dialog is resolved, the answered question currently
appears in the chat transcript as a generic `ToolBox` for the `question`
tool call (the live dialog and the confirm/review page are separate from
this). Replace that transcript rendering with a dedicated two-line
format, **per question** in the set:

```
Question: "Example question?"
| User's answer
```

- **Line 1:** the literal label `Question:` in **bold**, followed by the
  question prompt wrapped in double quotes, in white. (`Question:` bold,
  the quoted prompt unbold/white.)
- **Line 2+:** the answer(s), grey/muted, each prefixed with `| `.
- **Multiple answers** (multi-select with several picks, or a freetext
  answer alongside picks): render **one `| answer` line per answer**, all
  grey — do not comma-join.
- **Multi-question sets:** repeat the block (`Question:` line + its
  `| answer` line(s)) once per question, in order.
- **Dismissed / cancelled questions:** use the **same format** — the
  `Question: "..."` line followed by a single grey `| (dismissed)` line —
  so cancelled questions read consistently with answered ones.
- **Scope:** this changes only the **TUI transcript display**. Leave the
  model-facing tool-result text (`render_answers` in
  `src/tools/question.rs`) unchanged.

## Edge cases & UX decisions

- **Scope: apply all three changes to both single-select and
  multiselect** custom-answer fields. They share the same rendering /
  cursor / typing-state code; keep behavior consistent across both — do
  not fork a single-select-only path.
- **Preserve typed text on Esc.** Exiting typing mode via `Esc` must
  keep the already-typed text intact (Esc defocuses; it does not clear
  the field). Re-entering typing mode resumes from the same text and
  caret.
- **Empty field reverts to placeholder.** If the user deletes all typed
  characters, the row shows `Type your own answer` again.
- **Single-select mutual exclusivity is unchanged.** Typing a custom
  answer still clears any selected radio option (existing behavior);
  this task does not alter that.

## Expected UX / acceptance

- Typing `hello` into the custom field shows a row reading `hello` (with
  the edit marker), not `Type your own answer: hello`; the terminal
  cursor sits immediately after `hello` (and correctly between
  characters when the caret is moved left/right, including multi-byte
  input).
- Clearing the field shows `Type your own answer` again.
- While typing, `Esc` stops the cursor/typing and returns focus to the
  option list (or defocuses the freetext field) without closing the
  dialog; pressing `Esc` again closes it.
- An answered question shows in the transcript as a bold `Question:`
  line with the prompt quoted in white, and the answer(s) on grey `| `
  lines below (one per answer); a dismissed question shows
  `| (dismissed)`; the model-facing tool result is unchanged.
- Existing tests in `src/tui/dialog/question.rs` still pass; add tests
  covering: label-replacement on type, Esc-exits-typing-then-cancels
  (both select and freetext-only pages), that typed text survives an Esc
  round-trip, and the answered/dismissed transcript rendering (single,
  multi-answer, and multi-question cases).

## Constraints

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (For display-width
  measurement, prefer a crate already in the tree — check `unicode-width`
  / what `ratatui` re-exports before adding anything new.)
- Token-economy and TUI-chrome design rules in `CLAUDE.md` still apply;
  do not change the fixed chrome or footer-hint contract beyond what
  these three fixes require.
