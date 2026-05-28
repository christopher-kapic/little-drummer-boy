# Judgement calls for review

## Task: live "agent is working" status indicator (`prompts/status-indicator-message.md`)

1. **Working-message strings stored without trailing `...`.** The
   prompt's *Animation* section said to "strip that trailing `...`",
   but the *Working-message list* note clarified the stored strings
   carry no `...` and the animated ellipsis is appended at render time.
   These two statements conflict; I followed the list note — the const
   `WORKING_MESSAGES` (in `src/tui/app/mod.rs`) holds the lines verbatim
   with no `...`, and render appends the width-3 padded ellipsis. Also
   applied the prompt's noted typo fix: "You were the chosen one".

2. **Submit queue-decision now gates on `busy`, not `pending.is_some()`.**
   The prompt explicitly asked only that the *grey border* track the new
   `busy` state. I also switched `submit_input` (`src/tui/app/input.rs`)
   from `pending.is_some()` to `self.busy` for the "queue vs. render-now"
   decision, because that is exactly the "fresh submit vs. message folded
   into an in-flight turn" distinction the span clock needs — and it
   fixes the pre-existing case where a message typed *during tool
   execution* (when `pending == None`) was misclassified as a fresh turn.
   This is a deliberate extension beyond the literal ask.

3. **"In a thinking block" is keyed off accumulated reasoning, not
   strictly `ReasoningDelta`.** `App::in_thinking_block` returns true when
   `pending` has non-empty reasoning and no assistant text yet. This also
   treats inline `<think>…</think>` blocks (parsed into `reasoning` by
   `route_text_delta`) as "thinking", which is slightly broader than the
   prompt's "receiving `ReasoningDelta`" wording but matches the intent
   (the model is thinking). Models that emit no reasoning at all still
   never flip to the yellow indicator, as required.

4. **Kept the inline chat-body `Thinking…` placeholder.** The prompt said
   "Replace/**extend**" the inline `Thinking…` (`render_pending` in
   `src/tui/history.rs`) but its detailed spec only described the new
   above-input indicator. I left the inline placeholder in place and
   added the new indicator alongside it. Consequence: during a reasoning
   gap that lasts past the 2s grace, the streaming-position `Thinking…`
   and the above-input `Thinking (Xs)` can both show. If you'd rather the
   inline one be removed now that the status bar exists, that's a small
   follow-up.

5. **`AgentIdle` also finalizes any in-flight pending turn.** In
   `apply_event`, the `AgentIdle` arm calls `finalize_pending()` before
   `end_working_span()`. In normal flow `pending` is already finalized by
   `AssistantText`/`ToolStart`; this is defensive so an unfinalized
   pending can't linger after the agent goes idle.

6. **Indicator is its own geometry pane between history and the queue
   strip.** Modeled on the existing queue-strip pane (`src/tui/geometry.rs`
   gained an `indicator` slot). Like the queue strip, it shifts the input
   box up by one row when it appears (at the 2s mark) and back down when
   it clears (on idle). Indicator text is indented one leading space to
   sit off the terminal edge, loosely matching the queue strip's inset
   rather than the composer's prompt-prefix exactly.

7. **Could not run the interactive TUI verification.** The prompt's manual
   checklist (climbing timer, yellow flip and back, queue not resetting
   the span, fresh message re-roll) needs a live model + a real terminal,
   which this environment can't drive. I validated the logic by tracing
   the prompt's worked example (think 3s / work 10s / think 2s / work 10s
   / idle) against the implementation and by unit tests for the pure
   helpers (`format_status_elapsed`, `thinking_dots_padded`,
   `pick_working_msg`). `cargo build`, `cargo test` (300 pass), and
   `cargo fmt --check` are green.

8. **`cargo clippy -- -D warnings` fails on the pre-existing baseline.**
   The repo currently has 107 clippy errors in code unrelated to this
   task (e.g. `auth::codex` private-type leaks, `AgentDef never
   constructed`, `collapsible_if` in `welcome.rs`). Stashing my changes
   shows the same 107, so this change introduces **zero** new clippy
   findings. I did not fix the pre-existing lints (out of scope). (The
   later prompts' refactors happened to remove two of these, so the
   baseline now reads 105 — still zero introduced by any of my work.)

## Task: in-TUI launch banner box (`prompts/launch-branding.md`)

1. **Added `ACCENT_BLUE_INDEX` to `theme.rs`.** The prompt said to reuse
   "the theme's accent blue (see `theme.rs`)", but `theme.rs` had no
   accent constant — only `MUTED_COLOR_INDEX`. I added
   `ACCENT_BLUE_INDEX = 33`, matching the existing user-message-bubble
   blue (`USER_BORDER_FG = Color::Indexed(33)` in `history.rs`), and used
   it for the box. I left `USER_BORDER_FG` as-is rather than refactoring
   it to reference the new constant (out of scope).

2. **Too-small terminal → skip the box, don't clip.** The prompt allowed
   "clip or skip"; I skip (render nothing) when the box is wider or
   taller than the chat pane, so a half-drawn box never corrupts the
   layout. The chat pane then just starts empty, exactly as the
   suppressed-banner path does.

3. **Vertical placement implemented as: messages always bottom-aligned;
   box floats at the vertical center until the rising message block would
   reach it, then sits directly above the messages and scrolls off with
   them.** When box + messages overflow the pane, the box becomes the top
   of one contiguous bottom-aligned scroll buffer (so wheel-scroll-up can
   still reveal it, like the oldest message). This matches the prompt's
   three-phase description.

4. **The box re-renders from `self.launch` every frame**, so `/new`
   (which clears history + reloads launch info) brings it back centered
   with no extra wiring. It is not stored in history.

5. **Reused `chrome::status_line_spans` for the cwd + branch-badge line**
   so the box's path line is byte-for-byte identical to the persistent
   chrome, per "match the existing header exactly."

## Task: frequency-ranked autocomplete (`prompts/frequency-ranked-autocomplete.md`)

1. **Counts delivered via a dedicated `GetUsageCounts` request** issued
   right after `Attach` (the prompt's "add a small message" option),
   rather than extending the `Attached` response. Because the daemon
   attach is **lazy** (first submit), frequency ranking activates after
   the first message is sent; before that the three surfaces use their
   existing alpha / declaration-order fallback — which the prompt
   explicitly endorses for zero-count items. Optimistic local increments
   work from the first recordable pick regardless.

2. **`RecordUsage` requires no attached session server-side** (it's a
   global DB write). The TUI sends records over the runner's client;
   picks made before the runner exists are buffered and flushed on
   attach, with `tag` project ids backfilled at flush time from the
   now-known project.

3. **Model pick recorded on picker completion (`is_done()`), keyed on the
   finally-chosen `provider/model`, and only when accepted (not on Esc).**
   The prompt said "Enter on the Pick step"; recording at completion is
   where the TUI has the daemon client and where the choice is actually
   committed, and it's functionally equivalent for a frequency tally.

4. **Seed merge is additive** (daemon counts + optimistic local) and the
   maps are **reset on `/new`** so the next attach re-seeds cleanly
   without double-counting. The daemon is queried once per session.

5. **`@`-tag suggestions are cached by query** (`at_cache`); the
   count-based re-sort is computed at walk time and cached. Acceptable
   because accepting a tag changes the composer query, which invalidates
   the cache — so a stale sort is never shown.

6. **Tab-descending into a directory is not counted** — only a finalized
   tag (file accept, or Enter on a directory) is recorded, since
   descending is navigation, not a committed pick.

## Task: instruction-file token display + tokenizer calibration (`prompts/better-token-estimation.md`)

1. **Migration numbered `0004`, not `0003`.** The prompt's SQL header
   said "migration 0003", but `0003` was already taken this session by
   the frequency-ranked feature's `usage_events` migration. The table
   schema is otherwise verbatim; it's `0004_tokenizer_calibration.sql`.

2. **Feature 1 estimate delivered via a dedicated `GuidanceEstimate`
   request the TUI issues eagerly at launch**, connecting to an
   already-running daemon (no attach, no spawn — so it never creates a
   session). It computes daemon-side as required. Limitation: the
   fresh-chat `X tokens in <file>` only appears when the daemon was
   already running at launch; if the user starts the daemon via the
   startup prompt, the estimate (fetched once at `run()` start) won't be
   present that session. Acceptable under the daemon-first default;
   could be made reactive later.

3. **Calibration text basis = `serde_json` of the messages sent (history
   + this prompt) + the assistant output text**, per round. The prompt
   said "concatenate the message contents sent + the assistant output
   text" and "do NOT couple to rig's exact request serialization." I use
   serde of the rig `Message`s as a *stable proxy* (not the provider wire
   format); the scale factor absorbs the system-prompt / tool-schema /
   serialization overhead, exactly as the prompt's caveat allows.

4. **Calibration sampling runs in the engine's `turn`** (the one place
   with session + prompt + output + usage all in scope), calling
   `Session::note_calibration_sample`. The `Session` owns the in-memory
   `Calibrator` and performs the DB upsert on window close. The prompt
   suggested an `estimate(text, provider, model)` in `tokens.rs`; since
   that needs DB access (the resolver), I split the capability instead:
   pure `tokens::count_with` + `tokens::scaled_estimate`, plus
   `Db::resolve_tokenizer`, combined at the call site (the server's
   `GuidanceEstimate` handler). `tokens::count()` now delegates to
   `count_with(cl100k)` — unchanged behavior, still used by callers
   without model context (auto-title).

5. **Sampling skips cached / empty-usage calls and skips entirely while a
   non-expired calibration row exists** for the active `(provider,
   model)`; an expired row is reused by the resolver but still allows a
   fresh window to recompute and overwrite — never dropping to the
   global default mid-recompute, per the spec.

6. **clippy `-D warnings`** parity holds (baseline now 105 after earlier
   refactors); this feature introduces zero new findings.
