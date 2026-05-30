# Plan-status chrome indicator + needs-attention resolver

**Prompt 5 of 5 in the planning-mode set. Depends on prompt 1**
(plan/step storage + the `ready` status), **prompt 3** (`/plans`
browser — the resolver lives inside it), and **prompt 4** (background
execution that raises questions). Implement last.

## Goal

Give the user ambient awareness of background plan execution and a way
to answer the questions/blockers those background agents raise — without
ever pulling a background agent into the foreground. Plan-implementation
agents are **always noninteractive and daemon-resident** (they keep
working with no TUI open — prompt 4); this chrome indicator + resolver
are the human's only window into that work.

## What exists today

- The **caffeinate** glyph proves the exact pattern to reuse: a
  **daemon-broadcast** state (`proto::Event::CaffeinateState`,
  `src/tui/agent_runner.rs`) rendered as an *additive* chrome slot that
  appears only when active and never displaces the fixed slots. Build
  plan status the same way — render **daemon-broadcast state**, not
  TUI-local bookkeeping, so a reconnecting/late-opened TUI shows correct
  state and the v2 remote dashboard gets it for free.
- Chrome rendering: `src/tui/chrome.rs`. The fixed slots (cwd +
  git-branch pill, GOALS §1a) are non-displaceable; the branch pill is a
  *filled* badge (`▐ branch ▌`, xterm-220 yellow).
- The `needs_attention` queue, the `question` tool
  (`src/tools/question.rs`), and the question dialog
  (`src/tui/dialog/question.rs`) already exist.
- An in-flight `prompts/question-dialog-ux-overhaul.md` reworks the
  question dialog — **consume whatever it lands; do not fork a second
  dialog.**
- `/plans` browser (prompt 3).

## Desired behavior

### Chrome indicator (additive slot)

A new chrome slot, **project-scoped** (this repo's unfinished plans),
driven by daemon-broadcast state, composed around the fixed slots like
the `☕` glyph. It shows up to three segments; **each segment is omitted
when its count is zero**, and when all three are zero the **slot is
absent entirely** (normal coding sessions stay uncluttered):

- **ready** plans (status `ready` — authored + branch chosen, queued for
  the single execution slot)
- **in-progress** plan (status `in_progress` — the one executing; **≤1
  per project**, since only one plan runs at a time)
- **interruptions** (count of pending `needs_attention` items across
  this project's unfinished plans)

Color: **plan-yellow `#f8d749`** (`Color::Rgb(0xf8, 0xd7, 0x49)`). It
won't be confused with the branch pill because the branch is a *filled*
badge while this slot is *unfilled* colored glyph+number text. Propose
clear glyphs per segment (e.g. in-progress ▶, ready ⧖, interruptions ?)
and pick icons consistent with the existing theme; the interruptions
segment should read as the actionable, attention-grabbing one (it's the
thing blocking progress). `done`/`draft` plans are never shown.

### Needs-attention resolver

A view that lists this project's pending `needs_attention` items —
each showing which **plan**, which **step**, and the question/blocker
text — and lets the user answer inline, **reusing the question dialog**
(`src/tui/dialog/question.rs` as reworked by the dialog overhaul). Since
background agents use the `question` tool with a free-text response
(prompt 4), answers are typically free text. Answering an item resolves
the paused step, which resumes from where it stopped without blocking
siblings (`plan.md §4.1`).

Two entry points:
- **`/plans answer`** — opens the resolver directly.
- A **button in the `/plans` browser** (prompt 3) — opens the same
  resolver.

The resolver is a slice of `/plans` (prompt 3), not a separate
top-level surface. Its item scope matches the chrome counter scope (this
project).

## Edge cases & decisions (settled)

- Indicator is additive and daemon-broadcast-driven; never displaces the
  fixed chrome (GOALS §1a).
- Segments omitted when zero; whole slot absent when nothing unfinished.
- Plan-yellow `#f8d749`; distinguished from the branch pill by fill, not
  hue.
- One queue, one counter; no new interrupt tool (background agents use
  `question` — prompt 4).
- Resolver reached via `/plans answer` and a `/plans`-browser button;
  reuses the (overhauled) question dialog; scoped to this project.

## Expected acceptance

- With background plans running, the chrome shows the correct
  ready/in-progress/interruption segments, omitting zero segments; with
  nothing unfinished, no slot appears.
- The indicator updates from daemon broadcast — a TUI opened *after* a
  plan started, or reconnected, shows correct state.
- A background agent raising a `question` increments the interruptions
  count; `/plans answer` (and the browser button) opens the resolver
  listing it with plan/step context; answering it resumes the paused
  step and decrements the count.
- The slot renders in `#f8d749` and is visually distinct from the branch
  pill.

## Design-doc updates (do as part of this work)

Document the plan-status chrome slot in `plan.md`/`GOALS.md §1a` as an
*additive* slot (alongside the `☕` exception, not a replacement for any
fixed slot), and note `/plans answer` + the resolver. Record the
"interruptions reuse the `question` tool, no new interrupt tool"
decision.

## Constraints (non-negotiable)

Implement without incurring tech debt — no shortcuts, no
TODO-for-later, no half-finished paths. For any new package use the
latest stable release unless this prompt says otherwise, and verify
correct API/dependency usage with `kcl ask <package> "<question>"`
before wiring it in. Do not make the fixed chrome slots configurable or
displaceable (GOALS §1a). Drive the indicator from daemon-broadcast
state, not TUI-local state, so it is reconnect-safe and remote-dashboard
ready.
