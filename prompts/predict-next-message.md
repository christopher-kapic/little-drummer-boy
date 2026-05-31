# Predict next message (utility-model input ghost-text)

## Goal

After each agent turn, use the utility model to predict what the user
is likely to type next, and offer it as **ghost text** (grey) in the
composer's input box. When the input is empty, the prediction shows in
grey; the user presses **Tab** to accept it as real, editable white
text they can then edit and send. Controlled by a new
`predict next message` setting with three values: `off`, `short`
(default), `long`.

## Current behavior

- The composer (`tui/`, vim mode default-on) renders user input; there
  is no predictive/ghost-text affordance.
- A utility model already exists and is used for ancillary tasks
  (e.g. `auto_title.rs`, the utility-model safety/translation/injection
  gates). Outbound prompts go through `redact::scrub()` (non-bypassable).
- `/settings` exposes configurable options (`tui/settings/`,
  config under `config/`).

(Orientation only — verify against the tree, don't treat as a spec:
composer rendering + key handling in `src/tui/`, settings UI in
`src/tui/settings/`, the utility-model call path used by
`src/auto_title.rs`, config in `src/config/`.)

## Desired behavior

1. **Setting.** Add `predict next message: off | short | long`, default
   `short`. Configurable in `/settings` alongside the other options.
   - `off` — feature disabled, no utility call, no ghost text.
   - `short` — prediction is **1 line or less**.
   - `long` — prediction is a **full proposed response** (may be
     multi-line).

2. **Prediction input.** When a prediction is generated, feed the
   utility model the **last 3 turns** of conversation — **only the
   user's input and the agent's final response per turn; no tool calls,
   no intermediate reasoning**. The model returns the predicted next
   user message. The prompt to the utility model goes through
   `redact::scrub()` like every outbound prompt.

3. **Lifecycle — eager, regenerates on clear.** Generate a prediction
   automatically when each agent turn ends. While the input box is
   empty, display the prediction as grey ghost text. When the user
   starts typing, the ghost text disappears. If the user clears the box
   back to empty (same turn, prediction unchanged), show the cached
   prediction again — **do not** issue a new utility call for the same
   turn. A new prediction is computed on the next agent turn.

4. **Accept — Tab, insert mode only.** Tab accepts the ghost text; it
   is active **only in vim insert mode** and only when the box is empty
   with a pending ghost prediction. In vim normal mode, Tab keeps its
   existing behavior (the prediction does not interfere). Accepting
   **fills** the input with editable real (white) text — it does **not**
   auto-send; the user edits and sends normally.

5. **Display & the two-stage `long` expansion.**
   - Ghost text appears **once ready** (no streaming, no partial
     render, no flicker) when the prediction completes.
   - `short`: the prediction is ≤1 line; one Tab converts it directly
     to real editable text.
   - `long`, prediction is a single line: behaves like `short` — one
     Tab converts to real text.
   - `long`, prediction is **longer than one line**: initially show
     only the **first line** as ghost text (box stays single-line
     height). **First Tab** expands the input box and reveals the
     **whole** proposed response, **still as ghost text**. **Second
     Tab** converts the full response to real editable text.

## Edge cases & UX decisions

- **No agent response yet** (fresh session, before the first agent
  turn): nothing to predict — no ghost text, no utility call.
- **`off`** issues no utility call at all and renders no ghost affordance.
- **User typing wins.** Any keystroke that puts content in the box
  hides the ghost; the prediction never overwrites what the user typed.
  Tab-to-accept is only meaningful while the box is empty.
- **Stale prediction.** A prediction belongs to the turn it was
  generated for. Once a new agent turn completes, replace it; never
  show a prediction from a prior turn.
- **In-flight prediction.** If the utility call hasn't returned yet,
  show no ghost text (appear-once-ready); if the user starts typing
  before it lands, discard the result silently — no popping in over
  active input.
- **Token economy (priority #2).** This adds a utility call per agent
  turn when enabled — keep the input minimal (last 3 turns, final
  responses only, no tool calls), and cap the predicted output to what
  the mode needs (`short` ≈ one line; `long` a bounded full response,
  not unbounded). Don't recompute on clear-and-retype within the same
  turn.
- **Redaction is non-bypassable** — the prediction prompt goes through
  `redact::scrub()`; no per-call opt-out.
- **Applies regardless of primary agent** (`Auto`/`Plan`/`Build`) —
  the prediction is about the user's next message, not the agent.

## Expected UX / acceptance

- With `short` (default): after the agent replies, an empty input box
  shows a ≤1-line grey suggestion; Tab in insert mode turns it white
  and editable; typing dismisses it; clearing back to empty restores it
  without a new model call.
- With `long` + multi-line prediction: empty box shows the grey first
  line; first Tab expands the box and shows the full grey response;
  second Tab makes it editable; the user can then edit and send.
- With `off`: no ghost text, no utility call, behavior unchanged.
- `/settings` shows `predict next message` with the three values and
  `short` as default; changing it takes effect for subsequent turns.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` all pass. Add tests covering: setting parse +
  default, prediction-input assembly (last 3 turns, responses only, no
  tool calls), lifecycle (eager generate, hide-on-type, restore-on-clear
  without re-call), and the `long` two-Tab expansion vs `short`/single-
  line one-Tab path.

## Constraints (always)

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (No new runtime
  deps requiring `node`/`bun`/`deno`.)
- Honor the priority order in CLAUDE.md: correctness/defensiveness
  first, token economy second, speed third. Keep tool/setting
  descriptions one sentence and parameter descriptions noun-phrases.
- Update the design docs (GOALS.md / plan.md and any relevant section)
  to reflect this feature before/with the code, per "update the docs
  first; then code."

## Notes

- Decisions baked in from the requester: setting `off|short|long`,
  default `short`; prediction input = last 3 turns, user input + agent
  final response only (no tool calls); eager generation per turn,
  regenerate-on-clear from cache (no re-call same turn); accept = Tab,
  insert mode only, fills-not-sends; appear-once-ready (no streaming);
  `long` multi-line = first-Tab expands box to full ghost response,
  second-Tab converts to real (single-line `long` and `short` are
  one-Tab convert).
