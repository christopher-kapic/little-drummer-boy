# User-definable agents (markdown agent files + reset-to-default)

> **Large refactor.** Re-read `GOALS.md`, `plan.md`, and `CLAUDE.md` before
> starting — the invariants below must hold.

## Goal

Make every built-in agent a markdown-defined agent that users can read,
modify, or replace, and let users add their own agents — the way opencode
exposes custom agents. Provide a single "reset default agents" control in
`/settings` that restores all built-in agents to what cockpit ships.

## Current behavior

- Built-in agent system prompts are embedded in the binary via
  `include_str!` (`src/engine/builtin/mod.rs`, the `*_PROMPT` consts) from
  `src/engine/builtin/*.md`. The `.md` files are plain markdown, no
  frontmatter.
- Each agent's model, params, and **tool surface are hardcoded** in Rust
  factory functions in `src/engine/builtin/mod.rs` (`coder()`, `explore()`,
  `orchestrator_build()`, etc.) — e.g. `coder` gets the write/lock tools,
  `explore` is read-only.
- `src/agents/mod.rs` already sketches the target shape: an `AgentDef`
  struct (`name`, `description`, `mode`, `model`, `temperature`, `tools`,
  `permission`, `prompt`, `source`) and stubbed `load_from_file()` /
  `list_all()` — both currently `todo!()`. The intended file format is YAML
  frontmatter + markdown body. `extended.agent_dirs`
  (`src/config/extended.rs`) is the configured extra search path.
- The `/settings` TUI (`src/tui/settings/mod.rs`) has an `Agents` page that
  is a **stub** (`AGENTS_STUB` text only).
- Config discovery walks up the `.cockpit/` chain with home/global layers
  (`src/config/dirs.rs`), left-to-right precedence.

## Desired behavior

Implement `AgentDef` loading/parsing and make the built-in cast
markdown-defined and user-overridable, with these settled decisions:

### Disk model — overlay with on-demand eject
- Default agent definitions **stay embedded in the binary**. Nothing is
  written to disk on first run.
- A user "editing" a built-in agent **ejects** it: cockpit writes that
  default's markdown (frontmatter + prompt body) to
  `.cockpit/agents/<name>.md` for the user to modify. From then on the
  on-disk file **overrides** the embedded default by name.
- "Reset" = delete the on-disk override file; the embedded default takes
  over again. No stale on-disk copies of unmodified defaults; on-disk files
  exist only when customized.
- Override resolution follows the existing config-layering precedence
  (`src/config/dirs.rs` walk-up + global/home layers + `agent_dirs`).
  Reuse that discovery; do not invent a parallel path scheme.

### Reset control — reset-all only, custom agents untouched
- `/settings` → Agents page gets **one** "Reset all built-in agents to
  default" action behind a confirmation dialog.
- It deletes all on-disk overrides for built-in agent names, restoring the
  embedded defaults.
- **User-created custom agents** (any name that is not a built-in) are
  **never** touched by reset. No per-agent reset button in this iteration.
- The Agents page should list built-in agents (marked when overridden) and
  custom agents (marked custom), replacing the current stub.

### Editable agents and adding new ones
- Users can edit an ejected built-in's system prompt, `model`,
  `temperature`, and `tools` via its markdown file.
- Users can **add their own** agents by dropping a new `<name>.md` (with
  frontmatter) into an agents dir. Respect the `mode` field
  (primary/subagent/all) for reachability, following existing delegation
  wiring (`task` for subagents). Custom agents are subject to the same
  invariant validation as edited built-ins (below).
- Built-in agents in scope: every bundled agent **except the docs
  pipeline** — enumerate from the actual built-in set in
  `src/engine/builtin/mod.rs` (today: `coder`, `explore`,
  `Build`), minus the docs resolver/answerer. Note the cast
  documented in `CLAUDE.md` lists `Plan` too, but no such
  agent ships yet — drive the in-scope list off the code, not the doc.

### Docs pipeline — fully special-cased, not exposed
- The `docs` two-stage pipeline (Docs.1 resolver / Docs.2 answerer,
  `src/engine/docs_pipeline.rs`, routed in `src/engine/driver.rs`) stays
  **entirely hardcoded/embedded**. It does **not** appear in the agent
  editor or the agents list, and its prompts/wiring/sandboxed tool set are
  not user-editable. Leave it as-is.

### Tool grants — editable, invariants enforced
- The `tools:` frontmatter is editable, but every loaded agent definition
  (edited built-in or custom) is **validated against core invariants** at
  load time, with a clear, actionable error on violation:
  - **Single-writer:** write/lock/edit tools (the file-mutating + lock
    tools that today only `coder` holds) may be granted to **at most the
    one writer** in a delegation tree. Reject a non-coder agent requesting
    write/lock tools. (See the single-writer design rule in `CLAUDE.md` /
    GOALS §3a — the lock manager assumes one writer per delegation tree.)
  - **Docs-answerer sandbox:** N/A to user agents since docs is not exposed,
    but do not allow any user agent to acquire the answerer-only sandboxed
    `grep`/`glob` tools (those are docs-answerer-only per `CLAUDE.md`).
  - Unknown tool names → reject with the offending name backticked.
- Validation errors use the project error-style conventions (backticks for
  identifiers/literals, single quotes reserved for char literals).

## Edge cases & UX decisions

- **First run / unmodified defaults:** nothing on disk; agents resolve from
  embedded. No materialization.
- **Eject when an override already exists:** do not clobber the user's file;
  open/select the existing one.
- **Reset with no overrides present:** no-op (still safe to confirm).
- **Custom agent name collides with a built-in name:** treat as an override
  of that built-in (same name = override), consistent with the overlay
  model — not a separate agent.
- **Malformed agent file (bad YAML / missing required field):** fail that
  agent's load with a clear error naming the file (`source` path); do not
  silently fall back in a way that hides the user's mistake.
- **Invariant violation:** reject the definition with the specific reason;
  do not silently strip the offending tool.

## Forward-compatibility: defensive/normal LLM modes

A future feature (its own deferred prompt,
`deferred-prompts/llm-modes-defensive-normal.md`) will add **defensive** vs
**normal** LLM modes: an explicit config toggle (defaulting to defensive)
that conditionally varies tool-description verbosity, per-agent prompt
content, and delegation/subagent shape based on model strength. **Do not
build modes in this work** — but **do not foreclose them.** Concretely:

- **Do not overload the existing `mode` field.** `AgentDef.mode`
  (`primary`/`subagent`/`all`) is reachability and stays that way. The
  defensive/normal axis is a *separate* concept; the future feature will use
  a distinct name (e.g. `llm_mode` / steering profile). Leave that name
  free — don't repurpose `mode` or grab a colliding frontmatter key.
- **Keep prompt resolution able to grow a per-mode variant later.** The
  agent's system prompt today is the whole markdown body. Don't hardcode the
  assumption that the body is *always* a single monolithic, mode-independent
  prompt. Resolve the prompt through a path that a future mode parameter
  could thread through (i.e. "give me agent X's prompt" rather than reaching
  straight for `def.prompt` in scattered call sites). Do **not** invent a
  per-mode body syntax now — just don't paint resolution into a corner.
- **Settled future disk layout (do not build now, do not foreclose).** A
  single-mode agent is a flat file `<agents-dir>/<name>.md`. A multi-mode
  agent is instead a **directory** `<agents-dir>/<name>/` containing one file
  per mode (e.g. `<name>/normal.md`, `<name>/defensive.md`). This work ships
  only the flat-file form, but write agent-name → path resolution so it can
  later accept "name resolves to either `<name>.md` **or** a `<name>/`
  directory of per-mode files" without a rewrite. Don't special-case the
  directory form yet; just don't assume a name always maps to exactly one
  `.md` file. Eject continues to write the flat `<name>.md`.
- **Keep tool-description rendering centralized,** so a future global mode
  toggle can adjust verbosity in one place rather than requiring a
  per-agent change. (Tool grants themselves are NOT mode-dependent — only
  how their descriptions render — so the `tools:` schema needs no mode
  awareness.)
- The unmodified single-mode file written by eject must remain valid and
  unchanged in meaning once modes land — additive only.

## Expected UX / acceptance

- A user can open `/settings` → Agents, see the cast, eject `coder` to
  `.cockpit/agents/coder.md`, edit its prompt/tools, and have the change
  take effect on next agent run.
- Granting write tools to `explore` is rejected with a single-writer error.
- "Reset all built-in agents to default" (after confirm) removes built-in
  overrides and leaves a user's custom `my-reviewer.md` in place.
- Adding a new `my-reviewer.md` makes it discoverable/usable per its `mode`.
- Docs Q&A behaves exactly as before; docs prompts are not listed anywhere.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` all pass. New behavior is covered by tests
  (parsing, override resolution, invariant validation, reset).

## Implementation notes

- `src/agents/mod.rs` (`AgentDef`, `load_from_file`, `list_all`) is the
  intended home for parsing/discovery — flesh out the stubs rather than
  starting fresh. The factory functions in `src/engine/builtin/mod.rs` are
  where hardcoded tool surfaces currently live; the embedded defaults should
  become the fallback `AgentDef`s (carry frontmatter so eject produces a
  faithful, editable file).
- Keep token-economy rules (GOALS §10) intact — agent prompts and any new
  tool/description text stay within the existing budgets.
- This necessarily reserializes how agents are constructed; preserve the
  prompt-cache discipline (don't churn the cached system-prefix
  unnecessarily) and the wire-vs-user transcript split.

## Constraints (non-negotiable)

- Implement **without incurring tech debt** — no shortcuts, no
  TODO-for-later, no half-finished paths. The feature lands complete.
- For any new package, use the **latest stable release** unless this prompt
  says otherwise, and **verify correct API/dependency usage** with
  `kcl ask <package> "<question>"` before wiring it in.
- Hold all `CLAUDE.md` design rules: single-writer file locking, daemon-first
  architecture, cockpit-native config (do not parse opencode files), token
  economy, redaction non-bypassable, cross-platform (Linux/macOS/Windows).
