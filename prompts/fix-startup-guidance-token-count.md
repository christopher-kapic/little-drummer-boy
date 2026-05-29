# Fix the fresh-chat startup token count (instructions-file size)

## Goal

The fresh-chat context indicator should accurately report the size of
the project instructions/guidance file (and the full system prompt),
and show *which* file it loaded — in **both** daemon and daemonless
mode. Today it reports a tiny, misleading number (~13–18 tokens) and
usually shows no filename.

## Current behavior (confirmed root cause)

On a fresh chat (empty history, no provider usage yet) the context
indicator tries to show `"<N> tokens in <file>"` via
`fresh_chat_guidance_label` (`src/tui/app/render.rs:1460`), called from
`context_indicator_text` (`src/tui/app/render.rs:866`).

That label renders **only** when `self.guidance_estimate` is `Some`
(`src/tui/app/mod.rs:542`). `guidance_estimate` is populated **only** by
`fetch_guidance_estimate` (`src/tui/agent_runner.rs:240`), called once in
`App::run` (`src/tui/app/mod.rs:916`). That function:

- returns `None` whenever no daemon is running (daemon-only by design —
  its doc comment claims "the TUI can't see the guidance file", which is
  false: `engine::builtin::load_agent_guidance` is `pub(crate)` and works
  in-process), and
- swallows every failure with `.ok()?` (connect/request/daemon-side miss
  all silently become `None`).

When the label doesn't fire, `context_indicator_text` falls through to
`format!("{} tokens", format_token_count(tokens))` where
`tokens = estimate_context_tokens()` (`src/tui/app/render.rs:933`). That
estimator counts **only** conversation history + pending composer text +
buffered git blocks — it does **not** include the system prompt or the
guidance file. On a fresh chat that's the ~13–18 tokens the user sees,
with no filename.

Net: the informative label silently never fires (daemonless always;
daemon when the request misses), and the fallback reports a number that
excludes the instructions file entirely — making a ~4–5k-token CLAUDE.md
look like 13 tokens.

The daemon-side estimate (`Request::GuidanceEstimate`,
`src/daemon/server.rs:585`) also counts only the guidance-file **body**,
not the full composed system prompt (role prompt + OS line + session +
guidance) built by `compose_system_prompt`
(`src/engine/builtin/mod.rs:57`).

## Desired behavior

1. **Show the guidance file label in both modes.** On a fresh chat,
   display `"<N> tokens in <file>"` whenever a guidance file exists —
   regardless of whether a daemon is running.
   - Daemon running: use the daemon's calibrated estimate (current path).
   - No daemon (daemonless): compute the estimate locally in the TUI via
     `engine::builtin::load_agent_guidance` + raw cl100k counting
     (`crate::tokens::count`). Update/remove the inaccurate
     "the TUI can't see the guidance file" comment.

2. **Fold the full system prompt into the running context estimate.**
   `estimate_context_tokens` must account for the full composed system
   prompt (role prompt + OS line + session + guidance body) — the actual
   fresh-context size sent to the model — not just history/pending. After
   the first round-trip the provider's authoritative usage still anchors
   the count as it does today; this only fixes the pre-first-turn
   estimate and the steady local component so the indicator never
   under-reports the baseline.

   So: the `"<N> tokens in <file>"` label reports the **guidance-file
   body** tokens; the underlying context estimate reflects the **full
   system prompt**. Both are correct for what they name.

## Edge cases & decisions (settled — implement as written)

- **Calibration:** when the daemon is available, use its calibrated
  count (`resolve_tokenizer` + `scaled_estimate`); in the local fallback
  use raw cl100k (`crate::tokens::count`). The two modes may differ by
  the calibration factor (~30%); that is acceptable — each is the best
  available for its mode. Do not try to replicate DB calibration in the
  TUI.
- **No guidance file found:** no `"… in <file>"` label. Fall through to
  the normal context indicator — but that fallback must now reflect the
  full system prompt (item 2), so even with no guidance file the fresh
  count includes the base role prompt rather than ~0.
- **Daemon path fails (connect/request error) while a daemon is
  nominally up:** fall back to the local computation rather than
  silently showing the misleading history-only count. Do not leave the
  current silent `.ok()? → None → tiny number` behavior in place.
- **Label trigger/revert unchanged:** still fresh-chat only (no history,
  no provider usage yet); reverts to `ctx X%` / `… prunable` form once a
  round-trip returns usage or history exists.
- **Token formatting unchanged:** keep `format_token_count` (≥1000 →
  `"N.Nk"`). A real CLAUDE.md will now correctly render as e.g.
  `"4.1k tokens in CLAUDE.md"`.

## Acceptance

- Launch the TUI in this repo (root has `CLAUDE.md`, ~13 KB / ~4–5k
  tokens) **with** a daemon running: fresh-chat indicator shows
  `"~Nk tokens in CLAUDE.md"` where N reflects the real file size
  (calibrated), with the filename.
- Launch **without** a daemon: same label appears (raw cl100k count),
  with the filename. No more bare `"13 tokens"` with no file.
- In a directory with no guidance file, the fresh indicator shows a
  full-system-prompt-inclusive count (not ~0/13), and no `"… in <file>"`
  suffix.
- Keep `fresh_chat_guidance_label`'s purity/unit-testability; add/extend
  tests covering: daemon-estimate present, local-fallback path, and
  no-guidance-file path.

## Constraints (always)

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise (no new deps are anticipated here).
- Verify any third-party API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Respect repo priorities: code correctness first, then token economy
  (this is display-only, but don't bloat the system prompt), then speed.
  The local fallback must not block TUI launch — keep it cheap/best-
  effort like the current daemon fetch.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` must all pass.

## Notes

- Feature originated in commit `cd13770` ("Overnight /goal"); there was
  never a local fallback, so this is a structural gap, not a recent
  regression of working code.
- `load_agent_guidance` already walks `cwd` → git worktree root and
  honors `extended.agent_guidance_files` (default
  `["AGENTS.md", "CLAUDE.md"]`); reuse it for the local path — do not
  reimplement discovery.
