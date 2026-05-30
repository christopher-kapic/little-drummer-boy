//! Keyed resource lock for `exclusive` test concurrency (plan.md §4.1,
//! prompt 4).
//!
//! A plan-step test declared `concurrency: exclusive: <key>` must hold a
//! lock on its opaque resource string (`"port:8080"`, `"gpu0"`, …) for the
//! duration of the run: **no two tests holding the same key run
//! simultaneously, different keys parallelize, and `parallel` tests take no
//! lock at all** (prompt 4). This is the v1 contention mechanism;
//! per-worktree parameterized resource injection ("Way B") is explicitly
//! deferred and ships nothing.
//!
//! This is the cross-tree analogue of the single-tree file-lock manager
//! (`crate::locks`), and it reuses the **same primitive shape** that manager
//! is built from — a `Mutex`-guarded map keyed on a string, with per-key FIFO
//! waiters woken via `tokio::sync::Notify` — rather than introducing a
//! bespoke serializer or a second `DashMap`. Keyed on the resource string
//! instead of a canonical file path; otherwise identical machinery.
//!
//! Fairness: waiters are served in arrival (FIFO) order. A waiter that is
//! cancelled before its turn (the test's future is dropped) removes itself
//! from its key's queue on drop, so subsequent waiters never block on a ghost
//! — the same auto-cleanup invariant the file-lock manager enforces on agent
//! termination.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

/// One process-wide keyed-resource lock authority for a plan run. Cheap to
/// clone (it's an `Arc` inside); hand a clone to each concurrently-running
/// test future.
#[derive(Clone, Default)]
pub struct ResourceLocks {
    inner: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    /// Resource key → whether it is currently held + its FIFO waiter queue.
    keys: HashMap<String, KeyState>,
}

struct KeyState {
    held: bool,
    /// FIFO queue of waiters' wake handles. The head is the next to be
    /// granted on release.
    waiters: VecDeque<Arc<Notify>>,
}

impl ResourceLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the lock on `key`, waiting (FIFO) if another holder has it.
    /// Returns a guard that releases on drop (waking the next waiter).
    ///
    /// Held immediately if the key is free; otherwise the caller registers a
    /// `Notify` at the tail of the queue and parks until it is both at the
    /// head and the key is free.
    pub async fn acquire(&self, key: &str) -> ResourceGuard {
        // Fast path: free key, take it.
        {
            let mut st = self.inner.lock().unwrap();
            let entry = st.keys.entry(key.to_string()).or_insert_with(|| KeyState {
                held: false,
                waiters: VecDeque::new(),
            });
            if !entry.held && entry.waiters.is_empty() {
                entry.held = true;
                return ResourceGuard {
                    locks: self.clone(),
                    key: key.to_string(),
                    released: false,
                };
            }
        }

        // Slow path: enqueue a notify and wait for our turn. We register a
        // fresh `Notify` and only proceed when we are at the head AND the key
        // is free. A `WaiterCleanup` guard removes us from the queue if this
        // future is dropped (cancelled) before we win the lock.
        let notify = Arc::new(Notify::new());
        {
            let mut st = self.inner.lock().unwrap();
            let entry = st.keys.get_mut(key).expect("key inserted above");
            entry.waiters.push_back(notify.clone());
        }
        let cleanup = WaiterCleanup {
            locks: self.clone(),
            key: key.to_string(),
            notify: notify.clone(),
            disarmed: std::cell::Cell::new(false),
        };

        loop {
            // Try to claim: are we at the head and is the key free?
            {
                let mut st = self.inner.lock().unwrap();
                let entry = st.keys.get_mut(key).expect("key present while waiting");
                let at_head = entry
                    .waiters
                    .front()
                    .map(|n| Arc::ptr_eq(n, &notify))
                    .unwrap_or(false);
                if at_head && !entry.held {
                    entry.waiters.pop_front();
                    entry.held = true;
                    cleanup.disarm();
                    return ResourceGuard {
                        locks: self.clone(),
                        key: key.to_string(),
                        released: false,
                    };
                }
            }
            // Not our turn yet — park until a release wakes us.
            notify.notified().await;
        }
    }

    /// Release `key` and wake the next FIFO waiter. Called from the guard's
    /// `Drop`. Idempotent on an already-free key.
    fn release(&self, key: &str) {
        let mut st = self.inner.lock().unwrap();
        if let Some(entry) = st.keys.get_mut(key) {
            entry.held = false;
            if let Some(next) = entry.waiters.front() {
                next.notify_one();
            } else {
                // No waiters and not held — drop the empty entry to keep the
                // map from growing unbounded across many distinct keys.
                st.keys.remove(key);
            }
        }
    }

    /// Test-only: is `key` currently held?
    #[cfg(test)]
    fn is_held(&self, key: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .keys
            .get(key)
            .map(|k| k.held)
            .unwrap_or(false)
    }
}

/// RAII guard for a held resource lock. Releasing on drop wakes the next
/// FIFO waiter.
pub struct ResourceGuard {
    locks: ResourceLocks,
    key: String,
    released: bool,
}

impl Drop for ResourceGuard {
    fn drop(&mut self) {
        if !self.released {
            self.locks.release(&self.key);
        }
    }
}

/// Removes a waiter's `Notify` from its key's FIFO queue if the waiting
/// future is dropped before it wins the lock — so a cancelled test never
/// leaves a ghost entry that blocks the rest of the queue. Disarmed once the
/// waiter is promoted to holder.
struct WaiterCleanup {
    locks: ResourceLocks,
    key: String,
    notify: Arc<Notify>,
    /// `Cell` so the no-longer-needed flag can be set through the `&self` we
    /// hold right before returning the lock guard.
    disarmed: std::cell::Cell<bool>,
}

impl WaiterCleanup {
    fn disarm(&self) {
        self.disarmed.set(true);
    }
}

impl Drop for WaiterCleanup {
    fn drop(&mut self) {
        if self.disarmed.get() {
            return;
        }
        let mut st = self.locks.inner.lock().unwrap();
        if let Some(entry) = st.keys.get_mut(&self.key) {
            // Remove our notify from wherever it sits in the queue.
            if let Some(pos) = entry
                .waiters
                .iter()
                .position(|n| Arc::ptr_eq(n, &self.notify))
            {
                entry.waiters.remove(pos);
            }
            // If we were the head, a release may have woken us but we're
            // bailing — pass the wake along to the new head.
            if !entry.held
                && let Some(next) = entry.waiters.front()
            {
                next.notify_one();
            }
            if !entry.held && entry.waiters.is_empty() {
                st.keys.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn same_key_serializes() {
        let locks = ResourceLocks::new();
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let locks = locks.clone();
            let active = active.clone();
            let max_seen = max_seen.clone();
            handles.push(tokio::spawn(async move {
                let _g = locks.acquire("port:8080").await;
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(15)).await;
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "no two holders of the same key may overlap"
        );
        assert!(!locks.is_held("port:8080"), "lock free after all release");
    }

    #[tokio::test]
    async fn different_keys_parallelize() {
        let locks = ResourceLocks::new();
        // Acquire two distinct keys; both should be held simultaneously
        // without either blocking.
        let g1 = locks.acquire("a").await;
        let g2 = tokio::time::timeout(Duration::from_millis(200), locks.acquire("b"))
            .await
            .expect("distinct key must not block");
        assert!(locks.is_held("a") && locks.is_held("b"));
        drop(g1);
        drop(g2);
    }

    #[tokio::test]
    async fn fifo_order_is_respected() {
        let locks = ResourceLocks::new();
        let order = Arc::new(Mutex::new(Vec::<u32>::new()));

        // Hold the key, then queue three waiters in a known order, then
        // release and confirm they run FIFO.
        let held = locks.acquire("k").await;

        let mut handles = Vec::new();
        for i in 0..3u32 {
            let locks = locks.clone();
            let order = order.clone();
            handles.push(tokio::spawn(async move {
                let _g = locks.acquire("k").await;
                order.lock().unwrap().push(i);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }));
            // Stagger spawns so enqueue order is deterministic.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(held); // release → wake head of FIFO
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2], "FIFO grant order");
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_block_queue() {
        let locks = ResourceLocks::new();
        let held = locks.acquire("k").await;

        // Waiter A is cancelled before it ever wins.
        let locks_a = locks.clone();
        let a = tokio::spawn(async move {
            let _g = locks_a.acquire("k").await;
            // Should never reach here in the test window.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Waiter B queues behind A.
        let got_b = Arc::new(AtomicUsize::new(0));
        let locks_b = locks.clone();
        let got_b2 = got_b.clone();
        let b = tokio::spawn(async move {
            let _g = locks_b.acquire("k").await;
            got_b2.fetch_add(1, Ordering::SeqCst);
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Cancel A, then release the lock. B must get it despite A being
        // ahead in the queue — A's drop removes its ghost waiter.
        a.abort();
        let _ = a.await;
        drop(held);

        tokio::time::timeout(Duration::from_millis(500), b)
            .await
            .expect("B must acquire after A cancelled")
            .unwrap();
        assert_eq!(got_b.load(Ordering::SeqCst), 1);
    }
}
