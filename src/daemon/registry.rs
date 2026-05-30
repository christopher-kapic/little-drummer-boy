//! Session registry — owns the live [`SessionWorkerHandle`]s.
//!
//! One [`SessionRegistry`] per daemon process. Maps `session_id →
//! handle`; spawns a worker lazily on first `attach`, returns the
//! existing handle on subsequent attaches to the same id.
//!
//! Attach modes:
//!
//! - `attach(None, Some(project_root))` — create a fresh session in
//!   `project_root`.
//! - `attach(Some(id), _)` — resume the session with that id. Errors
//!   if no DB row exists.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::extended::ExtendedConfig;
use crate::config::providers::ProvidersConfig;
use crate::daemon::session_worker::{self, SessionWorkerHandle};
use crate::daemon::shutdown::ShutdownSignal;
use crate::db::Db;
use crate::engine::model::Model;
use crate::locks::LockManager;
use crate::redact::RedactionTable;
use crate::session::Session;

/// Daemon-wide registry of active session workers.
#[derive(Clone)]
pub struct SessionRegistry {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    locks: Arc<LockManager>,
    workers: Mutex<HashMap<Uuid, SessionWorkerHandle>>,
    /// Live `JoinHandle` per worker, so a graceful drain can *await* the
    /// in-flight turn finishing (and `abort()` it past the deadline).
    /// Keyed by the same `session_id` as `workers`; populated on spawn,
    /// removed by [`Self::forget`] when the worker exits.
    worker_joins: Mutex<HashMap<Uuid, JoinHandle<()>>>,
    /// Daemon-wide graceful-shutdown gate
    /// (`daemon-graceful-drain-shutdown.md`). Installed into every worker's
    /// model so the inference-dispatch chokepoint refuses new provider
    /// requests once a drain begins. The drain state lives here, on the
    /// daemon's central authority — never scattered per call.
    shutdown: ShutdownSignal,
}

impl SessionRegistry {
    pub fn new(db: Db, locks: Arc<LockManager>, shutdown: ShutdownSignal) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                locks,
                workers: Mutex::new(HashMap::new()),
                worker_joins: Mutex::new(HashMap::new()),
                shutdown,
            }),
        }
    }

    /// Spawn (or look up) the worker for a session. The caller
    /// supplies the resolved provider + extended configs so the
    /// registry can build the model and redaction table without
    /// re-walking the layered config every attach. (Wiring the
    /// resolver inside the daemon lands with the daemon-side `/config`
    /// payload.)
    #[allow(clippy::too_many_arguments)]
    pub fn attach(
        &self,
        session_id: Option<Uuid>,
        project_root: Option<PathBuf>,
        providers_cfg: &ProvidersConfig,
        extended_cfg: &ExtendedConfig,
        client_no_sandbox: bool,
        model_override: Option<&str>,
        plan_context: Option<(Uuid, Uuid)>,
    ) -> Result<SessionWorkerHandle> {
        // Resume path.
        if let Some(id) = session_id {
            if let Some(handle) = self.lookup(id) {
                return Ok(handle);
            }
            let session = Session::resume(self.inner.db.clone(), id)
                .context("resuming session")?
                .ok_or_else(|| anyhow::anyhow!("unknown session {id}"))?;
            // Resume keeps the running worker's model; an override only seeds
            // a newly-created session (matched by the server's gating). Plan
            // attribution is likewise create-only.
            return self.start_worker(
                session,
                providers_cfg,
                extended_cfg,
                client_no_sandbox,
                None,
                None,
            );
        }

        // Create path.
        let Some(project_root) = project_root else {
            bail!("attach requires either session_id or project_root");
        };
        // Lazy persistence (session-id-display-and-lazy-persist): hold the
        // new session in memory with its id assigned but its `sessions` row
        // un-written. The worker persists it on the first user message.
        let session = Session::create_deferred(
            self.inner.db.clone(),
            project_root,
            session_worker::initial_active_agent(),
        )
        .context("creating session")?;
        if let Some(active) = &providers_cfg.active_model {
            session
                .set_active_model(&active.provider, &active.model)
                .context("setting active model on new session")?;
        }
        self.start_worker(
            session,
            providers_cfg,
            extended_cfg,
            client_no_sandbox,
            model_override,
            plan_context,
        )
    }

    fn lookup(&self, session_id: Uuid) -> Option<SessionWorkerHandle> {
        self.inner.workers.lock().unwrap().get(&session_id).cloned()
    }

    fn start_worker(
        &self,
        session: Session,
        providers_cfg: &ProvidersConfig,
        extended_cfg: &ExtendedConfig,
        client_no_sandbox: bool,
        model_override: Option<&str>,
        plan_context: Option<(Uuid, Uuid)>,
    ) -> Result<SessionWorkerHandle> {
        let session_id = session.id;
        let project_root = session.project_root.clone();

        // Plan-run metric attribution (`plan-run-metrics`): stamp the session
        // with its plan/step so every inference call rolls up per plan. Set
        // before the session is shared so the first call already carries it.
        if let Some((plan_id, step_id)) = plan_context {
            session.set_plan_context(plan_id.to_string(), step_id.to_string());
        }

        // Build per-session redaction table from the session's
        // project_root + the daemon's env.
        let redact = RedactionTable::build(&extended_cfg.redact, &project_root)
            .context("building redaction table")?;
        let redact = Arc::new(redact);

        // Build the model from providers config. Errors out loud if
        // no provider is configured for the session's active model. Install
        // the daemon's shared shutdown gate so this worker's inference
        // dispatch refuses new provider requests once a drain begins
        // (`daemon-graceful-drain-shutdown.md`).
        let model = Arc::new(
            Model::from_config(providers_cfg)
                .context("resolving model")?
                .with_shutdown_gate(self.inner.shutdown.clone()),
        );

        // Plan-level model override (`cockpit run --model`): a well-formed
        // `provider/model` selector built through the same provider pipeline as
        // the session model, with the same shutdown gate. A malformed selector
        // or unconfigured provider degrades to no override rather than failing
        // the attach — the executor already validated `--model` up front.
        let model_override = model_override
            .and_then(crate::config::provider::split_provider_model)
            .and_then(|(provider, model_id)| {
                Model::for_provider(providers_cfg, &provider, &model_id).ok()
            })
            .map(|m| Arc::new(m.with_shutdown_gate(self.inner.shutdown.clone())));

        let session = Arc::new(session);
        let (handle, join) = session_worker::spawn(
            session,
            self.inner.locks.clone(),
            redact,
            model,
            model_override,
            project_root,
            client_no_sandbox,
        );

        self.inner
            .workers
            .lock()
            .unwrap()
            .insert(session_id, handle.clone());
        self.inner
            .worker_joins
            .lock()
            .unwrap()
            .insert(session_id, join);

        Ok(handle)
    }

    /// Drop a session's worker handle from the registry. Called when
    /// the worker exits (session ended, daemon shutdown).
    pub fn forget(&self, session_id: Uuid) {
        self.inner.workers.lock().unwrap().remove(&session_id);
        self.inner.worker_joins.lock().unwrap().remove(&session_id);
    }

    /// Graceful drain (`daemon-graceful-drain-shutdown.md`). Sends
    /// `Shutdown` to every running worker — which closes its driver input
    /// so the in-flight turn finishes — then **awaits** all worker tasks up
    /// to `grace`. Any worker still running when the deadline fires (a hung
    /// provider call, a wedged tool) is `abort()`ed so the daemon can exit
    /// regardless. The new-request gate must already be set
    /// (`shutdown.begin_drain()`) before calling this, so no fresh provider
    /// dispatch slips out while we drain.
    ///
    /// Returns `true` when every worker drained cleanly within `grace`, and
    /// `false` when the deadline forced an abort — the caller surfaces the
    /// "shutdown was forced" note from that.
    pub async fn drain_all(&self, grace: Duration) -> bool {
        // Snapshot + take the join handles. Taking them out of the map means
        // a worker that exits on its own mid-drain (and calls `forget`)
        // can't race us for its handle.
        let handles: Vec<SessionWorkerHandle> = {
            let workers = self.inner.workers.lock().unwrap();
            workers.values().cloned().collect()
        };
        let joins: Vec<(Uuid, JoinHandle<()>)> = {
            let mut joins = self.inner.worker_joins.lock().unwrap();
            joins.drain().collect()
        };

        // Ask each worker to stop: closes its driver input so the current
        // turn (if any) drains, then the worker task ends.
        for h in &handles {
            let _ = h
                .send_work(crate::daemon::session_worker::SessionWork::Shutdown)
                .await;
        }

        // Await all worker tasks concurrently, racing the shared grace
        // deadline. We wait for ALL to finish (or the deadline), never just
        // the first — `join_all` resolves only when every future has. The
        // `abort_handle`s let the deadline arm force-abort whatever's left.
        let abort_handles: Vec<tokio::task::AbortHandle> =
            joins.iter().map(|(_, j)| j.abort_handle()).collect();
        let drain = futures::future::join_all(joins.into_iter().map(|(_, j)| j));

        match tokio::time::timeout(grace, drain).await {
            Ok(_) => true,
            Err(_) => {
                // Grace exhausted with work still outstanding: force-abort
                // every (possibly already-finished — abort is then a no-op)
                // worker task so the daemon can exit. Aborting drops the
                // worker's driver, which cancels its streaming inference and
                // kills any running `bash` subprocess.
                tracing::warn!("daemon drain grace exhausted; forcing worker abort");
                for ah in &abort_handles {
                    ah.abort();
                }
                false
            }
        }
    }

    /// Snapshot of currently-active session ids. Useful for `cockpit
    /// daemon status` and the `list_sessions` request.
    pub fn active_session_ids(&self) -> Vec<Uuid> {
        self.inner.workers.lock().unwrap().keys().copied().collect()
    }

    /// Whether *any* live session worker is currently doing agent work —
    /// either mid-turn (`processing`) or holding an async job
    /// (loop/timer/background). Drives `/caffeinate until-idle` auto-off:
    /// the daemon owns the session workers / `JobAuthority`, so it is the
    /// authority for "is an agent running anywhere?". Lock-free reads of
    /// each worker's shared atomics.
    pub fn any_agent_running(&self) -> bool {
        self.inner.workers.lock().unwrap().values().any(|h| {
            let (has_jobs, processing) = h.live_status();
            has_jobs || processing
        })
    }

    /// Live `(has_active_jobs, processing)` status for a session, or
    /// `None` when no worker is live for it (the browser then treats it
    /// as not-processing / no-jobs). Lock-free read of the worker's
    /// shared atomics (GOALS §17f).
    pub fn live_status(&self, session_id: Uuid) -> Option<(bool, bool)> {
        self.inner
            .workers
            .lock()
            .unwrap()
            .get(&session_id)
            .map(|h| h.live_status())
    }

    /// Interrupt a live session before archive/delete (GOALS §17h): stop
    /// its worker (which closes the driver — cancelling its async jobs as
    /// the driver task drops, and ending the current turn cleanly) and
    /// forget the handle. No-op when no worker is live. Returns `true`
    /// when a live worker was stopped. Awaits the `Shutdown` send so the
    /// worker has begun teardown before the caller applies the DB op.
    pub async fn interrupt_and_stop(&self, session_id: Uuid) -> bool {
        let handle = self.lookup(session_id);
        let Some(handle) = handle else {
            return false;
        };
        let _ = handle
            .send_work(crate::daemon::session_worker::SessionWork::Shutdown)
            .await;
        self.forget(session_id);
        true
    }

    /// Test-only: register a raw worker `JoinHandle` directly, bypassing the
    /// full `Session`/`Driver`/`Model` wiring. Lets the drain tests
    /// (`daemon-graceful-drain-shutdown.md`) inject tasks with controlled
    /// in-flight duration so they can assert the await / grace / force
    /// behavior without standing up a real provider call. No
    /// `SessionWorkerHandle` is inserted, so `drain_all` sends `Shutdown` to
    /// zero handles and exercises the join/timeout/abort path in isolation.
    #[cfg(test)]
    fn insert_test_join(&self, id: Uuid, join: JoinHandle<()>) {
        self.inner.worker_joins.lock().unwrap().insert(id, join);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn test_registry() -> SessionRegistry {
        // The DB + lock manager aren't touched by `drain_all`; point them at
        // a throwaway in-memory DB so construction never hits user state.
        let db = Db::open_in_memory().expect("in-memory db");
        let locks = Arc::new(LockManager::from_db(db.clone()).expect("locks"));
        SessionRegistry::new(db, locks, ShutdownSignal::new())
    }

    /// drain-awaits-in-flight: a worker still finishing its turn must be
    /// awaited to completion (within grace), not abandoned. The join runs to
    /// its natural end and `drain_all` reports a clean drain.
    #[tokio::test]
    async fn drain_awaits_in_flight_work() {
        let reg = test_registry();
        let finished = Arc::new(AtomicBool::new(false));

        let finished_c = finished.clone();
        let join = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            finished_c.store(true, Ordering::SeqCst);
        });
        reg.insert_test_join(Uuid::new_v4(), join);

        // Generous grace: the in-flight work finishes well inside it.
        let clean = reg.drain_all(Duration::from_secs(5)).await;
        assert!(
            clean,
            "drain should report clean when work finishes in grace"
        );
        assert!(
            finished.load(Ordering::SeqCst),
            "in-flight work must run to completion, not be abandoned"
        );
    }

    /// force-at-deadline: a hung worker (never finishes) is force-aborted at
    /// the grace deadline and `drain_all` reports a forced (non-clean)
    /// drain, so a truncated turn isn't mistaken for a clean finish.
    #[tokio::test]
    async fn force_aborts_hung_worker_at_deadline() {
        let reg = test_registry();
        let aborted = Arc::new(AtomicBool::new(false));

        // A task that "hangs" forever, with a drop guard that records the
        // abort (dropping the task future runs the guard's `Drop`).
        struct AbortFlag(Arc<AtomicBool>);
        impl Drop for AbortFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flag = AbortFlag(aborted.clone());
        let join = tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        reg.insert_test_join(Uuid::new_v4(), join);

        let start = std::time::Instant::now();
        let clean = reg.drain_all(Duration::from_millis(120)).await;
        assert!(
            !clean,
            "a hung worker must yield a forced (non-clean) drain"
        );
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "drain should wait out the grace before forcing"
        );
        // The abort dropped the task future, running its guard.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            aborted.load(Ordering::SeqCst),
            "the hung worker must be force-aborted at the deadline"
        );
    }

    /// idle-fast-path: with no live workers, `drain_all` returns promptly and
    /// cleanly — it never sleeps the grace.
    #[tokio::test]
    async fn idle_drain_is_fast_and_clean() {
        let reg = test_registry();
        let start = std::time::Instant::now();
        let clean = reg.drain_all(Duration::from_secs(30)).await;
        assert!(clean, "idle drain is clean");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "idle drain must not wait out the grace"
        );
    }
}
