# Rename the orchestrator agents to `Build` / `Plan` (code)

## Goal

Rename cockpit's top-level orchestrator agent `orchestrator-build` to `Build`
throughout the code, and establish the standing naming convention that the
**docs and memory already reflect**: top-level (primary) agents are
**Capitalized** (`Build`, `Plan`); subagents (interactive or noninteractive)
are **lowercase** (`coder`, `explore`, `docs`). The docs sweep
(`GOALS.md`, `plan.md`, `CLAUDE.md`, `opencode-features-review.md`,
`design-need-to-discuss-or-test.md`, `novel.md`, `ctrlcplan.md`, and the
`prompts/` set) is **already done** — this prompt is the matching code change
so code and docs reconverge.

## Scope

- The only orchestrator agent that exists in code today is
  `orchestrator-build`. Rename it to `Build`.
- `orchestrator-plan` does **not** exist in code yet — it is a planning-mode
  deliverable (`prompts/planning-mode-authoring-flow.md`). Do **not** create
  it here. The planning-mode prompts already name it `Plan`; this prompt only
  needs to make sure nothing blocks that (the convention below).
- Subagents (`coder`, `explore`, `docs`) keep their lowercase names — no
  change.

## Canonical rename

`orchestrator-build` (the agent **name** / identifier / DB value) → `Build`.

Slash commands stay lowercase: `/build` and `/plan` are unchanged as command
tokens, but the agent each swaps to is now `Build` / `Plan` (capitalized).
Wherever a slash command maps to an agent name string, that target string
becomes `Build` (and, when planning-mode lands, `Plan`).

## Code sites (enumerate from the tree — these are the known ones)

Drive the exact list off a fresh search; do not rely solely on this list.
Search for `orchestrator-build`, `orchestrator_build`, `ORCHESTRATOR_BUILD`,
and the generic role word `orchestrator`.

1. **Agent name string + dispatch** — `src/engine/builtin/mod.rs`: the
   dispatch arm `"orchestrator-build" => …`, the `name: "orchestrator-build"`
   field, doc comments. The agent name string becomes `"Build"`.
2. **Embedded prompt + symbols** — for a complete (no half-rename) change,
   rename the embedded prompt file `src/engine/builtin/orchestrator_build.md`
   → `build.md`, the `include_str!` target, the const `ORCHESTRATOR_BUILD_PROMPT`
   → `BUILD_PROMPT`, and the factory fn `orchestrator_build()` → `build()`.
   Update the prompt **body** of that file: it currently opens "You are
   `orchestrator-build`, …" and references swapping to `orchestrator-plan` via
   `/plan` — change to "You are `Build`, …" and `Plan`.
3. **Default agent const** — `src/welcome.rs`: `DEFAULT_AGENT` →
   `"Build"`.
4. **DB default + existing rows (REQUIRED migration)** —
   `src/db/migrations/0001_initial.sql` sets
   `active_agent TEXT NOT NULL DEFAULT 'orchestrator-build'`. Do **not** edit
   the already-shipped migration. Add a **new** migration that:
   - `UPDATE sessions SET active_agent = 'Build' WHERE active_agent = 'orchestrator-build';`
     so resumed/old sessions don't carry a name that no longer dispatches.
   - Brings the column default in line with `'Build'` for new rows (follow
     the repo's existing pattern for changing a column default in SQLite —
     table rebuild or app-level default; the app already sets the name on
     session creation via `DEFAULT_AGENT`, so confirm what the column default
     actually governs and do the minimal correct thing, no tech debt).
5. **Session creation / worker** — `src/daemon/session_worker.rs`
   (`active_agent_name: "orchestrator-build"`), and any other runtime
   `create_session(..., "orchestrator-build")` / active-agent assignment.
6. **Tests** — `src/db/session_search.rs`, `src/db/sessions.rs`,
   `src/session/mod.rs`, `src/commands/export.rs`, `src/tools/session_search.rs`,
   `src/tools/session_read.rs`, `src/engine/driver.rs` all create sessions or
   assert on `"orchestrator-build"`. Update the literals and assertions to
   `"Build"`.
7. **Comments / doc-comments** referencing the agent —
   `src/engine/mod.rs`, `src/engine/agent.rs`, `src/locks/mod.rs`,
   `src/tools/read.rs`, `src/tools/skill.rs`, `src/tools/mod.rs`,
   `src/engine/builtin/coder.md`, `src/engine/builtin/explore.md`: rename
   `orchestrator-build` → `Build`. For the **generic** word "orchestrator"
   meaning "whichever primary agent owns the conversation," rewrite to
   "the primary agent" / "`Build`" as reads best. Leave unrelated uses
   alone: the `cockpit meta` meta-harness "orchestrator over other harnesses",
   the ralph executor, and the function/tool name `defer_to_orchestrator` are
   **not** the Build/Plan agents — do not touch them.

## Naming convention (add to CLAUDE.md design rules)

Add a one-line design rule under the bundled-cast rules in `CLAUDE.md`:
top-level/primary agents are Capitalized (`Build`, `Plan`); subagents
(`coder`, `explore`, `docs`) are lowercase. This is the rule the
planning-mode work relies on to name its planner `Plan`.

## Edge cases

- **Resumed old session** whose `active_agent` is still `'orchestrator-build'`
  on disk: handled by the migration (step 4). Verify a session created before
  the rename still loads and dispatches after migration.
- **Slash command `/build`**: still resolves and now swaps to `Build`.
- **No partial rename**: if you rename the file/const/fn (step 2), update
  every reference in the same change — `cargo build` must compile with no
  dangling symbol.

## Expected UX / acceptance

- A new session's active agent is `Build`; the TUI chrome shows `Build`.
- A session row created before the migration shows `Build` after upgrade and
  dispatches correctly.
- `/build` swaps to `Build`.
- No occurrence of `orchestrator-build` / `orchestrator_build` /
  `ORCHESTRATOR_BUILD` remains in `src/` except, if you choose to keep a
  historical note, none is load-bearing.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` all pass; tests updated to the new name.

## Constraints (non-negotiable)

- Implement **without incurring tech debt** — no shortcuts, no
  TODO-for-later, no half-finished paths (a partial file/symbol rename that
  leaves dangling references is a half-finished path). The rename lands
  complete and compiling.
- For any new package, use the **latest stable release** unless this prompt
  says otherwise, and **verify correct API/dependency usage** with
  `kcl ask <package> "<question>"` before wiring it in. (None expected here.)
- Hold all `CLAUDE.md` design rules: single-writer file locking, daemon-first
  architecture, cockpit-native config, token economy, redaction
  non-bypassable, cross-platform (Linux/macOS/Windows).
