---
name: draft-prompt
description: Turn a rough task description into a clean, structured prompt saved to prompts/ for a future agent to read. Use when the user wants to draft, clean up, or save a prompt to hand off later (NOT when they want the task implemented now).
---

# Draft an agent prompt

Help the user turn a rough description of a task into a clean,
self-contained prompt file in `prompts/`, ready to hand to another agent
later. You are **writing the prompt, not doing the task** — never start
implementing the feature described.

## Workflow

1. **Understand the intent.** Read the user's rough description. The
   output is a prompt a fresh agent will read cold, with no memory of
   this conversation, so it must stand on its own.

2. **Force every value-level decision before writing — leave only
   implementation detail to the agent.** The bar: a fresh agent must be
   able to implement the whole task end-to-end without asking the user
   anything. So resolve *with the user now* every decision whose "right"
   answer is a matter of preference or product judgment rather than
   mechanics:
   - scope and tradeoffs (estimate vs. exact, sync vs. async, which
     layer owns the change),
   - edge-case handling (empty input, errors, conflicts, huge inputs,
     concurrent access),
   - UI / UX calls (layout, wording, what's shown vs. hidden),
   - which packages to add, if any — a new dependency is a value-level
     call, so surface candidates and let the user weigh in.
   Ask with `AskUserQuestion`. Always offer "leave it to the agent" so a
   point can be *deliberately* deferred rather than silently guessed.
   Low-level mechanics (function names, file structure, algorithm
   choice) stay with the agent — don't ask about those.
   - Do **not** invent decisions to look thorough.
   - Do **not** ask about things the user already specified.
   - Do **not** pad with questions whose answer doesn't change the work.

3. **Write the prompt.** Structure it for a cold reader. A good default
   shape (drop sections that don't apply):
   - **Goal** — one or two sentences on what to achieve.
   - **Current behavior** — what exists today.
   - **Desired behavior** — what it should do, including any decisions
     the user made when answering clarifying questions.
   - **Edge cases & UX decisions** — the calls the user made in step 2,
     written as settled instructions, not open questions.
   - **Expected UX / acceptance** — observable end state.
   - **Suggested packages** — candidate crates/libs to reach for, if
     any, with a one-line reason each. New deps go on their latest
     stable release unless the user said otherwise.
   - **Constraints (always include)** — verbatim in every prompt:
     implement without incurring tech debt (no shortcuts, no
     TODO-for-later, no half-finished paths); for any new package use
     the latest stable release unless this prompt says otherwise, and
     verify correct API/dependency usage with
     `kcl ask <package> "<question>"` before wiring it in.
   - **Notes** — only constraints/decisions the user actually gave.

4. **Save it.** Write to `prompts/<kebab-case-topic>.md`. Derive the
   filename from the topic.

5. **Report.** Tell the user the path and one line on what's baked in.

## Rules

- **Resolve all value-level decisions up front.** The output must be
  implementable end-to-end with zero further input from the user. If a
  decision was deliberately deferred (user chose "leave it to the
  agent"), say so explicitly in the prompt rather than leaving it
  silent.
- **Every prompt carries the standing constraints block** — no tech
  debt; latest-stable for new deps unless specified; `kcl ask <package>`
  to verify dependency usage. Non-optional, even for tiny tasks.
- **No speculative specifics.** Don't invent file paths, function names,
  or architecture the user didn't provide — that's the implementing
  agent's job, and wrong guesses mislead it. Include specifics only when
  the user gave them or they're genuinely necessary. (Package
  *candidates* are an exception — naming them is expected, since adding a
  dependency is a value-level call the user weighed in on.)
- **Bake in answers.** When the user answers a clarifying question, write
  the decision into the prompt so the future agent doesn't re-litigate
  it.
- **Stay terse.** The prompt should be complete but token-efficient — no
  filler, no restating the obvious. Every line should earn its place.
- **One topic per file.** Separate unrelated tasks into separate prompts.
