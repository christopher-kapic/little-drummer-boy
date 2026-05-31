# `auto` router agent (new default primary)

## Goal

Add a new primary agent, `auto`, that becomes the default agent a
session starts on (user-overridable). `auto` is a conversational
front door: it reads the user's request, and once it can tell what the
user wants, it hands off to `Plan` (planning/decomposition) or `Build`
(do-it-now implementation). It can also answer simple, non-coding
questions directly without delegating.

## Current behavior

- Sessions always start on `Build` — `initial_active_agent()` in
  `src/daemon/session_worker.rs` returns `"Build"`; restore-on-reopen
  filters the persisted agent to `Plan` or `Build` only.
- The two primary agents are `Build` and `Plan`. Primary-agent swap
  happens at an idle boundary via `swap_primary()` in
  `src/engine/driver.rs` (no mid-turn swap).
- Built-in cast is registered in `src/agents/builtin_defs.rs`
  (`BUILTIN_AGENT_NAMES` + embedded factory fns); agent prompts live in
  `src/engine/builtin/*.md`. `AgentDef` / `AgentMode` in
  `src/agents/mod.rs` (`Primary` vs `Subagent`).

## Desired behavior

- `auto` is a **real primary agent on the main model** (not a
  utility-model classifier) — it converses and can answer directly.
- On each new session, `auto` reads the user's request:
  - **Clear planning intent** (decompose a feature, build a plan,
    multi-step design) → hand off to `Plan`.
  - **Clear build intent** (make this change now, fix this, implement
    X) → hand off to `Build`.
  - **Ambiguous** → do **not** guess. Converse with the user (normal
    conversation and/or the `question` tool) across as many turns as
    needed until intent is clear, then hand off.
  - **Pure question / no code change** → answer directly, no handoff.
- Hand-off uses the existing `swap_primary()` idle-boundary mechanism.
  Once `auto` hands off, the chosen agent owns the conversation; `auto`
  does not stay in the loop.
- `auto` becomes the **default** initial agent, and the default is
  **user-overridable in `/settings`**: add a setting for "default
  primary agent" (choose among the primary agents — `auto`, `Build`,
  `Plan`). Persist it in the extended config (`extended-config.json`,
  `src/config/extended.rs`); surface a row in
  `src/tui/settings/ui_page.rs`.

## Edge cases & decisions (settled)

- Engine: main model, conversational — `auto` defers (hands off) as
  soon as intent is determined, which may take several turns of chat.
- `initial_active_agent()` returns the configured default (falling back
  to `auto`); restore-on-reopen must allow `auto` alongside `Plan` /
  `Build`.
- Keep `auto`'s system prompt terse — base system-prompt budget is
  ~400 tokens (GOALS §10).
- **Naming convention conflict to reconcile:** CLAUDE.md says primary
  agents are Capitalized (`Build`, `Plan`) and subagents are lowercase.
  The user named this agent `auto` (lowercase) but it is a *primary*
  agent. Resolve one of two ways and apply consistently: either name it
  `Auto` to follow the convention, or keep `auto` and update the
  casing-convention note in CLAUDE.md to carve out this exception. Do
  not leave the codebase and the documented convention disagreeing.

## Expected UX / acceptance

- New sessions open on `auto` (or the user's configured default).
- A clearly-build request gets handed to `Build`; a clearly-plan
  request to `Plan`; an ambiguous one produces a clarifying exchange
  first, then a handoff; a plain question is answered without handoff.
- `/settings` lets the user change which primary agent new sessions
  start on, and the choice persists across restarts.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Honor token economy (GOALS §10): one-sentence tool descriptions,
  noun-phrase parameter descriptions, base system prompt ≤ ~400 tokens.
