//! The single async-job authority + registry (GOALS §22).
//!
//! The authority lives on the driver. It owns the registry of live jobs
//! and the per-job spawned tasks. Two channels connect it to the rest of
//! the engine:
//!
//! - **commands** ([`JobCommand`]): driver → authority. A `jobs` tool call
//!   in the main context turns into a command. The driver calls
//!   [`JobAuthority::handle_command`] inline (it's cheap; no `.await` on
//!   network).
//! - **events** ([`JobEvent`]): authority → driver. Drained by the driver
//!   at the **same turn boundary** as the user-input queue. Carries the
//!   things that must enter *main context*: a keep-in-context loop
//!   iteration's prompt, and any job's terminal result.
//!
//! UI-only signals (job started, per-iteration progress, fork notes, job
//! failed) are emitted by the authority straight onto the engine
//! [`TurnEvent`] channel — they reach the TUI but never the model's main
//! context until termination (token economy, §22 "UI visibility and
//! context injection are deliberately separated").
//!
//! ## Loop execution split
//!
//! - `keep_in_context = true`: the authority schedules a ticking timer
//!   that sends [`JobEvent::LoopIterationDue`] to the driver; the driver
//!   runs the prompt as a real turn in **main history**, then tells the
//!   authority the iteration finished ([`JobCommand::IterationFinished`])
//!   so it can schedule the next tick or terminate.
//! - `keep_in_context = false`: the whole loop runs inside the spawned
//!   task on an **ephemeral fork** ([`super::loop_runner`]); only `note`s
//!   (live UI) and the terminal result (via [`JobEvent::Completed`]) cross
//!   to main.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::engine::agent::{Agent, TurnEvent};
use crate::engine::jobs::loop_runner::{self, LoopRunCtx};
use crate::engine::jobs::spec::{BackgroundStartArgs, JobKind, LoopStartArgs, SpawnRequest};
use crate::redact::RedactionTable;
use crate::session::Session;

use super::background;

/// A command from the driver to the authority over the async command
/// channel. The driver mutates the authority directly for everything it
/// originates (start / iteration-finished); this channel exists for
/// commands that arrive from **outside** the turn loop — today, a
/// human-initiated cancel routed through the session worker (GOALS §22).
#[derive(Debug)]
pub enum JobCommand {
    /// Cancel a job (loop / timer / background) by id. From the human
    /// ("stop checking the deploy", `/jobs cancel <id>`).
    Cancel { job_id: String },
}

/// An event from the authority to the driver, drained at the turn
/// boundary. These are the only signals that affect **main context**.
#[derive(Debug)]
pub enum JobEvent {
    /// A keep-in-context loop iteration is due: run `prompt` as a turn in
    /// main history. After the turn the driver posts
    /// [`JobCommand::IterationFinished`].
    LoopIterationDue { job_id: String, prompt: String },
    /// A job reached a terminal state and its result must be injected into
    /// main context as a late-arriving turn. `notes` are the fork's
    /// accumulated notes (ephemeral loops); empty otherwise.
    Completed {
        job_id: String,
        label: String,
        kind: JobKind,
        /// Budget-capped result text.
        result: String,
        /// `true` when the job failed (non-zero exit, error). Drives the
        /// `needs_attention` flag wording.
        failed: bool,
        /// Create-action requests a fork emitted (anti-runaway): main
        /// decides whether to honour them. Empty for non-fork jobs.
        requests: Vec<SpawnRequest>,
    },
}

/// One row in the live-jobs registry. Cloned cheaply into the
/// [`JobSnapshot`] the TUI strip / `/jobs` read.
struct JobEntry {
    job_id: String,
    label: String,
    kind: JobKind,
    /// `Some(n)` = iteration cap; `None` = unlimited.
    limit: Option<u64>,
    /// Completed iterations so far (loops only).
    iteration: u64,
    /// Abort handle for the spawned task (background, ephemeral loop) or
    /// `None` for an in-context loop (driven by the driver, no task).
    abort: Option<AbortHandle>,
    /// For in-context loops: the scheduler state needed to re-arm.
    in_context: Option<InContextLoop>,
    /// Handle the authority uses to talk to a background job (tail / kill).
    background: Option<Arc<background::BackgroundHandle>>,
}

/// Per-iteration scheduling state for a keep-in-context loop. The
/// authority arms a timer task that fires [`JobEvent::LoopIterationDue`].
struct InContextLoop {
    args: LoopStartArgs,
    /// The next tick's delay, doubled each iteration when `backoff`.
    next_delay_secs: u64,
    /// Abort handle for the currently-armed tick timer (if any).
    timer_abort: Option<AbortHandle>,
}

/// A read-only snapshot of one live job, for the TUI strip and `/jobs`.
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub job_id: String,
    pub label: String,
    pub kind: JobKind,
    pub limit: Option<u64>,
    pub iteration: u64,
}

/// Shared context the authority threads into spawned job tasks.
#[derive(Clone)]
pub struct JobContext {
    pub session: Arc<Session>,
    pub locks: Arc<crate::locks::LockManager>,
    pub redact: Arc<RedactionTable>,
    pub cwd: std::path::PathBuf,
    /// The main agent — ephemeral-fork loop iterations run on the same
    /// agent/model/provider config (GOALS §22).
    pub agent: Arc<Agent>,
}

/// The single async-job authority. Owned by the driver; never cloned.
pub struct JobAuthority {
    registry: BTreeMap<String, JobEntry>,
    /// Cap on concurrently-running jobs.
    pub max_concurrent: usize,
    /// Sender the authority hands to spawned tasks + timers so they post
    /// [`JobEvent`]s back to the driver.
    event_tx: mpsc::Sender<JobEvent>,
    /// Self-command channel: spawned timers post `IterationFinished`-style
    /// re-arm requests here. Actually the driver owns command delivery;
    /// the authority also holds a clone so in-task timers can re-arm.
    cmd_tx: mpsc::Sender<JobCommand>,
    /// Engine event channel for UI-only signals (started / progress /
    /// note / failed). Cloned into spawned tasks.
    turn_tx: mpsc::Sender<TurnEvent>,
    /// Shared per-session context for spawning ephemeral-fork loops +
    /// background jobs.
    ctx: JobContext,
}

impl JobAuthority {
    /// Build an authority. `event_tx` is drained by the driver at the turn
    /// boundary; `cmd_tx` lets in-task timers re-arm; `turn_tx` is the
    /// engine event channel for UI-only signals.
    pub fn new(
        event_tx: mpsc::Sender<JobEvent>,
        cmd_tx: mpsc::Sender<JobCommand>,
        turn_tx: mpsc::Sender<TurnEvent>,
        ctx: JobContext,
        max_concurrent: usize,
    ) -> Self {
        Self {
            registry: BTreeMap::new(),
            max_concurrent: max_concurrent.max(1),
            event_tx,
            cmd_tx,
            turn_tx,
            ctx,
        }
    }

    /// `true` when at least one loop is live — gates `loop.cancel` enabling.
    pub fn has_loop(&self) -> bool {
        self.registry
            .values()
            .any(|e| matches!(e.kind, JobKind::Loop | JobKind::Timer))
    }

    /// `true` when at least one background job exists — gates
    /// `background.tail` / `background.cancel` enabling.
    pub fn has_background(&self) -> bool {
        self.registry
            .values()
            .any(|e| matches!(e.kind, JobKind::Background))
    }

    /// Snapshot for the TUI strip / `/jobs`, sorted by job id.
    pub fn snapshot(&self) -> Vec<JobSnapshot> {
        self.registry
            .values()
            .map(|e| JobSnapshot {
                job_id: e.job_id.clone(),
                label: e.label.clone(),
                kind: e.kind,
                limit: e.limit,
                iteration: e.iteration,
            })
            .collect()
    }

    /// Look up a background handle for `tail`.
    pub fn background_handle(&self, job_id: &str) -> Option<Arc<background::BackgroundHandle>> {
        self.registry.get(job_id).and_then(|e| e.background.clone())
    }

    /// `true` when the concurrency cap would be exceeded by one more job.
    pub fn at_capacity(&self) -> bool {
        self.registry.len() >= self.max_concurrent
    }

    /// Start a loop/timer that accumulates in the main context. Returns
    /// the registered job id (echoed back to the model so it can cancel).
    pub fn start_loop_in_context(&mut self, args: LoopStartArgs) -> String {
        let job_id = new_job_id();
        let kind = args.kind();
        let label = loop_label(&args);
        let entry = JobEntry {
            job_id: job_id.clone(),
            label: label.clone(),
            kind,
            limit: args.limit,
            iteration: 0,
            abort: None,
            in_context: Some(InContextLoop {
                next_delay_secs: args.interval_secs,
                args,
                timer_abort: None,
            }),
            background: None,
        };
        self.registry.insert(job_id.clone(), entry);
        self.emit_started(&job_id, &label, kind);
        // Arm the first tick.
        self.arm_in_context_tick(&job_id);
        job_id
    }

    /// Start an ephemeral-fork loop (`keep_in_context = false`). The whole
    /// loop runs inside the spawned task; only notes (live UI) + the
    /// terminal result cross to main.
    pub fn start_loop_forked(&mut self, args: LoopStartArgs) -> String {
        let job_id = new_job_id();
        let kind = args.kind();
        let label = loop_label(&args);
        self.emit_started(&job_id, &label, kind);

        let run_ctx = LoopRunCtx {
            job_id: job_id.clone(),
            label: label.clone(),
            args: args.clone(),
            ctx: self.ctx.clone(),
            turn_tx: self.turn_tx.clone(),
            event_tx: self.event_tx.clone(),
        };
        let handle = tokio::spawn(loop_runner::run_forked_loop(run_ctx));
        let entry = JobEntry {
            job_id: job_id.clone(),
            label,
            kind,
            limit: args.limit,
            iteration: 0,
            abort: Some(handle.abort_handle()),
            in_context: None,
            background: None,
        };
        self.registry.insert(job_id.clone(), entry);
        job_id
    }

    /// Start a background shell job. Returns the job id.
    pub fn start_background(&mut self, args: BackgroundStartArgs) -> String {
        let job_id = new_job_id();
        let label = background_label(&args);
        self.emit_started(&job_id, &label, JobKind::Background);

        let cwd = args
            .cwd
            .as_deref()
            .map(|s| crate::tools::common::resolve(s, &self.ctx.cwd))
            .unwrap_or_else(|| self.ctx.cwd.clone());

        let (handle, task) = background::spawn(
            job_id.clone(),
            label.clone(),
            args.command.clone(),
            cwd,
            self.ctx.redact.clone(),
            self.turn_tx.clone(),
            self.event_tx.clone(),
        );
        let abort = task.abort_handle();
        let entry = JobEntry {
            job_id: job_id.clone(),
            label,
            kind: JobKind::Background,
            limit: None,
            iteration: 0,
            abort: Some(abort),
            in_context: None,
            background: Some(Arc::new(handle)),
        };
        self.registry.insert(job_id.clone(), entry);
        job_id
    }

    /// Cancel a job by id. Returns `true` if it existed. For an
    /// in-context loop, this also promotes its current state as the
    /// terminal result (the model called `loop.cancel`, so the loop is
    /// done — the spec promotes the terminal iteration's result; here the
    /// in-context iterations already accumulated in main, so we just
    /// drop the schedule and emit a terminal marker for the strip).
    pub fn cancel(&mut self, job_id: &str) -> bool {
        let Some(mut entry) = self.registry.remove(job_id) else {
            return false;
        };
        // Stop any armed tick timer + spawned task.
        if let Some(ic) = &mut entry.in_context
            && let Some(t) = ic.timer_abort.take()
        {
            t.abort();
        }
        if let Some(a) = entry.abort.take() {
            a.abort();
        }
        if let Some(bg) = &entry.background {
            bg.kill();
        }
        // In-context loops: the iterations already reached main; emit a
        // terminal completion so the strip clears and a marker shows.
        if entry.in_context.is_some() {
            let _ = self.event_tx.try_send(JobEvent::Completed {
                job_id: entry.job_id.clone(),
                label: entry.label.clone(),
                kind: entry.kind,
                result: format!(
                    "{} cancelled after {} iteration(s)",
                    entry.kind.as_str(),
                    entry.iteration
                ),
                failed: false,
                requests: Vec::new(),
            });
        }
        // Ephemeral loops + background: the spawned task is aborted; we
        // synthesize the terminal completion here since the task won't get
        // to send its own.
        else {
            let _ = self.event_tx.try_send(JobEvent::Completed {
                job_id: entry.job_id.clone(),
                label: entry.label.clone(),
                kind: entry.kind,
                result: format!("{} `{}` cancelled", entry.kind.as_str(), entry.label),
                failed: false,
                requests: Vec::new(),
            });
        }
        true
    }

    /// A keep-in-context iteration finished. Advance the count; arm the
    /// next tick or terminate (limit reached).
    pub fn iteration_finished(&mut self, job_id: &str) {
        let terminal = {
            let Some(entry) = self.registry.get_mut(job_id) else {
                return;
            };
            entry.iteration = entry.iteration.saturating_add(1);
            let Some(ic) = &mut entry.in_context else {
                return;
            };
            // Backoff: double the next delay up to the ceiling.
            if ic.args.backoff {
                ic.next_delay_secs =
                    (ic.next_delay_secs.saturating_mul(2)).min(super::spec::BACKOFF_CEILING_SECS);
            }
            matches!(entry.limit, Some(limit) if entry.iteration >= limit)
        };
        if terminal {
            // Limit reached: emit terminal completion, drop the entry.
            if let Some(entry) = self.registry.remove(job_id) {
                let _ = self.event_tx.try_send(JobEvent::Completed {
                    job_id: entry.job_id.clone(),
                    label: entry.label.clone(),
                    kind: entry.kind,
                    result: format!(
                        "{} `{}` completed after {} iteration(s)",
                        entry.kind.as_str(),
                        entry.label,
                        entry.iteration
                    ),
                    failed: false,
                    requests: Vec::new(),
                });
            }
        } else {
            self.arm_in_context_tick(job_id);
        }
    }

    /// Handle a [`JobCommand`] that arrived over the async command channel
    /// (a human-initiated cancel). Everything the driver originates it
    /// calls directly via the dedicated `start_*` / `iteration_finished`
    /// methods.
    pub fn handle_command(&mut self, cmd: JobCommand) {
        match cmd {
            JobCommand::Cancel { job_id } => {
                self.cancel(&job_id);
            }
        }
    }

    /// Arm a timer task that, after the next delay, posts
    /// [`JobEvent::LoopIterationDue`] for `job_id`.
    fn arm_in_context_tick(&mut self, job_id: &str) {
        let (delay, prompt) = {
            let Some(entry) = self.registry.get(job_id) else {
                return;
            };
            let Some(ic) = &entry.in_context else {
                return;
            };
            (ic.next_delay_secs, ic.args.prompt.clone())
        };
        let event_tx = self.event_tx.clone();
        let jid = job_id.to_string();
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            let _ = event_tx
                .send(JobEvent::LoopIterationDue {
                    job_id: jid,
                    prompt,
                })
                .await;
        });
        if let Some(entry) = self.registry.get_mut(job_id)
            && let Some(ic) = &mut entry.in_context
            && let Some(old) = ic.timer_abort.replace(task.abort_handle())
        {
            old.abort();
        }
    }

    /// Emit the UI-only `started` signal.
    fn emit_started(&self, job_id: &str, label: &str, kind: JobKind) {
        let _ = self.turn_tx.try_send(TurnEvent::JobStarted {
            session_id: self.ctx.session.id,
            job_id: job_id.to_string(),
            label: label.to_string(),
            kind: kind.as_str().to_string(),
        });
    }

    /// Re-derive the command sender so the session worker can post
    /// driver-side commands (used by tests + the driver wiring).
    pub fn command_sender(&self) -> mpsc::Sender<JobCommand> {
        self.cmd_tx.clone()
    }

    /// Rebind the engine [`TurnEvent`] channel used for UI-only signals.
    /// The driver builds the authority before it has the per-turn event
    /// sender (`tx`), then rebinds it once `run_main_loop` starts — before
    /// any job can be created, so no UI signal is ever lost.
    pub fn set_turn_tx(&mut self, tx: mpsc::Sender<TurnEvent>) {
        self.turn_tx = tx;
    }

    /// Rebind the fork context's agent after a primary swap (`/plan` ↔
    /// `/build`, `plan.md §4.6.d`) so future ephemeral-fork loop iterations
    /// run on the new primary's model/tool surface. Existing live jobs keep
    /// the agent they were spawned with.
    pub fn set_agent(&mut self, agent: Arc<Agent>) {
        self.ctx.agent = agent;
    }
}

/// Short random job id (`job-xxxxxxxx`). Human-typable in `/jobs cancel`
/// and short enough for the strip.
fn new_job_id() -> String {
    let u = Uuid::new_v4();
    let short = &u.simple().to_string()[..8];
    format!("job-{short}")
}

/// One-line label for a loop/timer (the command-ish summary shown in the
/// strip and completion marker).
fn loop_label(args: &LoopStartArgs) -> String {
    let first = args.prompt.lines().next().unwrap_or("").trim();
    let snippet: String = first.chars().take(32).collect();
    if first.chars().count() > 32 {
        format!("{snippet}…")
    } else {
        snippet
    }
}

/// One-line label for a background job (first line of the command).
fn background_label(args: &BackgroundStartArgs) -> String {
    let first = args.command.lines().next().unwrap_or("").trim();
    let snippet: String = first.chars().take(40).collect();
    if first.chars().count() > 40 {
        format!("{snippet}…")
    } else {
        snippet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::jobs::spec::parse_loop_start;
    use std::time::Duration;

    /// Build a test authority with a keyless localhost model (never
    /// called by the in-context / background paths under test). Returns
    /// the authority + the job-event receiver + the UI-event receiver.
    fn test_authority(
        max: usize,
    ) -> (
        JobAuthority,
        mpsc::Receiver<JobEvent>,
        mpsc::Receiver<TurnEvent>,
        tempfile::TempDir,
    ) {
        use crate::config::providers::{ActiveModelRef, ProviderEntry, ProvidersConfig};
        use std::collections::BTreeMap;

        // A real on-disk cwd so background jobs (real subprocesses) can
        // spawn. Returned so it outlives the authority.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let db = crate::db::Db::open_in_memory().unwrap();
        let session =
            Arc::new(crate::session::Session::create(db.clone(), root.clone(), "coder").unwrap());
        let locks = Arc::new(crate::locks::LockManager::from_db(db).unwrap());
        let cfg = crate::config::extended::RedactConfig::default();
        let redact = Arc::new(RedactionTable::build(&cfg, &root).unwrap());

        let mut providers = BTreeMap::new();
        providers.insert(
            "lmstudio".to_string(),
            ProviderEntry {
                url: "http://localhost:1/v1".into(),
                headers: vec![],
                ..ProviderEntry::default()
            },
        );
        let pcfg = ProvidersConfig {
            providers,
            active_model: Some(ActiveModelRef {
                provider: "lmstudio".into(),
                model: "local".into(),
                thinking_mode: None,
            }),
            ..ProvidersConfig::default()
        };
        let model = Arc::new(crate::engine::model::Model::from_config(&pcfg).unwrap());
        let agent = Arc::new(crate::engine::agent::Agent {
            name: "coder".into(),
            system: String::new(),
            tools: crate::engine::tool::ToolBox::new(),
            model,
            params: crate::engine::model::ModelParams::default(),
            llm_mode: crate::config::extended::LlmMode::default(),
        });

        let (event_tx, event_rx) = mpsc::channel(64);
        let (cmd_tx, _cmd_rx) = mpsc::channel(64);
        let (turn_tx, turn_rx) = mpsc::channel(64);
        let ctx = JobContext {
            session,
            locks,
            redact,
            cwd: root,
            agent,
        };
        let authority = JobAuthority::new(event_tx, cmd_tx, turn_tx, ctx, max);
        (authority, event_rx, turn_rx, tmp)
    }

    /// An in-context loop fires a `LoopIterationDue` each interval and,
    /// once its limit is reached, emits a terminal `Completed`. The driver
    /// drives the turns; here we simulate that by calling
    /// `iteration_finished` after each due event.
    #[tokio::test(start_paused = true)]
    async fn in_context_loop_ticks_then_terminates_at_limit() {
        let (mut auth, mut events, mut ui, _tmp) = test_authority(8);
        let args = parse_loop_start(&serde_json::json!({
            "interval": 10, "prompt": "poll", "limit": 2
        }))
        .unwrap();
        let job_id = auth.start_loop_in_context(args);
        assert!(auth.has_loop());
        // started UI signal.
        assert!(matches!(ui.try_recv(), Ok(TurnEvent::JobStarted { .. })));

        // Tick 1.
        tokio::time::advance(Duration::from_secs(10)).await;
        match events.recv().await.unwrap() {
            JobEvent::LoopIterationDue { job_id: j, prompt } => {
                assert_eq!(j, job_id);
                assert_eq!(prompt, "poll");
            }
            other => panic!("expected LoopIterationDue, got {other:?}"),
        }
        auth.iteration_finished(&job_id);

        // Tick 2 (the last).
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(matches!(
            events.recv().await.unwrap(),
            JobEvent::LoopIterationDue { .. }
        ));
        auth.iteration_finished(&job_id);

        // Limit reached → terminal Completed, registry emptied.
        match events.recv().await.unwrap() {
            JobEvent::Completed { kind, failed, .. } => {
                assert_eq!(kind, JobKind::Loop);
                assert!(!failed);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(!auth.has_loop());
    }

    /// A timer (`limit = 1`) fires exactly one iteration then completes.
    #[tokio::test(start_paused = true)]
    async fn timer_fires_once() {
        let (mut auth, mut events, _ui, _tmp) = test_authority(8);
        let args = parse_loop_start(&serde_json::json!({
            "interval": 5, "prompt": "fire", "limit": 1
        }))
        .unwrap();
        assert!(args.is_timer());
        let job_id = auth.start_loop_in_context(args);

        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(matches!(
            events.recv().await.unwrap(),
            JobEvent::LoopIterationDue { .. }
        ));
        auth.iteration_finished(&job_id);
        match events.recv().await.unwrap() {
            JobEvent::Completed { kind, .. } => assert_eq!(kind, JobKind::Timer),
            other => panic!("expected timer Completed, got {other:?}"),
        }
        assert!(!auth.has_loop());
    }

    /// `loop.cancel` ends a live in-context loop early and emits a
    /// terminal Completed.
    #[tokio::test(start_paused = true)]
    async fn cancel_ends_loop_early() {
        let (mut auth, mut events, _ui, _tmp) = test_authority(8);
        let args = parse_loop_start(&serde_json::json!({
            "interval": 60, "prompt": "poll", "limit": 0
        }))
        .unwrap();
        let job_id = auth.start_loop_in_context(args);
        assert!(auth.has_loop());
        assert!(auth.cancel(&job_id));
        match events.recv().await.unwrap() {
            JobEvent::Completed { failed, .. } => assert!(!failed),
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(!auth.has_loop());
        assert!(!auth.cancel(&job_id), "double-cancel is a no-op");
    }

    /// A background `echo` job runs, retains output for `tail`, and injects
    /// a budget-capped completion. Uses real wall-clock time (the child is
    /// a real subprocess) so this test does not pause time.
    #[tokio::test]
    async fn background_runs_tails_and_completes() {
        let (mut auth, mut events, mut ui, _tmp) = test_authority(8);
        let args = crate::engine::jobs::spec::parse_background_start(&serde_json::json!({
            "command": "printf 'hello\\nworld\\n'"
        }))
        .unwrap();
        let job_id = auth.start_background(args);
        assert!(auth.has_background());
        match ui.try_recv() {
            Ok(TurnEvent::JobStarted {
                job_id: j, kind, ..
            }) => {
                assert_eq!(j, job_id);
                assert_eq!(kind, "background");
            }
            other => panic!("expected JobStarted, got {other:?}"),
        }

        // Wait for completion.
        let completed = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("background should complete")
            .unwrap();
        match completed {
            JobEvent::Completed {
                kind,
                failed,
                result,
                ..
            } => {
                assert_eq!(kind, JobKind::Background);
                assert!(!failed, "echo exits 0 — got result: {result}");
                assert!(
                    result.contains("world"),
                    "output should be captured: {result}"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// The concurrency cap is observable via `at_capacity` once the
    /// registry is full.
    #[tokio::test(start_paused = true)]
    async fn capacity_cap_observed() {
        let (mut auth, _events, _ui, _tmp) = test_authority(1);
        assert!(!auth.at_capacity());
        let args =
            parse_loop_start(&serde_json::json!({ "interval": 60, "prompt": "p", "limit": 0 }))
                .unwrap();
        auth.start_loop_in_context(args);
        assert!(auth.at_capacity());
    }
}
