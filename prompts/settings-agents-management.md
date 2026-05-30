# /settings → Agents: view, edit, delete, reset

## Goal

Make the `/settings → Agents` page a full management surface: view every
agent (builtin + custom) with its effective model, **edit** an agent's
markdown definition in place, **delete** user-created agents (builtins
can never be deleted — only reset), and **reset** an overridden builtin
back to its embedded default.

## Current behavior

- `src/tui/settings/agents_page.rs` already lists the five builtins
  (`Build`, `coder`, `explore`, `Plan`, `plan-author` per
  `src/agents/builtin_defs.rs`) — marked when overridden — followed by
  custom agents discovered via `list_all(cwd)` in `src/agents/mod.rs`.
  `AgentKind` distinguishes `Builtin { overridden }` vs `Custom`.
- Actions today: `enter`/`e` ejects a builtin (writes
  `.cockpit/agents/<name>.md` via `eject_builtin()`) or selects an
  existing custom agent; `R` arms a reset-**all** confirmation for
  builtin overrides. There is **no inline edit, no delete, no per-agent
  reset.**
- Agent files are markdown with YAML frontmatter
  (`split_frontmatter`/`parse_agent` in `src/agents/mod.rs`): fields
  `description`, `mode`, `model` (`provider/model` slash form — the
  canonical convention from `split_provider_model` in
  `src/config/provider.rs`), `temperature`, `tools`, `permission`.
- The reset state machine (`src/tui/settings/reset.rs`,
  `ResetButton`/`ResetOutcome`) and other pages
  (`tools_page.rs`, `ui_page.rs`, `skills_page.rs`) are the patterns to
  follow.
- Vim mode is default-on in the composer (CLAUDE.md design rules).

## Desired behavior

### Edit

- Editing an agent opens its on-disk `.cockpit/agents/<name>.md` for
  modification, choosing the editor as follows:
  1. If `$EDITOR` is set, launch `$EDITOR` on the file (suspend/restore
     the TUI cleanly around the external process).
  2. Else, if the user has vim mode enabled, edit the file in an
     **in-TUI vim-mode text editor** (reuse the composer's vim-mode
     editing machinery).
  3. Else (no `$EDITOR`, vim mode off), edit in the in-TUI editor with
     plain (non-vim) keybindings — no dead end.
- **Editing a builtin auto-ejects first**: if the selected agent is a
  non-overridden builtin, copy the embedded default to
  `.cockpit/agents/<name>.md` (existing `eject_builtin()` path), then
  open that file. After editing, re-parse and refresh the row so the
  "overridden" marker and effective model update.
- The agent's **model** is set by editing the `model:` frontmatter field
  in this same flow (no separate model picker). Display each agent's
  effective model in the list so the current value is visible.
- After an edit returns, validate the file via `parse_agent`; on a parse
  error show the error inline (backticked identifiers) and keep the
  user on the page rather than silently accepting a broken agent.

### Delete

- Custom agents can be **deleted**: remove the `.cockpit/agents/<name>.md`
  file, behind a two-step arm→confirm guard consistent with
  `ResetButton`.
- Builtins can **never** be deleted. For an overridden builtin, the
  destructive action is **reset** (remove the override file, reverting to
  the embedded default), not delete. A non-overridden builtin offers
  neither delete nor reset.

### Reset

- Keep the existing reset-**all** action, and add **per-agent reset** for
  the highlighted overridden builtin (arm→confirm), deleting just that
  one override file.

## Edge cases & decisions

- **Layered configs**: edit/delete/reset operate on the agent file in the
  appropriate `.cockpit/agents/` layer for the current cwd (the same
  layer `eject_builtin()` writes to / `list_all` reports). Don't touch
  files in other layers.
- **External-editor failure** (non-zero exit, missing binary): report it
  inline and leave the file unchanged; restore the TUI to a clean state.
- **Empty list / no custom agents**: page renders normally; delete is
  simply unavailable for builtins.
- **Concurrent edit**: re-read the file from disk before re-parsing on
  return (don't trust stale in-memory state).
- **Model-string format**: any model value shown or written uses the
  `provider/model` slash convention; if existing agent parsing/docs
  accept a colon form, reconcile to slash.

## Expected UX / acceptance

- In `/settings → Agents`, each row shows the agent name, builtin/custom
  (and overridden) status, and its effective model.
- Selecting edit opens `$EDITOR` (or the in-TUI vim editor when `$EDITOR`
  is unset and vim mode is on); editing a pristine builtin first ejects
  it; saving an invalid file surfaces a parse error and does not corrupt
  the page.
- A custom agent can be deleted (arm→confirm); a builtin cannot be
  deleted but an overridden one can be reset per-agent or via reset-all.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Token economy is non-negotiable (GOALS §10).

## Notes

- Per-agent `model` frontmatter and `resolve_agent_model()` already
  exist; this prompt makes that field viewable/editable from the TUI and
  standardizes the model-string format — it does not add the resolution
  logic (that and the plan-level override live in
  `prompts/plan-duplication-and-model-override.md`).
