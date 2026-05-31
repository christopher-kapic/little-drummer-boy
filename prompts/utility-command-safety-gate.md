# Utility-model command-safety gate (`auto` approval mode)

## Goal

Add an `auto` **approval mode** in which the utility model evaluates
each shell command and network tool call — with **no conversation
context** — for safety before it runs: safe → run without prompting,
unsafe → escalate to the user. The evaluator also decides whether the
call's **result** must be re-checked for prompt injection (e.g. a
command that fetches a tweet); flagged high-risk results are blocked
pending user approval.

## Dependency

Builds on `prompts/utility-prompt-injection-detection.md`, which
provides the shared injection-check mechanism (random-hex-key wrapping +
`risk` tool + user-editable check-prompt) and the utility-model helper.
Reuse that mechanism for the result re-check — do not reimplement it.

## Current behavior

- Approval layer: `src/approval/mod.rs` (`classify()`, `GrantStore`,
  `Approver::approve_command()/approve_path()`).
- `src/tools/bash.rs` currently auto-allows (v0 bootstrap); the
  approval-mode cycling (Shift+Tab) and `exec_approval` flow are noted
  as planned (plan §3e). This task builds the `auto` mode within that
  approval framework.
- Utility-model one-shot calls: `Model::text_completion()` in
  `src/engine/model.rs`.

## Desired behavior

### `auto` approval mode

- A new approval mode `auto`, selectable alongside the planned
  manual / yolo modes. When active:
  - Each **bash** command and each **network tool call**
    (`webfetch`, `mcp_invoke`) is sent to the utility model for a
    safety verdict. The evaluator sees **only that single command/call**
    — no conversation history; judge it on its own merits.
  - Verdict **safe** → execute without prompting. Verdict **unsafe** →
    escalate to the user via the existing approval prompt.
- In **manual** mode the user approves everything (gate not invoked);
  in **yolo** everything runs (gate bypassed). The gate is the engine of
  `auto` mode specifically.
- Scope is **bash + `webfetch` + `mcp_invoke`** only. Other tools
  (`read`, `edit`, intel tools, etc.) are out of scope here.

### Result injection re-check

- The same safety evaluation returns whether the call's **result**
  should be checked for prompt injection (set true for calls that pull
  in external/untrusted content).
- When flagged, after the tool runs, route the result text through the
  shared injection-check mechanism (hex-key-wrapped, `risk` tool).
  - Result rated **high** → **block and ask the user**, with the same
    override UX as the prompt-injection block (allow through / drop /
    edit).
  - Result rated **medium** → deliver with a warn chip. **Low** →
    deliver normally.
- Only re-check results the evaluator flagged — never re-check every
  result (token economy).
- Both the safety eval and the result re-check are single-shot,
  history-free utility-model calls.

## Edge cases & decisions (settled)

- **Utility model unset/unavailable in `auto` mode:** fail safe — treat
  every gated call as requiring user approval rather than silently
  running it. Surface that the safety gate is unavailable.
- The result-injection block must preserve the wire-vs-user transcript
  split (the result is still recorded; see GOALS §14).
- **Naming-collision callout:** this `auto` *approval mode* is distinct
  from the `auto` *router agent*
  (`prompts/auto-router-agent.md`). Keep them clearly separate in code
  and UI so they are never conflated; if a label would be ambiguous to
  the user, disambiguate the approval-mode label while preserving the
  user's "auto" intent.

## Expected UX / acceptance

- In `auto` mode, a benign `ls` runs without a prompt; a destructive or
  suspicious command escalates to user approval.
- A `webfetch` flagged by the evaluator gets its result injection-
  checked; a high-risk result is withheld behind the override prompt.
- With the utility model unset, `auto` mode falls back to asking the
  user for every gated call.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Honor token economy (GOALS §10): one-sentence tool descriptions,
  noun-phrase parameter descriptions, base system prompt ≤ ~400 tokens.
