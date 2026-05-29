# Loop guard: approval prompt on back-to-back identical tool calls

## Goal

Stop the failure mode where a model (especially the ~120k-context OS
models that are cockpit's primary target) gets stuck calling the same
tool the same way over and over. When cockpit detects an immediate
repeat of a tool call, pause and require user approval before running
it again, with per-call and persistent always-allow/always-deny
choices.

## Current behavior

Tool calls dispatch and run with no detection of repetition. A looping
model burns the entire context window re-issuing an identical call.

## Desired behavior

Track the **previous** tool call in each session. When the model emits a
new tool call whose signature is **identical to the one immediately
before it**, do not run it — surface an approval prompt to the user.

### What counts as "the same"

- The comparator is the **canonical `wire_input`** (the post-repair
  form the model actually sent), not `original_input`. Tool name +
  `wire_input` must match exactly.
- Detection is **back-to-back only.** Only the immediately preceding
  tool call is compared. If the model does anything else in between —
  any other tool call — and *then* repeats, that repeat is allowed and
  does not trigger a prompt. A non-consecutive repeat is not a loop.
- The threshold (number of consecutive identical calls before the
  prompt fires) is **configurable in `/settings`**, defaulting to **2**
  — i.e. fire on the first exact repeat. Wire the setting through the
  same config layering the rest of cockpit uses.

### The approval prompt

When triggered in an interactive session, present the user these six
choices (use the existing interrupt/approval plumbing —
`raise_interrupt`-style round-trip from daemon to TUI client; do not
build a parallel prompting path):

- **Accept** — run this one call; no rule saved.
- **Reject** — block this one call; no rule saved.
- **Always accept for this session** — auto-accept future matches this
  session.
- **Always reject for this session** — auto-reject future matches this
  session.
- **Always accept for this project** — persist auto-accept (in
  `.cockpit/`, via normal config layering).
- **Always reject for this project** — persist auto-reject (in
  `.cockpit/`).

### Rule scope (what "always" keys on)

Every always-* rule keys on the **exact call signature** (tool name +
identical `wire_input`). A different repeated call — different tool or
different args — prompts again on its own. Do not key rules on the tool
name alone.

- Session rules live in daemon session state; project rules persist to
  `.cockpit/` and are read back on later sessions in the same project.
- Define precedence explicitly and implement it: a matching
  session-scoped rule and a matching project-scoped rule for the same
  signature should resolve deterministically (decide and document which
  wins).

### On reject

A rejected call (one-off or via an always-reject rule) returns a
**guidance error** as the tool result: explain that the call was blocked
because it repeated the immediately preceding call (a likely loop) and
instruct the model to try a different approach. This must read as a
normal tool-result error to the model so it can change course — not a
hard session abort. Reserve any prose `Error:` framing per the existing
wire-vs-user transcript conventions.

### Non-interactive / headless runs

When there is no attached TUI client (`cockpit run`, daemon with no
client) and a back-to-back repeat is detected:

1. If a matching always-accept/always-reject rule (session or project)
   exists, honor it.
2. Otherwise **reject** the repeat (same guidance-error result as
   above). There is no human to prompt, and silently re-running the loop
   would bleed the whole context window. Do not block waiting for input.

## Expected UX / acceptance

- An interactive model that fires the same call twice in a row hits the
  approval prompt with all six options; choosing **Accept** runs it once
  and re-prompts on the next identical repeat; **Always accept for this
  project** runs it and never prompts for that exact signature again,
  including in a fresh session in the same project.
- Inserting any other tool call between two identical calls suppresses
  the prompt entirely.
- A headless run with no rule auto-rejects the repeat and the model
  receives the guidance error.
- The threshold is adjustable from `/settings`.

## Constraints

- Implement without incurring tech debt — no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Preserve the wire-vs-user transcript split: the saved tool-call row
  must still carry `wire_input` + `original_input` + `recovery`; loop
  detection reads `wire_input` and must not corrupt either form.
- Reuse the existing interrupt/approval and config-layering machinery;
  do not introduce a second prompting or persistence path.
