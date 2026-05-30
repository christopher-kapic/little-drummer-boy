# `/plans` — TUI plan browser

**Prompt 3 of 4 in the planning-mode set. Depends on prompt 1**
(plan/step storage). Independent of prompts 2 and 4 — can land any time
after prompt 1.

## Goal

A `/plans` slash command + TUI view that lets the user browse plans and
their steps, modeled on the existing `/sessions` browser.

## What exists today

- Plan/step storage from prompt 1 (`plans` / `plan_steps`, internally
  the `plan.md §4.1` graph plans).
- A `/sessions` browser already exists in the TUI — **match its
  interaction model, layout idiom, and key bindings** so `/plans` feels
  native. Find it under `src/tui/` and follow its patterns rather than
  inventing a new UX.

## Desired behavior

- `/plans` opens a browser listing all plans with: title, status
  (`pending` / `in_progress` / `done`), target branch, step count, and a
  one-line description. Default ordering: active first (in_progress, then
  pending, then done), newest within each group — but defer to whatever
  ordering convention `/sessions` already uses if it differs.
- Selecting a plan shows its **steps** with their dependency structure
  visible (the DAG — show each step's prerequisites), per-step status,
  and each step's tests (with `phase` and `concurrency` shown, e.g. an
  `exclusive: port:8080` badge). Render the dependency ordering clearly
  enough that the user can see what blocks what.
- Read-only browsing in v1. Authoring happens through `Plan`
  (prompt 2); plan *execution controls* (start/pause/status of a running
  plan) belong to prompt 4 — leave a clean integration point for those
  controls but do not build execution here.

## Edge cases & UX decisions (settled)

- Read-only for v1 (no editing/deleting plans from this view).
- Mirror `/sessions` for everything not specified here (theming, key
  bindings, empty-state, scrolling) — do not diverge gratuitously.
- Empty state (no plans yet): a brief message pointing the user to
  `/plan` to create one.

## Expected acceptance

- `/plans` lists plans with the fields above and feels consistent with
  `/sessions`.
- Drilling into a plan shows steps, their dependency prerequisites,
  per-step status, and per-test `phase` + `concurrency`.
- Empty state renders cleanly.

## Design-doc updates (do as part of this work)

Note the `/plans` command in `plan.md` (alongside the §4.1 CLI surface)
and `GOALS.md` if the slash-command inventory is tracked there.

## Constraints (non-negotiable)

Implement without incurring tech debt — no shortcuts, no
TODO-for-later, no half-finished paths. For any new package use the
latest stable release unless this prompt says otherwise, and verify
correct API/dependency usage with `kcl ask <package> "<question>"`
before wiring it in. Keep TUI chrome rules intact (GOALS §1a) — `/plans`
is a view, it does not alter the fixed chrome.
