# Question dialog UX overhaul

## Goal

Rework cockpit's interrupt/question TUI dialog so it's faster and less
clunky to interact with, borrowing proven interaction ideas from codex's
selection/approval widgets. This is a UX overhaul of the existing
question dialog — not a rewrite of the interrupt protocol or the agent
contract.

## Where this lives

- `src/tui/dialog/question.rs` — question dialog (the main target).
- `src/tui/dialog/mod.rs` — the reusable `DialogState` state machine.
- `src/tui/dialog/approval.rs` — approval dialog; shares the same core.
- `src/daemon/proto.rs` — `InterruptQuestion`, `InterruptOption`,
  `ResolveResponse` wire types.
- `src/tools/question.rs` — the `question` tool that raises interrupts.
- `src/tui/app/` — dialog integration / render wiring.

The `DialogState` core is shared with the approval dialog. Changes must
not regress the approval dialog; where an improvement applies cleanly to
both (e.g. number-key select, scrolling, visible cursor), apply it to
the shared core so both benefit. The focus and acceptance bar is the
question dialog.

## Current behavior

- Fullscreen-ish modal (`DIALOG_HEIGHT = 18` reserved) replacing the
  composer.
- Multi-question wizard: pages walk through questions, with a final
  confirm/review page; a single question fast-paths to submit.
- Options are `{id, label}` (label only); markers `(•)/( )` (single),
  `[x]/[ ]` (multi), `▸` cursor.
- Select via navigate (j/k/arrows) then Enter. Space toggles / enters
  typing mode. A "Type your own answer" affordance is always last.
- Anti-misfire lockout: grey border + input ignored for ~1.5s after open.

### Pain points this overhaul must fix (settled)

1. **Fullscreen feels heavy** — see desired size below.
2. **No visible text cursor** when typing a free-text answer.
3. **Text-only (freetext) questions require pressing space/enter first**
   before you can type.
4. **After Enter you must manually navigate to the next question** —
   advancing should be automatic.
5. **In a single-select, typing a custom answer doesn't deselect the
   previously chosen radio option** — the two must be mutually exclusive.

## Desired behavior (all settled — implement exactly)

### Layout & placement
- **Not fullscreen.** Compact, **bottom-anchored** overlay (replacing /
  sitting above the composer, codex bottom-pane style). Size to content
  with a max height; when content exceeds the max, scroll (see below).

### Selection & navigation
- **Number-key instant-select.** Pressing `1`–`9` targets that option.
  - Single-select: selects the option **and advances** to the next
    question (instant-accept).
  - Multi-select: **toggles** that option (no advance).
- **Single-select**: Enter on the focused option selects it and
  **auto-advances** to the next question (no manual navigation). The last
  question advances to the review page; a lone question fast-paths to
  submit.
- **Multi-select**: follow the pattern of this assistant's own
  multi-select UI — **Enter toggles the focused option**, and there is an
  explicit **"Next"** entry at the bottom of the option list; focusing it
  and pressing Enter advances to the next question. Multi-select never
  auto-advances on a toggle.
- **Freetext questions open directly in typing mode** (immediately
  editable once the lockout clears) — no space/enter needed to start.
  Enter submits/advances.
- **Visible text cursor** wherever the user types (freetext questions and
  the "type your own answer" field) — a clear caret/block at the input
  position.
- **Single-select custom answer is mutually exclusive with the options**:
  typing into "type your own answer" deselects any chosen radio option,
  and selecting a radio option clears custom text.

### Long option lists
- Cap visible option rows and **scroll**, keeping the focused row in view
  (codex caps at 8 — match that or pick a sensible cap), instead of
  clipping. Wire this into the shared scroll state so multi-line rows
  count correctly toward height.

### Per-option descriptions (schema change)
- Add an **optional** `description: Option<String>` to `InterruptOption`
  in `proto.rs`, and expose it in the `question` tool's input schema so
  the agent can supply a one-line description per option. Backward-
  compatible: options without a description render exactly as today.
- Render the description in a second column (or wrapped/indented under the
  label, codex-style), dimmed. Continuation lines align under the label
  column.

### Context header
- cockpit's interrupt carries a top-level `description` (from
  `raise_interrupt(description, question?)`) separate from each question's
  prompt. Render that interrupt `description` as an **italic/muted context
  block above the question prompt**, like codex's `Reason:` header. Omit
  the block when there's no description.

### Review page (kept)
- Keep the final confirm/review page that lists all questions with their
  answers before submit. Questions auto-advance into it; it still flags
  unanswered questions and gates submit on completeness. A single
  question keeps fast-pathing straight to submit.

### Anti-misfire lockout (kept)
- Keep the ~1.5s lockout (grey border, input ignored) on open. Once it
  clears, freetext questions are immediately editable per above.

## Out of scope / skip
- Per-option fixed letter shortcut hints (`(y)`, `(p)`) — cockpit's
  options are dynamic/agent-supplied; number keys cover quick-select.
- The specific marker glyph and color are at the agent's discretion;
  stay consistent with cockpit's existing dialog styling (`theme.rs`).

## Acceptance / observable end state
- Question dialog renders compact and bottom-anchored, sized to content,
  scrolling when option lists are long.
- Freetext question: typeable immediately after lockout, with a visible
  cursor; Enter advances.
- Single-select: a number key (or Enter on focused) selects and jumps to
  the next question; typing a custom answer clears the radio choice and
  vice-versa.
- Multi-select: Enter toggles options; a "Next" button advances; number
  keys toggle.
- Options with a `description` show it dimmed beside/under the label;
  options without one are unchanged.
- Interrupt `description` shows as an italic/muted context header above
  the prompt.
- Review page still gates submit on all questions being answered.
- Approval dialog still works (no regression from shared-core changes).
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` all pass.

## Constraints (non-negotiable)
- Implement **without incurring tech debt** — no shortcuts, no
  TODO-for-later, no half-finished paths. The proto schema change must be
  threaded through end to end (tool schema → wire type → dialog render →
  resolve response).
- For any new package, use the **latest stable release** unless this
  prompt says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (`ratatui`,
  `crossterm` already in tree — verify current cursor/scroll APIs via
  `kcl ask ratatui "…"` if unsure.)
- Honor cockpit's design rules in `CLAUDE.md`: wire-vs-user transcript
  split, redaction chokepoint, token economy. This change is TUI-side;
  don't alter the agent-facing tool description beyond adding the optional
  `description` field (keep it a noun-phrase, one line).
