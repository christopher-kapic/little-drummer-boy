# Planning mode — `Plan` authoring flow

**Prompt 2 of 4 in the planning-mode set. Depends on prompt 1**
(plan/step storage + `add_step` and related tools). Implement after
prompt 1 lands.

## Goal

Implement the conversational planning experience: how
`Plan` turns a feature request into a persisted plan (a DAG
of steps, prompt 1's substrate), by interviewing the user about each
subfeature through dedicated interactive subagents. This is cockpit's
deterministic answer to the ad-hoc `prompts/` + `handle-prompts`
workflow.

Read first: `plan.md §4.6.d` (bundled cast, the `Plan`
row), `plan.md §3d` (subagent modes — noninteractive / interactive /
fork — and `defer_to_orchestrator`), `plan.md §3d-bis` (the
`Build` episode-sequencing precedent), and the
`interrupt_schema` / question model (`raise_interrupt` with
`single`/`multi`/`freetext` kinds, freetext allowed by default).

## What exists today

- There is **no `orchestrator_plan.md` builtin agent file yet** — only
  `src/engine/builtin/orchestrator_build.md` (plus `coder`, `explore`,
  `docs_resolver`, `docs_answerer`). You will author the
  `Plan` agent prompt as part of this work.
- The `Plan` display color is currently hardcoded
  `Color::Magenta` at `src/tui/history.rs:217`.

## Desired behavior

### `Plan` agent

`/plan` swaps the primary agent to `Plan` (the planner);
`/build` swaps to `Build`. The planner owns the
conversation when the focus is *deciding what to do*. It authors and
mutates plans (prompt 1's tools); it does **not** write code. It can
also **start** a plan executing (hand off to the executor — prompt 4).

**Color:** set `Plan`'s display color to `#f8d749`. In
`src/tui/history.rs:217`, replace `Color::Magenta` with
`Color::Rgb(0xf8, 0xd7, 0x49)`. If a broader agent-color convention is
introduced, route this through it; otherwise the direct change is fine.

### Authoring flow (the core)

1. **Existing-vs-new plan.** On a feature request, the planner lists
   **pending + in-progress** plans (prompt 1's list/inspect tool) and
   judges fit:
   - If the feature fits an existing plan, **ask** the user: append to
     that plan, or start a new one? (`AskUserQuestion`-style; freetext
     always allowed.)
   - If it detects what would be a **cross-plan dependency** (the new
     feature depends on work in another *unimplemented* plan), it
     **strongly encourages appending to that existing plan** rather than
     modeling a plan-to-plan dependency (we deliberately don't model
     those — prompt 1).
   - If the dependency is on a plan whose **branch already exists but is
     not yet merged into the new plan's base branch**, **warn the user**
     and offer to base the new plan on that unmerged branch. Detect with
     a single `git merge-base --is-ancestor <plan-branch> <base>` check.

2. **Decompose + confirm.** The planner breaks the feature into
   high-level **subfeatures**, ordered roughly by implementation
   sequence (prerequisites first — e.g. DB setup before writes), and
   asks the user to confirm the subfeature strategy before proceeding.

3. **Per-subfeature interactive interviewer (subagent).** For each
   confirmed subfeature, the planner spawns an **interactive subagent**
   (`task(mode: "subagent_interactive", ...)`, `plan.md §3d`) that swaps
   into the foreground and talks to the user directly. This subagent —
   call it the **plan-author** (working name; a new bundled agent, see
   "Cast" below) — does, for its one subfeature:
   - Grills the user on implementation details, `/draft-prompt`-style
     (the `.claude/skills/draft-prompt` skill is the model for the
     interview quality: force every value-level decision, offer
     "leave it to the agent" to defer deliberately).
   - Proposes any packages it thinks are needed and **asks the user to
     confirm** before they're baked into a step.
   - **Infers each test's concurrency class from the project** (e.g.
     spots a test that binds a fixed port → `exclusive: "port:NNNN"`)
     and **confirms with the user** before recording it.
   - Breaks the subfeature into **small steps**, each sized to be
     implementable within ~100k context (this is **advisory guidance**,
     not an enforced limit — do not add a hard size check), and records
     them with `add_step` (prompt 1), including dependency refs and
     tests.
   - **Out-of-scope drift:** if the user asks for something outside this
     subfeature's scope, the subagent does **not** silently expand. It
     calls `defer_to_orchestrator(message)` (`plan.md §3d`) to append to
     its deferred-log buffer and continues its assigned work. On the
     subagent's completion, the planner ingests `{ report, deferred_log
     }` and addresses each deferred item (new subfeature, answer, or
     clarifying question) — exactly the `Build` pattern.

   **Token-economy rationale (why a subagent, not the planner itself):**
   the verbose interview lives in the subagent's context and is
   discarded on return; the planner only ingests a short capped report
   plus the steps written to the DB. Across many subfeatures the
   planner's context stays lean (GOALS §10 subagent-report caps).

4. **Branch selection (end of flow).** Once every subfeature has been
   interviewed and all steps added, the planner asks which branch to
   implement on. Offer: **the current branch** and
   **`${planBranchRoot}/<suggested-feature-branch>`** (`planBranchRoot`
   from config, default `cockpit-plan` — prompt 1). The question must
   **always allow manual entry** of an arbitrary branch (freetext
   interrupt). The answer is written to the plan's branch policy
   (prompt 1).

### Strategy seam (forward-looking — build the seam, not the feature)

The choice "interactive subagent per subfeature" (used here) vs.
"episode sequencing" (the `Build` mechanism) will later
become an **LLM-strategy** setting: `defensive` (weak models →
interactive subagents, this prompt) vs. `normal` (strong models →
episode sequencing). **Implement only the interactive-subagent path
now**, but structure the per-subfeature spawn behind a clean seam so the
episode-sequencing variant can slot in later **without rework**. Do not
build the strategy system, per-strategy tool definitions, or
episode-sequencing here — just don't hardcode in a way that blocks them.
See `design-need-to-discuss-or-test.md` (add an entry for LLM
strategies as part of this work).

## Cast change (deliberate, user-approved)

This adds a **6th bundled agent** (the `plan-author` interviewer),
beyond the current five. `CLAUDE.md` says resist growing the cast — this
expansion is intentional and approved. Update `plan.md §4.6.d`'s cast
inventory and `GOALS.md §3a` accordingly. The `plan-author`'s tool
surface is the planning/interview tools (`add_step` etc.,
`defer_to_orchestrator`, the question/interrupt tools) — **not**
`write`/`edit` and **not** code-writing delegation; it authors plan
structure only. Confirm the final agent name with the user if you'd
prefer something other than `plan-author`.

## Edge cases & decisions (settled)

- Interactive subagent per subfeature (option i), chosen for context
  minimization. Episode sequencing is deferred behind the strategy seam.
- ~100k step sizing is advisory guidance, not enforced.
- Out-of-scope handling reuses `defer_to_orchestrator` — no new
  bubble-up mechanism.
- Branch question always permits manual freetext entry.

## Expected acceptance

- `/plan` swaps to `Plan`, shown in `#f8d749` in the TUI
  chrome/history.
- A feature request runs end-to-end: fit-check (append/new, with the
  unmerged-branch warning when applicable) → confirmed subfeature
  list → one interactive `plan-author` per subfeature that interviews,
  confirms packages, infers+confirms test concurrency, and adds
  correctly-dependency-ordered steps → branch-selection question with
  current / `${planBranchRoot}/…` / manual options → branch policy
  persisted.
- Out-of-scope asks during an interview surface back to the planner via
  the deferred log and get addressed, not silently absorbed.

## Design-doc updates (do as part of this work)

Update `plan.md §4.6.d` (add `Plan` + `plan-author` to the
cast), `GOALS.md §3a`, and add the LLM-strategy axis to
`design-need-to-discuss-or-test.md`.

## Constraints (non-negotiable)

Implement without incurring tech debt — no shortcuts, no
TODO-for-later, no half-finished paths. For any new package use the
latest stable release unless this prompt says otherwise, and verify
correct API/dependency usage with `kcl ask <package> "<question>"`
before wiring it in. Honor token economy (GOALS §10) and the wire-vs-
user transcript split (GOALS §14). All tools validate through the repair
layer (GOALS §12).
