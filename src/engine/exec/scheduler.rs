//! Step-level DAG scheduler (plan.md §4.1).
//!
//! A plan is a DAG of steps; this module decides **which steps are eligible
//! to run** given the current run-state. The rule (plan.md §4.1) is exact: a
//! step is `Eligible` iff every step it depends on is `Finished`. Steps with
//! no unfinished dependencies between them run concurrently — that intra-plan
//! step parallelism is where *all* of the executor's concurrency lives
//! (inter-plan parallelism is deliberately rejected; see
//! `prompts/planning-mode-worktree-execution.md`).
//!
//! This is a pure data structure with no async, no git, and no I/O — the
//! [`crate::engine::exec`] driver feeds it the dependency edges + the live
//! per-step states and asks "what can start now?". Keeping it pure makes the
//! eligibility contract trivially testable (and it is, below).

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

/// Runtime lifecycle of one step within a single plan run. Distinct from the
/// coarse persisted `plan_steps.status` (`pending`/`in_progress`/`done`): the
/// scheduler tracks the finer in-flight phases a step moves through while the
/// plan executes. Only [`StepState::Merged`] counts as "finished" for the
/// purpose of unblocking dependents — a step's work is not truly done until
/// its branch has landed on the plan's main worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// Not started; waiting on dependencies or a free worker slot.
    Pending,
    /// A coder is implementing the step in its worktree.
    Running,
    /// Post-step tests are running (or queued) in the step's worktree.
    Testing,
    /// Implemented + green; sitting in the serial merge queue.
    Queued,
    /// The merge worker is rebasing/landing this step's branch.
    Merging,
    /// Branch landed on the plan's main worktree — counts as finished.
    Merged,
    /// Suspended waiting on a human answer (a `question` interrupt). Does
    /// **not** block siblings; only this step's own dependents wait.
    AwaitingHuman,
    /// Terminal failure (merge-resolver gave up → needs_attention). Blocks
    /// dependents; the plan surfaces it and keeps the rest running.
    Failed,
}

impl StepState {
    /// A step is *finished* (its dependents may become eligible) only once
    /// its branch has merged. A `Failed` step is terminal but **not**
    /// finished — its dependents can never run, which is the correct
    /// "downstream of a broken step stays blocked" semantics.
    pub fn is_finished(self) -> bool {
        matches!(self, StepState::Merged)
    }

    /// True for a terminal state (no further transitions): `Merged` or
    /// `Failed`. Used to decide when a plan run has settled.
    pub fn is_terminal(self) -> bool {
        matches!(self, StepState::Merged | StepState::Failed)
    }

    /// True while the step is actively occupying execution machinery (a
    /// worker or the merge worker). Used for the quiescence check: the
    /// `branch_stable` suite runs only when nothing is in-flight.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            StepState::Running | StepState::Testing | StepState::Queued | StepState::Merging
        )
    }
}

/// The DAG of a plan plus the live state of each step. Pure; no I/O.
#[derive(Debug, Clone)]
pub struct Scheduler {
    /// Every step id in the plan.
    steps: Vec<Uuid>,
    /// `step → the steps it depends on` (must finish before it can run).
    deps: HashMap<Uuid, Vec<Uuid>>,
    /// Live per-step state.
    state: HashMap<Uuid, StepState>,
}

impl Scheduler {
    /// Build a scheduler from the plan's step ids and its dependency edges.
    /// Each edge is `(from, to)` meaning *`from` depends on `to`* (from runs
    /// after to) — the exact shape `Db::list_dependencies` returns. Every
    /// step starts [`StepState::Pending`].
    pub fn new(steps: &[Uuid], edges: &[(Uuid, Uuid)]) -> Self {
        let mut deps: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for s in steps {
            deps.entry(*s).or_default();
        }
        for (from, to) in edges {
            deps.entry(*from).or_default().push(*to);
        }
        let state = steps.iter().map(|s| (*s, StepState::Pending)).collect();
        Self {
            steps: steps.to_vec(),
            deps,
            state,
        }
    }

    /// The state of `step`, or `None` if it isn't in this plan.
    pub fn state_of(&self, step: Uuid) -> Option<StepState> {
        self.state.get(&step).copied()
    }

    /// Transition `step` to `next`. No-op (returns `false`) if the step is
    /// unknown.
    pub fn set_state(&mut self, step: Uuid, next: StepState) -> bool {
        match self.state.get_mut(&step) {
            Some(s) => {
                *s = next;
                true
            }
            None => false,
        }
    }

    /// Every step that is currently eligible to **start running**: it is
    /// `Pending` and all of its dependencies are `Finished` (merged). A step
    /// with no dependencies is eligible immediately. Returns ids in the
    /// plan's authoring order for determinism.
    pub fn eligible(&self) -> Vec<Uuid> {
        self.steps
            .iter()
            .copied()
            .filter(|s| self.is_eligible(*s))
            .collect()
    }

    /// Whether `step` is eligible to start right now.
    pub fn is_eligible(&self, step: Uuid) -> bool {
        if self.state.get(&step) != Some(&StepState::Pending) {
            return false;
        }
        self.deps
            .get(&step)
            .map(|ds| {
                ds.iter()
                    .all(|d| self.state.get(d).map(|s| s.is_finished()).unwrap_or(false))
            })
            .unwrap_or(true)
    }

    /// True once **every** step has reached a terminal state (`Merged` or
    /// `Failed`) — the plan run has nothing left to do.
    pub fn all_terminal(&self) -> bool {
        self.steps.iter().all(|s| {
            self.state
                .get(s)
                .map(|st| st.is_terminal())
                .unwrap_or(false)
        })
    }

    /// True when **every** step is `Merged` — the success terminal for the
    /// whole plan (the precondition the final branch_stable gate guards).
    pub fn all_merged(&self) -> bool {
        self.steps
            .iter()
            .all(|s| self.state.get(s) == Some(&StepState::Merged))
    }

    /// A **quiescence point** (plan.md §4.1 / prompt 4 branch_stable
    /// semantics): no step is actively occupying a worker or the merge queue.
    /// This is true when all runnable work has momentarily landed — including
    /// when the only steps left are blocked behind a paused/human-waiting
    /// step, and finally when every step is terminal. The pooled
    /// `branch_stable` suite runs at each quiescence point (debounced on tip
    /// advance by the caller).
    pub fn is_quiescent(&self) -> bool {
        !self
            .steps
            .iter()
            .any(|s| self.state.get(s).map(|st| st.is_active()).unwrap_or(false))
    }

    /// Steps that can never run because they sit downstream of a `Failed`
    /// step (directly or transitively). Reported so the plan's status can
    /// distinguish "blocked by a broken dependency" from "still pending".
    pub fn blocked_by_failure(&self) -> HashSet<Uuid> {
        let mut blocked = HashSet::new();
        // Seed with directly-failed steps' dependents, then propagate.
        let mut changed = true;
        while changed {
            changed = false;
            for step in &self.steps {
                if blocked.contains(step) {
                    continue;
                }
                if self.state.get(step) == Some(&StepState::Failed) {
                    continue; // a failed step is failed, not "blocked".
                }
                let depends_on_bad = self
                    .deps
                    .get(step)
                    .map(|ds| {
                        ds.iter().any(|d| {
                            self.state.get(d) == Some(&StepState::Failed) || blocked.contains(d)
                        })
                    })
                    .unwrap_or(false);
                if depends_on_bad {
                    blocked.insert(*step);
                    changed = true;
                }
            }
        }
        blocked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<Uuid> {
        (0..n).map(|_| Uuid::new_v4()).collect()
    }

    #[test]
    fn independent_steps_are_both_eligible() {
        let s = ids(2);
        let sched = Scheduler::new(&s, &[]);
        let elig = sched.eligible();
        assert_eq!(elig.len(), 2, "two independent steps both eligible");
        assert!(elig.contains(&s[0]) && elig.contains(&s[1]));
    }

    #[test]
    fn dependent_step_blocks_until_dep_merged() {
        let s = ids(2);
        // s[1] depends on s[0].
        let mut sched = Scheduler::new(&s, &[(s[1], s[0])]);
        // Only s[0] is eligible at first.
        assert_eq!(sched.eligible(), vec![s[0]]);
        // s[0] running/testing/queued/merging does NOT unblock s[1].
        for state in [
            StepState::Running,
            StepState::Testing,
            StepState::Queued,
            StepState::Merging,
        ] {
            sched.set_state(s[0], state);
            assert_eq!(
                sched.eligible(),
                Vec::<Uuid>::new(),
                "{state:?} must not unblock dependent"
            );
        }
        // Only when s[0] is Merged does s[1] become eligible.
        sched.set_state(s[0], StepState::Merged);
        assert_eq!(sched.eligible(), vec![s[1]]);
    }

    #[test]
    fn step_eligible_iff_all_deps_finished() {
        // s[2] depends on BOTH s[0] and s[1].
        let s = ids(3);
        let mut sched = Scheduler::new(&s, &[(s[2], s[0]), (s[2], s[1])]);
        sched.set_state(s[0], StepState::Merged);
        // One dep merged, the other not → s[2] still blocked.
        assert!(!sched.is_eligible(s[2]));
        sched.set_state(s[1], StepState::Merged);
        // Both merged → s[2] eligible.
        assert!(sched.is_eligible(s[2]));
    }

    #[test]
    fn failed_step_blocks_dependents_forever() {
        let s = ids(3);
        // chain: s0 <- s1 <- s2
        let mut sched = Scheduler::new(&s, &[(s[1], s[0]), (s[2], s[1])]);
        sched.set_state(s[0], StepState::Failed);
        assert!(
            !sched.is_eligible(s[1]),
            "dependent of failed step never eligible"
        );
        let blocked = sched.blocked_by_failure();
        assert!(blocked.contains(&s[1]), "s1 blocked by failed s0");
        assert!(blocked.contains(&s[2]), "s2 transitively blocked");
        assert!(
            !blocked.contains(&s[0]),
            "the failed step itself is failed, not blocked"
        );
    }

    #[test]
    fn quiescence_requires_no_active_step() {
        let s = ids(2);
        let mut sched = Scheduler::new(&s, &[]);
        // All pending → quiescent (nothing in-flight yet).
        assert!(sched.is_quiescent());
        sched.set_state(s[0], StepState::Running);
        assert!(!sched.is_quiescent(), "a running step breaks quiescence");
        // A step awaiting a human is NOT active — quiescence can be reached
        // with the rest of the work settled and one step paused.
        sched.set_state(s[0], StepState::AwaitingHuman);
        assert!(sched.is_quiescent());
        sched.set_state(s[0], StepState::Merged);
        assert!(sched.is_quiescent());
    }

    #[test]
    fn all_merged_and_all_terminal() {
        let s = ids(2);
        let mut sched = Scheduler::new(&s, &[]);
        assert!(!sched.all_terminal());
        sched.set_state(s[0], StepState::Merged);
        sched.set_state(s[1], StepState::Failed);
        assert!(sched.all_terminal(), "merged + failed are both terminal");
        assert!(!sched.all_merged(), "a failed step means not all merged");
    }
}
