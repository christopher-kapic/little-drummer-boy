//! Merge-resolver input assembly (worktree-proposal.md §5, prompt 4).
//!
//! When the merge queue hits a conflict **or** a post-rebase test failure, it
//! hands off to a specialized, narrow-context resolver. The key design point
//! (worktree-proposal.md "things to flag") is that the resolver needs
//! **intent, not just markers**: given only conflict markers it would guess,
//! so it receives *what each side was trying to do*.
//!
//! ## Agent choice (CLAUDE.md "keep the cast minimal")
//!
//! The resolver is **not** a new bundled agent. The bundled cast stays the
//! five-agent set (`Build`/`coder`/`explore`/`Plan`/`plan-author`) plus the
//! `docs` pipeline; adding a sixth agent for a focused, single-shot
//! conflict-resolution task isn't warranted. Instead the executor dispatches
//! a **`coder` task** (the single writer — only `coder` holds locks/writes)
//! with a resolver-shaped brief assembled here. `coder` already has the exact
//! tool surface a resolver needs (read/edit/bash to re-run tests) and obeys
//! the single-writer + lock invariants for free.
//!
//! This module builds the brief; the executor's [`super::StepRunner`] hands
//! it to `coder` and reports back resolved / escalated.

/// The fully-assembled context a merge-resolver `coder` task receives. The
/// executor renders this into the `coder` task prompt.
#[derive(Debug, Clone)]
pub struct ResolverBrief {
    /// What the branch being landed was trying to do (its step's intent).
    pub incoming_intent: String,
    /// What the already-landed work on the base was trying to do — the
    /// accumulated intents of the steps already merged into the plan tip
    /// (so the resolver sees *both sides*, not just the incoming one).
    pub base_intent: String,
    /// The conflicted file paths (empty when the handoff was a post-rebase
    /// test failure rather than a textual conflict).
    pub conflicts: Vec<String>,
    /// The incoming branch's full diff against the merge base (surrounding
    /// context the resolver reasons over).
    pub incoming_diff: String,
    /// The base side's full diff against the merge base.
    pub base_diff: String,
    /// The test command(s) the resolver must make green before merging.
    pub test_commands: Vec<String>,
    /// Why the handoff happened — a textual conflict, or a post-rebase test
    /// failure (two independently-green branches that broke each other).
    pub reason: ResolverReason,
}

/// Why the merge queue handed off to the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolverReason {
    /// `git rebase` stopped on a textual conflict.
    Conflict,
    /// Rebase applied cleanly but the post-rebase test re-run failed — a
    /// semantic break with no textual conflict (worktree-proposal.md: the
    /// reason post-rebase testing is non-negotiable).
    PostRebaseTestFailure { output: String },
}

impl ResolverBrief {
    /// Render the brief into a self-contained `coder` task prompt. The
    /// resolver gets both intents up front (the load-bearing input), then the
    /// conflict/failure specifics, the diffs for context, and the exact test
    /// command it must make pass before it may land the branch — and the
    /// escalation contract (raise a `question` / needs_attention item when it
    /// genuinely can't resolve).
    pub fn render_prompt(&self) -> String {
        let mut p = String::new();
        p.push_str(
            "You are resolving a merge in cockpit's plan merge queue. Two pieces of work \
             were each independently green but cannot be combined as-is. Resolve them into a \
             single working tree, make the tests pass, then stop.\n\n",
        );
        p.push_str("## What the incoming branch was trying to do\n");
        p.push_str(&self.incoming_intent);
        p.push_str("\n\n## What the already-landed work was trying to do\n");
        p.push_str(&self.base_intent);
        p.push_str("\n\n## Why this needs resolving\n");
        match &self.reason {
            ResolverReason::Conflict => {
                p.push_str("A textual merge conflict. Conflicted files:\n");
                if self.conflicts.is_empty() {
                    p.push_str("  (none reported)\n");
                } else {
                    for c in &self.conflicts {
                        p.push_str("  - ");
                        p.push_str(c);
                        p.push('\n');
                    }
                }
            }
            ResolverReason::PostRebaseTestFailure { output } => {
                p.push_str(
                    "No textual conflict — the rebase applied cleanly, but the tests failed \
                     after rebasing. The two branches break each other semantically. Test \
                     output:\n",
                );
                p.push_str(output);
                p.push('\n');
            }
        }
        p.push_str("\n## Incoming branch diff (vs merge base)\n```diff\n");
        p.push_str(&self.incoming_diff);
        p.push_str("\n```\n\n## Base-side diff (vs merge base)\n```diff\n");
        p.push_str(&self.base_diff);
        p.push_str("\n```\n\n## Tests that must pass before you land\n");
        for cmd in &self.test_commands {
            p.push_str("  - `");
            p.push_str(cmd);
            p.push_str("`\n");
        }
        p.push_str(
            "\nWhen the tree is conflict-free and the tests pass, you are done — report what \
             you changed. If you genuinely cannot reconcile the two intents (a true design \
             contradiction, not a mechanical conflict), raise a `question` describing the \
             contradiction and stop; a human will decide. Do not guess past a real \
             contradiction.\n",
        );
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief(reason: ResolverReason) -> ResolverBrief {
        ResolverBrief {
            incoming_intent: "add a /metrics endpoint".into(),
            base_intent: "rename the server struct".into(),
            conflicts: vec!["src/server.rs".into()],
            incoming_diff: "+ fn metrics()".into(),
            base_diff: "- struct Server\n+ struct App".into(),
            test_commands: vec!["cargo test".into()],
            reason,
        }
    }

    #[test]
    fn prompt_includes_both_intents_and_test_cmd() {
        let p = brief(ResolverReason::Conflict).render_prompt();
        assert!(
            p.contains("add a /metrics endpoint"),
            "incoming intent present"
        );
        assert!(
            p.contains("rename the server struct"),
            "base intent present"
        );
        assert!(p.contains("cargo test"), "test command present");
        assert!(p.contains("src/server.rs"), "conflicted file present");
        assert!(p.contains("question"), "escalation contract present");
    }

    #[test]
    fn post_rebase_failure_reason_surfaces_output() {
        let p = brief(ResolverReason::PostRebaseTestFailure {
            output: "FAILED: assertion left == right".into(),
        })
        .render_prompt();
        assert!(p.contains("rebase applied cleanly"));
        assert!(p.contains("FAILED: assertion left == right"));
    }
}
