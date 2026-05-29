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

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::config::extended::ExtendedConfig;
use crate::config::providers::ProvidersConfig;
use crate::daemon::session_worker::{self, SessionWorkerHandle};
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
}

impl SessionRegistry {
    pub fn new(db: Db, locks: Arc<LockManager>) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                locks,
                workers: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Spawn (or look up) the worker for a session. The caller
    /// supplies the resolved provider + extended configs so the
    /// registry can build the model and redaction table without
    /// re-walking the layered config every attach. (Wiring the
    /// resolver inside the daemon lands with the daemon-side `/config`
    /// payload.)
    pub fn attach(
        &self,
        session_id: Option<Uuid>,
        project_root: Option<PathBuf>,
        providers_cfg: &ProvidersConfig,
        extended_cfg: &ExtendedConfig,
        client_no_sandbox: bool,
    ) -> Result<SessionWorkerHandle> {
        // Resume path.
        if let Some(id) = session_id {
            if let Some(handle) = self.lookup(id) {
                return Ok(handle);
            }
            let session = Session::resume(self.inner.db.clone(), id)
                .context("resuming session")?
                .ok_or_else(|| anyhow::anyhow!("unknown session {id}"))?;
            return self.start_worker(session, providers_cfg, extended_cfg, client_no_sandbox);
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
        self.start_worker(session, providers_cfg, extended_cfg, client_no_sandbox)
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
    ) -> Result<SessionWorkerHandle> {
        let session_id = session.id;
        let project_root = session.project_root.clone();

        // Build per-session redaction table from the session's
        // project_root + the daemon's env.
        let redact = RedactionTable::build(&extended_cfg.redact, &project_root)
            .context("building redaction table")?;
        let redact = Arc::new(redact);

        // Build the model from providers config. Errors out loud if
        // no provider is configured for the session's active model.
        let model = Arc::new(Model::from_config(providers_cfg).context("resolving model")?);

        let session = Arc::new(session);
        let handle = session_worker::spawn(
            session,
            self.inner.locks.clone(),
            redact,
            model,
            project_root,
            client_no_sandbox,
        );

        self.inner
            .workers
            .lock()
            .unwrap()
            .insert(session_id, handle.clone());

        Ok(handle)
    }

    /// Drop a session's worker handle from the registry. Called when
    /// the worker exits (session ended, daemon shutdown).
    pub fn forget(&self, session_id: Uuid) {
        self.inner.workers.lock().unwrap().remove(&session_id);
    }

    /// `Shutdown` every running worker and wait until they all exit.
    /// Called by the daemon's signal handler.
    pub async fn shutdown_all(&self) {
        let handles: Vec<SessionWorkerHandle> = {
            let workers = self.inner.workers.lock().unwrap();
            workers.values().cloned().collect()
        };
        for h in &handles {
            let _ = h
                .send_work(crate::daemon::session_worker::SessionWork::Shutdown)
                .await;
        }
        // The worker tasks set ended_at on the session row before
        // exiting; we don't have a join handle for them here (they're
        // detached `tokio::spawn`). For now the caller relies on the
        // signal-handler giving them a moment to finish; full
        // join-on-shutdown lands when we add a JoinSet.
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
}
