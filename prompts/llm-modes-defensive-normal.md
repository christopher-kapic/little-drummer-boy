# Defensive / normal LLM modes (`llm_mode`)

> **Status: blocked — do not implement yet.** Two hard prerequisites must
> land first:
> 1. `prompts/user-definable-agents.md` (markdown agent files + the
>    flat-file / `<name>/` per-mode directory resolution seam).
> 2. The planning-mode prompts — `prompts/planning-mode-authoring-flow.md`
>    and `prompts/planning-mode-tools-and-storage.md` — because the
>    `Plan` agent and the interactive-subagent machinery that
>    defensive mode routes through (item 3) are their deliverables.
>
> Re-read `GOALS.md`, `plan.md`, and `CLAUDE.md` before starting.

## Goal

Let cockpit steer subagents and tool definitions differently depending on
the strength of the active model, via an explicit two-value axis:
**`normal`** (strong/expensive models — terse descriptions, lean steering,
lean on model intelligence) and **`defensive`** (cheaper/weaker ~120k-context
OS models, cockpit's primary target per GOALS §1 — explicit steering, more
decomposition). The axis is `llm_mode`.

## Mode selection — explicit `llm_mode` config key

- New top-level field on `ExtendedConfig` (`src/config/extended.rs`):
  `llm_mode`, an enum with values `normal` and `defensive`
  (`#[serde(rename_all = "lowercase")]`, matching the existing enum
  conventions in that file — see `ThinkingDisplay`, `VimModeSetting`).
- **Default is `defensive`** — safe for the weak-model target; strong-model
  users opt into `normal`.
- Follows the existing config-layering precedence (`src/config/dirs.rs`
  walk-up + global/home layers). Reuse it; do not invent a parallel scheme.
- **Not** auto-inferred from model identity in this iteration — explicit only.
- **Unknown `llm_mode` value in config:** reject with the offending value
  backticked and the valid set listed (mirror the `vim_mode` deserializer's
  error style in `extended.rs`).

## What varies by mode

### 1. Tool-description verbosity (cockpit-global) — full surface

- **Defensive** renders explicit, steering tool and parameter descriptions;
  **normal** stays terse (the current GOALS §10 one-sentence / noun-phrase
  form).
- **Author defensive descriptions for the *entire* built-in tool surface**
  in this work — no partial coverage, no terse-fallback gaps, no
  TODO-for-later tools. This is the deliberate token tradeoff defensive mode
  accepts.
- Centralize the verbosity switch in **one** rendering place keyed off the
  active `llm_mode`, so the axis lives in a single location (the centralized
  rendering seam the `user-definable-agents` work was told to preserve).
- **Tool *grants* do not vary by mode** — only how each tool's description
  renders. The `tools:` schema needs no mode awareness.
- **Budgets:** normal mode must hold the *current* token-economy budgets
  (CI still enforces the base system-prompt budget unchanged); defensive's
  extra verbosity is the only place the budget is allowed to grow, and it is
  the intended tradeoff.

### 2. Per-agent prompt variants — flat file vs per-mode directory

Disk layout settled in `user-definable-agents` (build on that resolution,
do not reinvent):

- A single-mode agent is the flat file `<agents-dir>/<name>.md` — used for
  **every** mode.
- A multi-mode agent is the directory `<agents-dir>/<name>/` with one file
  per mode: `normal.md`, `defensive.md`. Resolving an agent picks the file
  matching the active `llm_mode`.
- Resolve the prompt **through** the agent-prompt resolution path that
  `user-definable-agents` was required to leave mode-threadable — do not
  reach straight for `def.prompt` at scattered call sites.
- **Do not overload `AgentDef.mode`** (`primary`/`subagent`/`all` — that is
  reachability). `llm_mode` is a distinct axis.
- **Mode set but an agent has no matching mode file:** fall back to the flat
  `<name>.md` if present; otherwise error naming the agent and the mode.

### 3. Delegation / subagent shape — per-mode, gated on planning-mode

This is the reason the planning-mode prompts are a hard prerequisite: the
defensive path routes through machinery those prompts deliver.

- **`normal`:** multi-part work uses **episode sequencing** — the
  `Build` agent walks tasks in sequence within one context.
  Less decomposition, fewer subagents.
- **`defensive`:** decompose harder and route through **interactive
  subagents** (the planning flow built by the planning-mode prompts, via the
  `Plan` agent) — each subagent does a narrow job, runs its
  heavy interview in its own leaf context, and returns only a small capped
  report (the GOALS §10 / token-economy report-cap pattern).
- Wire the per-mode cast/delegation explicitly. **Keep the leaf-terminated
  invocation tree and single-writer (only `coder` writes/holds locks)
  invariants intact in both modes** — these do not relax under either mode.

## Live switching

Owned by **this** prompt: the `/llm-mode` slash command and a **generic,
reusable cache-break-warning helper**.

- **`/llm-mode [toggle|defend|defensive|normal]`** switches the active
  `llm_mode` live (hotswap):
  - no argument or `toggle` → flip between `normal` and `defensive`
    (`toggle` is the default action).
  - `defend` → set `defensive`. **Advertise `defend`** in help/usage (shorter
    to type); also accept `defensive` as a silent alias.
  - `normal` → set `normal`.
- Switching busts the cached system prefix. Apply the **same
  pruning / prompt-cache discipline used elsewhere** (see the pruning policy:
  prune whenever expected cache hit is 0) rather than silently churning.
- **Cache-break warning:** on switch, warn the user that the prompt cache
  will be invalidated — **but suppress the warning entirely when the active
  model/provider doesn't cache** (the warning is meaningless there). Reuse
  the existing no-cache predicate from the pruning-policy code; do not
  duplicate the "does this provider cache" logic.
- Build the warning as a **shared helper**, not inline in `/llm-mode`. Two
  sibling switchers reuse it and are specced in their own prompts — do **not**
  build them here:
  - **shift+tab** at the top-level TUI cycles the active primary agent
    between `Plan` and `Build` →
    specced with the planning-mode prompts.
  - **`/agent`** lets the user change the active agent → specced with
    `user-definable-agents`.
  This prompt must leave that helper in a state both can call. (Cross-ref
  only — implementing shift+tab and `/agent` is out of scope here.)

## Terminology

`Plan` and `Build` are **agents**, not "modes."
Use "agent" for them throughout code, UI, and docs. The only thing called a
*mode* in this feature is `llm_mode` (`normal` / `defensive`). Audit any
existing "mode" wording that actually refers to these agents and correct it
as you touch it (no half-renamed surface).

## Surfacing in the TUI

- Expose `llm_mode` where comparable toggles already live — the `/settings`
  UI page (`src/tui/settings/`, alongside `vim_mode` / `thinking` etc.) and
  the config file. Match the existing toggle pattern; don't invent a new one.
- The live `/llm-mode` command is the in-session path; `/settings` and the
  config file are the persistent path. All three resolve to the same value.

## Edge cases & UX decisions

- **Unknown `llm_mode` in config:** reject, offending value backticked, valid
  set listed.
- **Agent missing a matching mode file:** fall back to flat `<name>.md`; else
  error naming agent + mode.
- **Mid-session switch:** prune/cache-bust per existing discipline; show the
  cache-break warning unless the provider doesn't cache.
- **`/llm-mode` with no arg:** toggles.

## Expected UX / acceptance

- Setting `llm_mode: defensive` (config or `/llm-mode defend`) produces the
  explicit tool descriptions, selects `defensive.md` agent prompt variants
  where present, and routes multi-part work through the interactive-subagent
  (planning / `Plan`) delegation shape.
- `normal` produces terse descriptions, `normal.md` variants, and episode-
  sequencing (`Build`) delegation.
- Single-mode (flat-file) agents behave identically in both modes.
- Normal mode holds the current token-economy budgets; CI still enforces the
  base system-prompt budget.
- `/llm-mode` switches live; the cache-break warning fires on a caching
  provider and is silent on a non-caching one.
- `Plan` / `Build` are referred to as agents
  everywhere; no lingering "mode" wording for them.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` pass; new behavior is covered by tests (mode
  resolution + default, per-mode prompt selection + flat-file fallback,
  full-surface description rendering, unknown-value rejection, `/llm-mode`
  parsing incl. `defend` alias and `toggle` default, cache-warning
  suppression on no-cache providers).

## Suggested packages

None expected — this reuses existing config, agent-resolution, slash-command,
and pruning/cache infrastructure. Add a dependency only if a concrete need
surfaces; if so, justify it per the constraints below.

## Constraints (non-negotiable)

- Implement **without incurring tech debt** — no shortcuts, no
  TODO-for-later, no half-finished paths. The feature lands complete.
- For any new package, use the **latest stable release** unless this prompt
  says otherwise, and **verify correct API/dependency usage** with
  `kcl ask <package> "<question>"` before wiring it in.
- Hold all `CLAUDE.md` design rules: single-writer file locking, daemon-first
  architecture, cockpit-native config (do not parse opencode files), token
  economy, redaction non-bypassable, cross-platform (Linux/macOS/Windows).
