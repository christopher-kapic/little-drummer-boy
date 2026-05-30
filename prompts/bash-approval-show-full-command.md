# Show the full bash command in the approval dialog

## Goal

When the user is asked to approve a `bash` tool call, show the **full
verbatim command** that is about to run, not just the grant-key. Today
the dialog shows only `` Run `cd`? `` — the user cannot tell which
directory `cd` is entering, or what any command's arguments are.

## Current behavior

`src/approval/` reuses the generic interrupt/question path to prompt for
command approval:

- `approval/classify.rs` parses the command into a bash-grammar AST
  (`brush-parser`, `sh_mode`) and decomposes it into the *simple
  commands* it would run. Each yields an `ApprovalKey` = `argv[0]` +
  first subcommand token (`gh pr`, `cargo build`, or just `cd`).
  Arguments beyond the subcommand are **discarded** — they are not part
  of the key. The `compound` flag marks chained/piped/grouped/
  substituted sources.
- `approval/mod.rs::approve_command(command)` classifies, then loops
  over every constituent simple command calling `approve_one`. An
  already-granted command is allowed silently; each ungranted (or
  wrapper) constituent triggers a prompt.
- `approve_one(info, full_command)` currently **ignores** `full_command`
  (`let _ = full_command;`) and prompts with only
  `info.key.as_storage_str()` (e.g. `cd`).
- `prompt(label, wrapper)` builds an `InterruptQuestion::Single` via
  `scope_question(label, wrapper)` — the prompt string is
  `` Run `{label}`? `` (or the wrapper variant) — and raises it through
  `db.raise_interrupt_questions` / `interrupts.emit_raised`.
- The TUI renders it in `src/tui/dialog/question.rs` (generic question
  dialog); the prompt is shown as a single bold line
  (`question.rs` ~330). There is no command-detail region today.

So the full command string already reaches `approve_one` — it's simply
not displayed — but the per-constituent text/spans are **not** preserved
by the classifier.

## Desired behavior

For each bash-approval prompt, the dialog shows:

1. **Heading (unchanged):** `` Run `{grant-key}`? `` — keep the existing
   wording; it doubles as the shorthand for what a "remember" choice will
   grant. Wrapper variant keeps its "Wrappers can't be remembered." note.
2. **The full verbatim command** the agent proposed, shown below the
   heading as a distinct, readable block (monospace/quoted styling
   consistent with the rest of the TUI).
3. **For compound commands** (more than one constituent will be
   prompted): a `step N of M` indicator, and the **current step's
   constituent highlighted within the full command** (e.g. underline /
   distinct style on the `cargo build` span of
   `git push origin main && cargo build`). `M` = the number of
   constituents that actually trigger a prompt (skip already-granted
   ones); `N` is the current one's position in that sequence. A
   single-constituent command shows no step indicator and no highlight.

The scope options (`Yes, once` / `Yes, for this session` /
`Yes, for this project` / `No`, or the wrapper's single `Yes, once`) are
unchanged. The grant semantics are unchanged — a "remember" choice still
records the **key**, not the full command. The heading already conveys
this; do not add a second redundant "this grants…" line.

### Highlighting the current step

Highlighting the exact constituent substring requires the source span of
each simple command within the original string. The classifier does not
track this today. Preferred: extend `classify`/`SimpleCommandInfo` to
carry the source span (byte/char range) of each constituent, sourced from
`brush-parser`'s AST position info if it exposes reliable spans — verify
with `kcl ask brush-parser "<question>"` whether AST nodes expose source
positions. Thread the span through to the dialog and render the
highlight.

If — and only if — `brush-parser` does not expose reliable source
positions, fall back gracefully: show `step N of M` plus the current
step's grant-key (`This step: cargo build`) **without** an inline
caret/underline, still showing the full command verbatim for context. Do
not ship a fragile substring-search heuristic to fake the highlight —
that is the silent-corruption hazard the project forbids. State in the PR
which path you took and why.

### Long / multi-line commands

Big heredocs and long one-liners must not blow out the dialog:

- By default, **truncate** the command block to a sensible height (fit
  the dialog without pushing the options off-screen) with a clear
  indicator of how much is hidden (e.g. `… 23 more lines`).
- Provide an **expand affordance**: a discoverable keybinding that
  toggles the command block to show the full contents, **scrolling**
  within the block when it exceeds the available height. Show the
  binding as a hint in the dialog (e.g. a footer key legend) so it is
  discoverable. Pick keys that don't collide with the dialog's existing
  option-select / confirm / cancel bindings; reuse existing TUI scroll
  conventions where they exist.
- Highlighting (above) must still point at the right span when the block
  is expanded/scrolled.

## Scope

- **In scope:** bash command approval prompts only — the
  `approve_command` → `approve_one` → `prompt` → `scope_question` path
  and its TUI rendering.
- **Out of scope:** `approve_path` (already shows the full path) and
  `approve_repeat` (the loop-guard prompt). Leave both unchanged.

## Implementation notes

- The full command already flows into `approve_one(info, full_command)`;
  thread it (and the step index/count + the constituent span, if
  obtained) through `prompt` and into the interrupt payload.
- The approval prompt rides the generic interrupt/question schema
  (`InterruptQuestion` / `InterruptQuestionSet`). Decide whether to
  extend that schema with an **optional** structured "command detail"
  payload (full command text + optional highlight span + step N/M) that
  the dialog renders specially, or to add a dedicated approval-dialog
  rendering path. Either is acceptable; keep the generic question dialog
  working unchanged for non-approval questions, and keep the change
  cache-safe and schema-additive (optional fields, no breakage to
  existing interrupts).
- Keep `prompt_description(...)` (the persisted/emitted description used
  by non-TUI surfaces) informative and consistent — include the full
  command there too so headless/log contexts aren't worse off than the
  TUI.
- Update `scope_question` / `prompt` signatures and all call sites; keep
  `approve_path`'s prompt path working.

## Expected UX / acceptance

- `cd /home/christopher/secret-project` → dialog heading `` Run `cd`? ``
  with `cd /home/christopher/secret-project` shown verbatim below it.
- `git push origin main && cargo build` (neither granted) → two prompts;
  each shows the full command, `step 1 of 2` / `step 2 of 2`, with the
  active constituent highlighted (or the documented fallback).
- A command with a granted first half and ungranted second half prompts
  once, labelled `step 1 of 1`, showing the full command.
- A 200-line heredoc → command block truncated with a `… N more lines`
  indicator and an expand key hinted; expanding shows all lines,
  scrollable.
- Selecting "Yes, for this session" still records the **key**, not the
  full command (existing grant tests still pass).
- Wrapper commands (`bash -c …`, `sudo …`) still offer only `Yes, once`
  and show the full command.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths. The classifier change (if any)
  must be correct and tested, not a heuristic.
- For any new package use the latest stable release unless this prompt
  says otherwise; this task should need no new dependencies
  (`brush-parser`, `ratatui` are already present). Verify any
  API/dependency usage — especially whether `brush-parser` exposes AST
  source spans — with `kcl ask <package> "<question>"` before wiring it
  in.
- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` must all pass. Add tests for the new classifier
  span output (if added) and the step-counting logic.
