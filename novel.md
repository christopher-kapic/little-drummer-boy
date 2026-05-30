# Novel cockpit design decisions

Design decisions present in cockpit's planning docs that do not appear in
codex, claude-code, opencode, or pi (based on `features/*.md` dossiers
and cross-reference with `GOALS.md`, `plan.md`, `miscellaneous.md`,
`opencode-features-review.md`, and `TUI-design-philosophy.md`).

## Security & redaction

1. **Non-bypassable redaction with a single chokepoint** — every prompt
   crossing the network passes through one `redact::scrub()`; no
   per-call flag disables it, only a global toggle.
2. **Greppable, secret-name-free placeholder** —
   `***redacted-by-cockpit-cli***` deliberately omits the env var name,
   avoiding indirect provider leakage.
3. **Redaction failure is a hard error with its own exit code (`4`)** —
   refusing to send rather than warning, distinct from generic CLI
   errors.
4. **`cockpit debug redact` to audit the redaction table** — users can dump
   exactly what would be scrubbed.
5. **Secondary-model prompt-injection guard (T3)** — untrusted text
   scanned by a cheap model before entering context. None of
   codex/opencode/claude/pi have this.

## Configuration model

6. ~~**Extended config as a purely additive layer.**~~
   **Superseded by GOALS.md §2a config collapse.** With
   opencode-config compatibility dropped (§2), the two-file split
   (`opencode.json` + `extended-config.json`) collapses to a single
   `config.json`. The original novelty of "additive on top of
   opencode" no longer applies; cockpit's config is just its own.
7. **Arbitrary agent paths via `--agent-file` and `agent_dirs`** —
   opencode forces fixed locations; cockpit accepts ad-hoc paths and extra
   search dirs.
8. **Configurable hierarchy of guidance files** —
   `agent_guidance_files: []` chooses load order across `AGENTS.md`,
   `CLAUDE.md`, `.github/copilot-instructions.md`, `.cursorrules`. Loads
   the first match, stops (doesn't concatenate).

## Token economy

9. **~400-token system-prompt budget enforced in CI** — build fails if a
   PR pushes the base prompt past budget. No comparable harness
   publishes a budget, let alone enforces one mechanically.
10. **Deterministic context pruning (snapshot/cumulative
    classification + backward + forward + user-facing `/prune`)** —
    cockpit classifies every tool call as **snapshot** (read/ls/grep/git
    status — only the latest result matters) or **cumulative**
    (write/edit/`npm install` — the *act* matters even after the body
    is stale). Snapshot results are eligible for **backward prune**
    (older bodies collapse to `Part::Elided` markers) and **forward
    prune** (a re-read on a file whose content is provably current —
    by continuous lock-hold or by hash match — returns a ~30-token
    stub instead of the full body). The live "% prunable" indicator
    in the status line makes the cache-vs-context trade-off visible
    before the user invokes `/prune`. Codex/opencode rely on LLM
    compaction; no surveyed harness exposes deterministic pruning
    as a first-class user-facing command.
10a. **Bash result handling is three-layered, not auto-classified.**
    cockpit refuses to auto-prune arbitrary bash output (the
    classification problem — is `mv` a snapshot? is `npm install`? —
    is genuinely hard, and silently dropping load-bearing output is
    unacceptable). Instead: (1) always-on head+tail truncation for
    over-cap bodies (call preserved, bulk shrinks); (2) opt-in
    `extended.prune.bash_snapshot_commands` allowlist for exact
    command-string match dedup; (3) manual `/prune` TUI picker with
    eyes-on review. Other harnesses either don't prune bash or
    auto-classify by command-line parsing.

## Orchestration & delegation

11. **`cockpit meta` as a built-in harness orchestrator** — invokes any
    declared harness via `harness_invoke()` and recurses into
    `cockpit_subagent()`. Every other harness in the comparison set is
    single-harness.
12. **Per-call `subagent` vs `fork` mode with a configurable default** —
    fresh-context vs inherited-context delegation chosen on each call.
    Opencode has one primitive; codex has thread forking; cockpit makes
    both first-class.
12a. **Interactive subagents that become the foreground primary,
    with a deferred-log return channel.** When an orchestrator
    spawns a subagent in interactive mode, the subagent is **swapped
    into the composer** as the agent the user talks to directly,
    while the active primary agent is paused. If the user asks the subagent
    for out-of-scope work, the subagent doesn't expand its scope —
    it calls `defer_to_orchestrator(message)`, which appends to a
    per-task deferred-log buffer. On subagent completion, the
    active primary agent resumes and receives `{ report, deferred_log: [..] }`
    in one go. The pattern keeps subagent scope discipline intact
    while letting users fluidly steer the conversation mid-flight.
    No surveyed harness combines transparent foreground-swap with
    the deferred-log return channel.
12b. **Two-agent split (`Build` vs `Plan`).** Other harnesses treat "plan" and "build"
    as behavioral modes of one agent (opencode's `/plan` / `/build`
    slash toggles). cockpit makes them **separate primary agents** with
    distinct prompts, distinct categories (planner defaults to a
    thinking model), distinct cognitive framing. They share the
    session DB and lock manager, so a graph plan authored under
    Plan is immediately consumable when the user switches to
    Build. The split is structural, not just naming.
13. **In-process ralph (graph plan) executor** — ralph-rs reimplemented
    inside cockpit so a single process owns file-lock leases. Possibly
    also in claw-code's lane orchestration, but not generalized there.
14. **Single-exclusive-lock file model with opportunistic-write
    semantics** — cockpit ships a single per-file exclusive lock (no
    shared-readers RWLock), exposed through four verbs (`read`
    unlocked / `readlock` exclusive acquire / `write` keep-lock /
    `writeunlock` release-after-write, plus the same pair for
    `edit`). The unifying write rule — "a write succeeds iff the
    agent holds the lock OR no other agent holds it AND the agent's
    last-known hash matches disk" — turns the lock into a
    *reservation* (claim of intent, useful for FIFO queueing) rather
    than a strict gate. Locks coordinate intra-process agents;
    hashes catch external editors / formatters that locks can't see;
    the two are complementary by design. Canonical-path lock
    ordering prevents multi-file deadlock by construction. 140s idle
    timeout resets on **any** tool call from the lock holder, not
    just writes — so a reasoning model thinking between `readlock`
    and `write` doesn't lose its lock. No surveyed harness exposes
    this verb set or the opportunistic-write rule.
14a. **`/prune` vs `/compact` are deliberately split.** Other
    harnesses bundle "shrink the context" into a single compaction
    operation. cockpit splits the surface: `/prune` is the
    deterministic, reviewable, no-LLM scalpel; `/compact` is the
    LLM-driven heavyweight handoff. Users see both indicators in
    the status line and choose deliberately.
14b. **`/compact` as fresh-thread handoff with deterministic state
    appendix.** Instead of opencode-style inline summarization
    (rewrite older turns in-place), cockpit's `/compact` asks the
    model to draft a self-contained handoff brief, concatenates a
    runtime-assembled **deterministic state appendix** (files
    touched + current hashes, bash commands with exit codes, git
    branch, dirty files, open todos, pinned messages verbatim),
    lets the user review/edit, then starts a fresh session seeded
    with the result. The old session is preserved on disk.
    Properties: no compaction sediment (no summarizing summaries),
    clean prompt cache for the new thread, model-summarized intent
    paired with programmatically-summarized facts. The fact-vs-
    intent split is the novel piece — the model is good at "why,"
    bad at "I touched these 17 files"; the appendix covers the
    second.

## TUI & composer

15. **Vim mode default-on** — codex has it as opt-in via `/vim`;
    opencode doesn't emphasize it. cockpit treats vim as the design
    center.
16. **cwd + git branch always shown, non-toggleable** — codex exposes
    `/statusline` to turn it off; cockpit makes it mandatory chrome.
17. **Unified `/` and `:` palette with mandatory `?` help and `<tab>`
    completion** — every keybinding has a named command equivalent
    surfaced through one palette.
17a. **Destructive up-arrow recall + fold-at-send for queued
    messages.** opencode's queue is buggy: pressing up-arrow loads
    a queued message into the composer but leaves the original in
    the queue, producing duplicate sends. cockpit's recall **pops**
    (removes from queue); multiple queued messages **fold** into
    a single `A\n\nB` user message before delivery; the queue
    flushes at the **next inference boundary** rather than the
    next user turn, so mid-tool-loop messages ride along with the
    next tool-result round-trip. None of the surveyed harnesses
    do all three together — opencode has the queue but the bug;
    codex doesn't queue mid-turn; Claude Code buffers but doesn't
    fold or expose up-arrow editing.
17b. **Alt-screen-then-flush exit (copyable transcript tail).**
    Full-screen-TUI harnesses that use the terminal alt screen
    (opencode, codex) wipe the conversation when the user exits —
    commands the agent produced can't be copied because they're
    gone. Claude Code avoids this by rendering in the primary
    buffer the whole time. cockpit takes a third path: alt screen
    *during* the session for the clean full-screen experience
    (status line, floating slash menu, approval dialogs), then
    on exit the alt screen tears down and the last N turns
    (default 3, configurable; whole session at `-1`) are printed
    to the primary buffer in copy-friendly form (no box chars,
    color stripped under `NO_COLOR`, agent-name prefixed). User
    gets the clean TUI during use and the scrollback-copyable
    transcript on exit.

## Persistence & observability

18. **Event metadata envelope includes `watcher_action` from v1** —
    reserves the schema slot for `cockpit connect` so adding it later
    isn't a breaking change. Possibly also in claw-code.
19. **Per-event persistence mode (`Suppress` / `PersistContent` /
    `PersistFull`)** — selectively store envelope vs payload to keep
    SQLite small while preserving the user-visible record.

## Provider integration

20. **Provider transform layer that preserves cache boundaries** —
    per-model mutations (Kimi `is_error` stripping, reasoning-param
    removal) without invalidating the system-prompt cache anchor.
    rig-core doesn't do this; the planning docs flag it as a deliberate
    cockpit responsibility.

## Testing & build hygiene

21. **Wire-level mock-LLM parity harness with scenario-via-API-key** —
    borrowed from claw-code wholesale; not present in
    codex/opencode/pi.
22. **Provenance-checked dogfood build** — `GIT_SHA` baked in at build
    time; binary fails if reported SHA disagrees with build script.
    Catches "you forgot to rebuild." Also from claw-code.

## Possibly-also-elsewhere (flagged, not claimed)

- Part-based messages with sortable IDs (opencode also uses part-based
  messages — cockpit just adopts the same shape).
- Lazy/defer-load tool specs (opencode does this; cockpit makes it
  mandatory for all tools).
- Hooks in extended config (opencode has `experimental.chat.*`;
  oh-my-codex and claw-code ship hooks too — cockpit's contribution is the
  unified vocabulary, not the mechanism).
- Pluggable `MemoryBackend` trait (oh-my-pi has Hindsight externally;
  cockpit ships the trait + local SQLite default).

## Strongest novelty clusters

The clearest "only in cockpit" clusters are **security/redaction**
(items 1–5), **meta-harness + graph execution** (items 11–14), and
**context-budget surface** (items 10, 10a, 14a, 14b — the deliberate
`/prune` vs `/compact` split, the deterministic state appendix, and
the live status-line indicator are all pieces of one design that
treats the context window as a first-class user-visible resource).
