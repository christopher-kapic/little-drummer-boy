//! Interrupt wakeup hub — the bridge that lets a blocked structural
//! tool (`question`, GOALS §3b) wait on a human answer that arrives,
//! out of band, on the daemon's `ResolveInterrupt` path.
//!
//! ## Why this exists
//!
//! The `question` tool runs inside the driver's tool-dispatch loop. It
//! must *block* until the user answers. But the answer round-trips
//! daemon ↔ client over NDJSON and lands in the **session worker's**
//! work loop ([`crate::daemon::session_worker`]) as
//! `SessionWork::ResolveInterrupt` — a different task from the one the
//! tool call is suspended in. The two need a rendezvous.
//!
//! The hub is that rendezvous: a shared registry of
//! `interrupt_id -> oneshot::Sender<ResolveResponse>`. The tool
//! [`register`](InterruptHub::register)s a channel, persists the
//! interrupt, emits the `InterruptRaised` event, and awaits the
//! receiver. The worker, on `ResolveInterrupt`, persists the response
//! and calls [`resolve`](InterruptHub::resolve), which fires the
//! matching sender and wakes the tool.
//!
//! ## Headless / no client
//!
//! Nothing in the hub times out. If no interactive client is attached
//! (headless daemon, scheduled run), the interrupt simply parks in the
//! `needs_attention` table and the tool's `await` blocks indefinitely
//! until *some* client answers — the TUI today, the remote dashboard
//! later (GOALS north star). That is the intended behavior.
//!
//! ## Single authority, like the lock manager
//!
//! One hub per session worker; both the driver (which threads it into
//! every [`crate::engine::tool::ToolCtx`]) and the worker's resolve
//! handler hold an `Arc` to the same instance. The `Mutex` is held only
//! for map insert/remove — never across an `.await`.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

use crate::daemon::proto::{self, InterruptQuestionSet, ResolveResponse};

/// Shared interrupt rendezvous. Cheap to clone via `Arc`.
pub struct InterruptHub {
    /// Pending wakeups keyed by interrupt id. A sender is inserted by
    /// [`Self::register`] and removed when [`Self::resolve`] fires it
    /// (or when the [`PendingInterrupt`] guard drops on cancellation).
    waiters: Mutex<HashMap<Uuid, oneshot::Sender<ResolveResponse>>>,
    /// Outbound event channel to attached clients. `None` in
    /// non-daemon paths (tool unit tests, the standalone run shim) where
    /// no client is listening — raising still works; the event is just
    /// not broadcast. Cloned from the session worker's fan-out sender.
    events: Option<broadcast::Sender<proto::Event>>,
}

impl InterruptHub {
    /// Build a hub wired to the worker's client event fan-out. Pass the
    /// session worker's `broadcast::Sender<proto::Event>` so raised
    /// interrupts reach every attached client.
    pub fn new(events: broadcast::Sender<proto::Event>) -> Self {
        Self {
            waiters: Mutex::new(HashMap::new()),
            events: Some(events),
        }
    }

    /// Build a detached hub with no client fan-out. Used where no client
    /// is attached (tests, the standalone shim): wakeups still work via
    /// [`Self::resolve`], but no `InterruptRaised` event is emitted.
    pub fn detached() -> Self {
        Self {
            waiters: Mutex::new(HashMap::new()),
            events: None,
        }
    }

    /// Register a wakeup for `interrupt_id` and return the guard the
    /// caller awaits. The guard removes its registry entry on drop, so a
    /// tool whose future is cancelled (e.g. the worker shuts down) never
    /// leaves a dangling sender.
    pub fn register(&self, interrupt_id: Uuid) -> PendingInterrupt<'_> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(interrupt_id, tx);
        PendingInterrupt {
            hub: self,
            interrupt_id,
            rx: Some(rx),
        }
    }

    /// Emit `InterruptRaised` to attached clients (no-op when detached).
    /// The `question` tool calls this right after persisting the
    /// interrupt and registering the wakeup, so a client can render the
    /// answering dialog.
    pub fn emit_raised(
        &self,
        session_id: Uuid,
        interrupt_id: Uuid,
        agent: &str,
        description: &str,
        questions: InterruptQuestionSet,
    ) {
        if let Some(events) = &self.events {
            // `send` errors only when there are no subscribers — fine,
            // the interrupt still parks in the DB for the next client.
            let _ = events.send(proto::Event::InterruptRaised {
                session_id,
                interrupt_id,
                agent: agent.to_string(),
                description: description.to_string(),
                question: None,
                questions: Some(questions),
            });
        }
    }

    /// Deliver a resolution to whoever is blocked on `interrupt_id`.
    /// Returns `true` if a waiter was woken. `false` means no tool was
    /// blocked on it locally — e.g. the worker restarted and the
    /// in-flight tool future was dropped, or the resolution targets a
    /// `jobs` needs-attention nudge that nobody awaits. The DB row has
    /// already been updated by the caller regardless.
    pub fn resolve(&self, interrupt_id: Uuid, response: ResolveResponse) -> bool {
        let Some(tx) = self.waiters.lock().unwrap().remove(&interrupt_id) else {
            return false;
        };
        tx.send(response).is_ok()
    }
}

/// Guard returned by [`InterruptHub::register`]. Awaiting it (via
/// [`Self::wait`]) blocks until [`InterruptHub::resolve`] fires for this
/// id; dropping it without resolving removes the registry entry so no
/// stale sender lingers.
pub struct PendingInterrupt<'a> {
    hub: &'a InterruptHub,
    interrupt_id: Uuid,
    /// `Option` so [`Self::wait`] can take the receiver out of `self`
    /// without fighting the `Drop` guard (a `Drop` type can't be moved
    /// out of field-by-field).
    rx: Option<oneshot::Receiver<ResolveResponse>>,
}

impl PendingInterrupt<'_> {
    /// Block until resolved. Returns the human's resolution, or
    /// [`ResolveResponse::Cancel`] if the wakeup channel closed without
    /// a value (only happens on worker teardown — the agent treats it
    /// as a dismissal, the safe default).
    pub async fn wait(mut self) -> ResolveResponse {
        let rx = self.rx.take().expect("wait called once");
        match rx.await {
            Ok(response) => response,
            Err(_) => ResolveResponse::Cancel,
        }
    }
}

impl Drop for PendingInterrupt<'_> {
    fn drop(&mut self) {
        // Idempotent: `resolve` already removed it on the happy path.
        self.hub.waiters.lock().unwrap().remove(&self.interrupt_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_wakes_a_registered_waiter() {
        let hub = InterruptHub::detached();
        let id = Uuid::new_v4();
        let pending = hub.register(id);
        assert!(hub.resolve(
            id,
            ResolveResponse::Single {
                selected_id: "y".into(),
            }
        ));
        let got = pending.wait().await;
        assert!(matches!(got, ResolveResponse::Single { selected_id } if selected_id == "y"));
    }

    #[tokio::test]
    async fn resolve_unknown_id_returns_false() {
        let hub = InterruptHub::detached();
        assert!(!hub.resolve(Uuid::new_v4(), ResolveResponse::Cancel));
    }

    #[tokio::test]
    async fn dropping_pending_clears_the_registry() {
        let hub = InterruptHub::detached();
        let id = Uuid::new_v4();
        let pending = hub.register(id);
        drop(pending);
        // No waiter remains, so a late resolve finds nothing.
        assert!(!hub.resolve(id, ResolveResponse::Cancel));
    }

    #[tokio::test]
    async fn dropped_sender_resolves_to_cancel() {
        // Worker teardown: the registry is cleared (sender dropped)
        // while a tool is still awaiting. `wait` must yield `Cancel`.
        let hub = InterruptHub::detached();
        let id = Uuid::new_v4();
        let pending = hub.register(id);
        hub.waiters.lock().unwrap().clear();
        assert!(matches!(pending.wait().await, ResolveResponse::Cancel));
    }
}
