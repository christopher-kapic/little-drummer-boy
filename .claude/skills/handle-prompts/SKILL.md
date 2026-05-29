---
name: handle-prompts
description: Work through the prompt files in prompts/ — implement each dependency-free one via a subagent, verify the gates, commit it with its prompt, delete the prompt, and repeat until none remain, then run the tests. Use when the user wants you to "go through the prompts", "handle the prompts directory", or clear out prompts/.
---

# Handle the prompts directory

Drain `prompts/` by implementing each prompt, one at a time, in
dependency order. Each prompt is a self-contained spec written for a cold
reader (the counterpart to the `draft-prompt` skill). Your job is to
**implement** them — fully, with no tech debt — then commit and remove
each one as it lands.

This is the inverse of `draft-prompt`: that skill *writes* specs to
`prompts/`; this one *consumes* them.

## Loop

Repeat until `prompts/` holds no prompt files:

1. **Read every remaining prompt.** List `prompts/` and read each file.

2. **Pick a dependency-free prompt.** A prompt depends on another when it
   can't be correctly implemented until the other's change exists (it
   references behavior/APIs/files another prompt introduces). Choose one
   with **no unimplemented dependencies**. If several qualify, prefer the
   smaller / more self-contained one. If prompts are mutually
   independent, order doesn't matter — just pick one. If a genuine
   dependency cycle exists, surface it to the user rather than guessing.

3. **Implement it — delegate to a subagent to save context.** Spawn a
   `general-purpose` subagent (via the Agent tool) and have it implement
   the prompt end-to-end. This keeps the heavy file-reading and
   edit-churn out of your context. In the subagent prompt:
   - Point it at the prompt file's absolute path and at `CLAUDE.md` for
     project conventions (token economy, wire-vs-user transcript split,
     single-writer, reuse-don't-duplicate, no tech debt).
   - Tell it to implement the spec **exactly as written**, including
     every settled edge-case decision.
   - Tell it **not** to commit and **not** to delete the prompt — you do
     that yourself after verifying.
   - Tell it to touch only files needed for the feature (this repo has
     untracked sibling directories — they are not in scope).
   - Tell it to verify and report the gates (see step 4), and to report
     the changed files, tests added, and the precedence/design decisions
     it made.
   - Run independent prompts in **separate sequential subagents**, not in
     parallel — commits are sequential and parallel worktrees would
     collide on shared files (e.g. `daemon/server.rs`, config).

4. **Independently verify the gates — do not trust the subagent's word.**
   Re-run them yourself; subagents sometimes misreport. All must hold:
   - `cargo fmt --check`
   - `cargo build`
   - `cargo test` — all pass
   - `cargo clippy -- -D warnings` — **zero new** errors.

   ⚠️ This repo carries **pre-existing** clippy errors on clean `master`
   (dead-code / doc lints in unrelated modules). The bar is "introduce no
   *new* ones," not "clippy is clean." Verifying this correctly is
   subtle:
   - A raw error **count** can stay flat while a new error hides behind a
     pre-existing dead-code lint that your now-*used* code resolved.
   - A `file:line` **diff** is noisy: adding code shifts line numbers, so
     pre-existing lints reappear at "new" locations.
   - **The robust check is a before/after diff of the error-*message*
     multiset**, which is immune to both. Capture the message lines on
     the current tree, `git stash push -u` your changes, capture them on
     clean `master`, `git stash pop`, and diff:
     ```bash
     cargo clippy -- -D warnings 2>&1 | grep -E "^error:" | sort | uniq -c | sort -rn > /tmp/after.txt
     git stash push -u -- src/ Cargo.toml Cargo.lock
     cargo clippy -- -D warnings 2>&1 | grep -E "^error:" | sort | uniq -c | sort -rn > /tmp/before.txt
     git stash pop
     diff /tmp/before.txt /tmp/after.txt   # identical ⇒ no new clippy errors
     ```
     If `diff` shows additions, those are genuinely new — fix them before
     committing. (Stash the new untracked migration/files too — hence
     `-u`.)

5. **Review the core for quality.** Skim the subagent's diff on the
   highest-risk logic (the dispatch/integration point, not the
   boilerplate). Confirm the prompt's settled decisions were honored and
   the project's invariants hold (e.g. wire-vs-user split preserved,
   existing machinery reused rather than a parallel path added). Verify
   any non-obvious claim the subagent made (e.g. "the constant cancels
   downstream") against the actual code.

6. **Commit the implementation together with its prompt file.** Stage the
   changed source files (and any new files) **plus the prompt file
   itself**, and commit with a descriptive message. Stage paths
   explicitly — never `git add -A` / `git add .`, because of the
   untracked sibling directories. Follow the repo's commit conventions
   (e.g. the `Co-Authored-By` trailer in `CLAUDE.md`).

7. **Delete the prompt in a follow-up commit.** `git rm` the prompt file
   and commit the removal on its own. ("Commit the prompt and the
   changes, then delete the prompt" — two commits: implement+prompt, then
   delete.)

## When the directory is empty

Run the full test suite once more (`cargo test`) and report the result.
Then summarize: one or two lines per prompt — what it did and its commit
— plus the final gate/test status.

## Rules

- **One prompt per iteration.** Implement, verify, commit, delete, then
  re-list. Don't batch multiple prompts into one commit.
- **No tech debt.** The prompts themselves say so; hold the subagent to
  it. No TODOs, no half-finished paths, no shortcuts.
- **Stage explicitly.** This repo has untracked sibling directories that
  must never be committed. Always `git add <specific paths>`.
- **Don't trust, verify.** Re-run every gate yourself; the
  message-multiset clippy check above is mandatory, not optional.
- **Surface, don't guess.** If you can't determine dependency order, hit
  a real cycle, or a gate fails in a way you can't resolve, stop and tell
  the user rather than committing something half-right.
