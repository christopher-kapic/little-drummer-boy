//! Async-jobs subsystem — loop / timer / background (GOALS §22).
//!
//! Three async capabilities, one `jobs` meta-tool: recurring self-prompts
//! (`loop`), one-shot delayed prompts (`timer` = a `loop` with `limit=1`),
//! and background shell jobs (`background`). They run without blocking the
//! human; their results inject as a late-arriving turn at the next turn
//! boundary.
//!
//! ## Single authority
//!
//! **Main is the single async-job authority** — same shape as cockpit's
//! single-writer (`coder`) and single-lock-authority (daemon) rules. The
//! [`JobAuthority`] lives on the driver (which the session worker owns).
//! Tool calls in the main context post a [`JobCommand`] to the authority;
//! the authority owns the [`JobRegistry`] and spawns the per-job tasks.
//! Spawned job tasks report progress / completion back over a single
//! [`tokio::sync::mpsc`] channel ([`JobEvent`]) that the driver drains at
//! the **same** turn boundary as the user-input queue (see
//! [`crate::engine::driver`]). This preserves the fold semantics: an async
//! result is just another thing folded in at an inference boundary.
//!
//! ## Anti-runaway invariant
//!
//! Forks **cannot** spawn async work. A loop iteration running in an
//! ephemeral fork that calls `loop.start` / `background.start` does not
//! execute the job; instead the `note`/`jobs` tools in the fork record a
//! [`SpawnRequest`] that rides back to main with the fork's terminal
//! return. Main decides whether to honour it. This prevents
//! recursive/runaway loops.
//!
//! ## Scope (v1)
//!
//! - `background` is shell-only (a loop is already a background job).
//! - A configurable [`max_concurrent`](JobAuthority::max_concurrent) cap
//!   guards against accidental fan-out.
//! - Jobs live for the **daemon/session lifetime**. Surviving a daemon
//!   restart is out of scope for v1 — the registry is in-memory only; a
//!   restart drops live jobs (they are not persisted).

pub mod authority;
mod background;
mod loop_runner;
pub mod spec;

pub use authority::{JobAuthority, JobCommand, JobEvent};
pub use spec::{
    JobAction, JobKind, parse_background_cancel, parse_background_start, parse_background_tail,
    parse_loop_cancel, parse_loop_start,
};

/// Default cap on concurrently-running async jobs per session. Generous
/// enough for "watch the deploy + run the test suite + a reminder timer"
/// but low enough that a confused model can't fan out into dozens of
/// background shells. Configurable via
/// `extended.jobs.max_concurrent` (see [`crate::config::extended`]).
pub const DEFAULT_MAX_CONCURRENT_JOBS: usize = 8;

/// Token cap on a single async result injected into main context (loop
/// terminal result, timer fire, background completion). A `cargo build`
/// can dump huge output; this is the §10 budget for what reaches the
/// model, enforced via [`crate::intel::budget::BudgetedWriter`].
pub const ASYNC_RESULT_TOKEN_CAP: usize = 2_000;

/// Token cap on a `background.tail` response.
pub const TAIL_TOKEN_CAP: usize = 1_000;

/// Lines of rolling stdout/stderr a background job retains for `tail`.
pub const BACKGROUND_RING_LINES: usize = 200;
