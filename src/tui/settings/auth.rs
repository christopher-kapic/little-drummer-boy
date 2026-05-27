//! Background async work owned by the settings dialog: Codex
//! device-code OAuth login and the `/models` fetch behind the
//! provider Save/Refetch actions.
//!
//! Both types are shared-cell wrappers: a tokio task writes into an
//! `Arc<Mutex<…>>`, the dialog's tick polls it on each event-loop
//! pass. They live here rather than in the main dialog file because
//! they are async plumbing, not UI state.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::providers::ProviderEntry;
use crate::providers::models_fetch::{self, FetchOutcome};

/// Codex device-code OAuth login state, shared between the dialog's
/// render path and the background task driving the flow.
pub struct CodexLoginState {
    shared: Arc<Mutex<CodexLoginProgress>>,
}

#[derive(Clone)]
pub enum CodexLoginProgress {
    /// POSTing to the usercode endpoint.
    Requesting,
    /// Server returned a user code; waiting for the user to enter it
    /// in a browser and for the poll loop to receive an authorization
    /// code.
    AwaitingUser {
        verification_url: String,
        user_code: String,
    },
    /// Persisted; the dialog can finalize the ProviderEntry.
    Success {
        saved_at: chrono::DateTime<chrono::Utc>,
    },
    /// Flow failed at any step. The dialog should show the message
    /// and let the user retry.
    Error(String),
}

impl CodexLoginState {
    pub fn spawn() -> Self {
        let cfg = crate::auth::codex::LoginConfig::default();
        Self::spawn_with(cfg)
    }

    pub fn spawn_with(cfg: crate::auth::codex::LoginConfig) -> Self {
        let shared = Arc::new(Mutex::new(CodexLoginProgress::Requesting));
        let w = Arc::clone(&shared);
        tokio::spawn(async move {
            match crate::auth::codex::request_device_code(&cfg).await {
                Err(e) => set(&w, CodexLoginProgress::Error(e.to_string())),
                Ok(device) => {
                    set(
                        &w,
                        CodexLoginProgress::AwaitingUser {
                            verification_url: device.verification_url.clone(),
                            user_code: device.user_code.clone(),
                        },
                    );
                    match crate::auth::codex::complete_login(&cfg, &device).await {
                        Err(e) => set(&w, CodexLoginProgress::Error(e.to_string())),
                        Ok(stored) => set(
                            &w,
                            CodexLoginProgress::Success {
                                saved_at: stored.saved_at,
                            },
                        ),
                    }
                }
            }
        });
        Self { shared }
    }

    pub fn snapshot(&self) -> CodexLoginProgress {
        self.shared
            .lock()
            .map(|g| g.clone())
            .unwrap_or(CodexLoginProgress::Error("poisoned login state".into()))
    }
}

fn set(shared: &Arc<Mutex<CodexLoginProgress>>, value: CodexLoginProgress) {
    if let Ok(mut g) = shared.lock() {
        *g = value;
    }
}

/// Shared cell for an in-flight `/models` fetch. The background task
/// writes the result; the event loop polls it on each tick.
#[derive(Clone)]
pub struct FetchHandle {
    pub provider_id: String,
    pub state: Arc<Mutex<FetchState>>,
}

pub enum FetchState {
    Running,
    Done(Result<FetchOutcome, String>),
    /// Consumed already — left as a terminal marker so the dialog
    /// doesn't double-apply the result.
    Consumed,
}

impl FetchHandle {
    pub fn spawn(provider_id: String, entry: ProviderEntry) -> Self {
        let state = Arc::new(Mutex::new(FetchState::Running));
        let state_w = Arc::clone(&state);
        let pid = provider_id.clone();
        tokio::spawn(async move {
            let result = match models_fetch::resolve_provider_request(&pid, &entry) {
                Err(e) => Err(e.to_string()),
                Ok(r) => models_fetch::fetch_models(
                    &r.base_url,
                    &r.headers,
                    Some(Duration::from_secs(15)),
                )
                .await
                .map_err(|e| e.to_string()),
            };
            if let Ok(mut s) = state_w.lock() {
                *s = FetchState::Done(result);
            }
        });
        Self { provider_id, state }
    }

    pub fn take(&self) -> Option<Result<FetchOutcome, String>> {
        let mut s = self.state.lock().ok()?;
        match std::mem::replace(&mut *s, FetchState::Consumed) {
            FetchState::Running => {
                *s = FetchState::Running;
                None
            }
            FetchState::Done(r) => Some(r),
            FetchState::Consumed => None,
        }
    }
}
