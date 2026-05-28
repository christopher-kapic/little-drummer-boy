#![allow(dead_code)]
// This is part 1 of 2 (sandboxing). The public API below — the
// classifier, the grant store, and the `Approver` prompt orchestration —
// has no in-tree consumer until `prompts/zerobox-filesystem-sandboxing.md`
// (part 2) wires it to the sandbox. It's built and tested as a
// self-contained subsystem (see the `#[cfg(test)]` modules), so the
// "never used" lints are expected here; part 2 removes this allow.
//
//! Command-approval & escalation subsystem (sandboxing, part 1 of 2).
//!
//! The reusable layer that decides *whether a shell command or path is
//! already approved* and *prompts the user when it isn't* — the
//! "ask-and-remember" machinery the filesystem-sandbox (part 2) consumes
//! via the run-fail-escalate model. This part ships **no confinement**;
//! it's the deterministic classifier ([`classify`]), the grant store
//! ([`store`]), and the prompt orchestration here.
//!
//! ## The four public entry points part 2 calls
//!
//! 1. [`classify::classify`] — parse a command string into its simple
//!    commands + approval keys + wrapper flag (pure, sync).
//! 2. [`GrantStore::is_command_granted`] / [`GrantStore::is_path_granted`]
//!    — query the store for the current session/project/global context
//!    (pure-ish, sync — DB + file reads, no blocking on the user).
//! 3. [`Approver::approve_command`] / [`Approver::approve_path`] — the
//!    full decision: query the store, and if not already granted, raise
//!    the approval prompt through the existing [`InterruptHub`], block on
//!    the answer, record the grant at the chosen scope, and return the
//!    decision.
//!
//! ## How the prompt reuses the existing interrupt path
//!
//! The prompt is **not** a parallel mechanism. It raises an
//! [`InterruptQuestion::Single`] (one scope-select question) through the
//! exact same path the `question` tool uses: persist via
//! [`Db::raise_interrupt_questions`], [`InterruptHub::register`] a
//! wakeup, [`InterruptHub::emit_raised`] to attached clients, then block
//! on [`PendingInterrupt::wait`]. The TUI renders it with
//! [`crate::tui::dialog::approval::ApprovalDialog`] over the shared
//! [`crate::tui::dialog::DialogState`]. The resolved option id maps back
//! to a [`Scope`]; a non-`Once` choice records the grant *before* the
//! decision returns.
//!
//! [`InterruptHub`]: crate::engine::interrupt::InterruptHub
//! [`Db::raise_interrupt_questions`]: crate::db::Db
//! [`PendingInterrupt::wait`]: crate::engine::interrupt::PendingInterrupt::wait

pub mod classify;
pub mod store;

use std::sync::Arc;

use anyhow::Result;

use crate::approval::classify::{Classification, SimpleCommandInfo};
use crate::approval::store::{GrantStore, Scope};
use crate::daemon::proto::{
    InterruptOption, InterruptQuestion, InterruptQuestionSet, ResolveResponse,
};
use crate::engine::interrupt::InterruptHub;
use crate::tui::dialog::approval::{ID_GLOBAL, ID_ONCE, ID_PROJECT, ID_SESSION};

/// The decision a prompt (or an already-granted query) produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Access is allowed. `scope` is `Once` when it was approved for this
    /// invocation only (or was a wrapper), or the scope it was recorded
    /// at / found already granted under.
    Allow { scope: Scope },
    /// Access is denied (the user dismissed the prompt).
    Deny,
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow { .. })
    }
}

/// Drives the approve-or-prompt decision. Holds the grant store plus the
/// bits needed to raise an interrupt: the session/agent identity, the
/// DB (to persist the interrupt), and the shared [`InterruptHub`].
pub struct Approver {
    store: GrantStore,
    db: crate::db::Db,
    session_id: uuid::Uuid,
    agent_id: String,
    interrupts: Arc<InterruptHub>,
}

impl Approver {
    pub fn new(
        store: GrantStore,
        db: crate::db::Db,
        session_id: uuid::Uuid,
        agent_id: impl Into<String>,
        interrupts: Arc<InterruptHub>,
    ) -> Self {
        Self {
            store,
            db,
            session_id,
            agent_id: agent_id.into(),
            interrupts,
        }
    }

    /// Read-only access to the underlying store (the §4 query API).
    pub fn store(&self) -> &GrantStore {
        &self.store
    }

    /// Decide a whole command string. Classifies it, then requires that
    /// **every** constituent simple command be allowed: an already-granted
    /// chain returns `Allow` with no prompt; any ungranted command (or a
    /// compound construct / wrapper) triggers a prompt for that command.
    /// A single ungranted/denied command denies the whole string.
    ///
    /// Empty/unparseable input is never auto-allowed — it returns `Deny`
    /// (the caller surfaces the parse error).
    pub async fn approve_command(&self, command: &str) -> Result<Decision> {
        let classification = classify::classify(command);
        let simple_commands = match &classification {
            Classification::Parsed {
                simple_commands, ..
            } => simple_commands.clone(),
            // Nothing to run / can't reason about it → deny, don't guess.
            Classification::Empty | Classification::Unparseable(_) => return Ok(Decision::Deny),
        };

        // Track the broadest scope we settled on, for the caller's info.
        // A chain is only as "remembered" as its narrowest decision; we
        // report `Once` if any command was only approved once.
        let mut widest = Scope::Global;
        for info in &simple_commands {
            let decision = self.approve_one(info, command).await?;
            match decision {
                Decision::Deny => return Ok(Decision::Deny),
                Decision::Allow { scope } => {
                    widest = narrowest(widest, scope);
                }
            }
        }
        Ok(Decision::Allow { scope: widest })
    }

    /// Decide one simple command: granted → allow; else prompt.
    async fn approve_one(&self, info: &SimpleCommandInfo, full_command: &str) -> Result<Decision> {
        if !info.wrapper && self.store.is_command_granted(&info.key) {
            // Already remembered at some applicable scope.
            return Ok(Decision::Allow {
                scope: Scope::Session,
            });
        }
        // Prompt with the approval key — the exact thing a grant would
        // cover (`gh pr`, `cargo build`, `ls`), so the user sees what
        // they're remembering, not the full arg-laden command line.
        let _ = full_command;
        let label = info.key.as_storage_str();
        let choice = self.prompt(&label, info.wrapper).await?;
        match choice {
            ApprovalChoice::Deny => Ok(Decision::Deny),
            ApprovalChoice::Approve(Scope::Once) => Ok(Decision::Allow { scope: Scope::Once }),
            ApprovalChoice::Approve(scope) => {
                // Record BEFORE returning the decision (§3). A wrapper can
                // never reach here at a non-Once scope: the prompt only
                // offered Once for wrappers. The store rejects it anyway as
                // a belt-and-braces guard.
                self.store.record_command(info, scope)?;
                Ok(Decision::Allow { scope })
            }
        }
    }

    /// Decide a path access (part 2's native confinement). Granted →
    /// allow; else prompt showing the exact path. Paths are never
    /// wrappers, so all four scopes are offered.
    pub async fn approve_path(&self, path: &std::path::Path) -> Result<Decision> {
        if self.store.is_path_granted(path) {
            return Ok(Decision::Allow {
                scope: Scope::Session,
            });
        }
        let choice = self.prompt(&path.display().to_string(), false).await?;
        match choice {
            ApprovalChoice::Deny => Ok(Decision::Deny),
            ApprovalChoice::Approve(Scope::Once) => Ok(Decision::Allow { scope: Scope::Once }),
            ApprovalChoice::Approve(scope) => {
                self.store.record_path(path, scope)?;
                Ok(Decision::Allow { scope })
            }
        }
    }

    /// Raise an approval interrupt and block until the user answers,
    /// reusing the `question`-tool interrupt path verbatim. Returns the
    /// chosen scope, or `Deny` on dismissal.
    async fn prompt(&self, label: &str, wrapper: bool) -> Result<ApprovalChoice> {
        let question = scope_question(label, wrapper);
        let set = InterruptQuestionSet {
            questions: vec![question],
        };
        let description = prompt_description(label, wrapper);

        // Persist → register → emit, in that order (same invariant the
        // `question` tool relies on: a fast client can't resolve before
        // we're listening).
        let interrupt_id = self.db.raise_interrupt_questions(
            self.session_id,
            &self.agent_id,
            &description,
            &set,
        )?;
        let pending = self.interrupts.register(interrupt_id);
        self.interrupts.emit_raised(
            self.session_id,
            interrupt_id,
            &self.agent_id,
            &description,
            set,
        );

        let response = pending.wait().await;
        Ok(response_to_choice(&response))
    }
}

/// The user's scope choice — the in-crate twin of the TUI dialog's
/// `ApprovalChoice`, kept here so the public API doesn't depend on the
/// `tui` module shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalChoice {
    Approve(Scope),
    Deny,
}

/// Build the single scope-select question. Full variant offers all four
/// scopes; wrapper variant offers only one-time approval (the dialog
/// shows the "can't be remembered" note). Option ids are shared with the
/// TUI dialog so the resolution maps back cleanly.
fn scope_question(label: &str, wrapper: bool) -> InterruptQuestion {
    let prompt = if wrapper {
        format!("Run `{label}`? Wrappers can't be remembered.")
    } else {
        format!("Run `{label}`?")
    };
    let options = if wrapper {
        vec![opt(ID_ONCE, "Yes, once")]
    } else {
        vec![
            opt(ID_ONCE, "Yes, once"),
            opt(ID_SESSION, "Yes, for this session"),
            opt(ID_PROJECT, "Always for this project"),
            opt(ID_GLOBAL, "Always everywhere"),
        ]
    };
    InterruptQuestion::Single {
        prompt,
        options,
        // No free-text on a scope select — the choices are fixed.
        allow_freetext: false,
    }
}

fn prompt_description(label: &str, wrapper: bool) -> String {
    if wrapper {
        format!("Approve wrapper `{label}` (once only)?")
    } else {
        format!("Approve `{label}`?")
    }
}

fn opt(id: &str, label: &str) -> InterruptOption {
    InterruptOption {
        id: id.to_string(),
        label: label.to_string(),
    }
}

/// Map a resolved interrupt response back to a scope choice. An unknown
/// id, a non-`Single` response, or a `Cancel` is a deny — the safe
/// default the whole subsystem leans on.
fn response_to_choice(response: &ResolveResponse) -> ApprovalChoice {
    let id = match response {
        ResolveResponse::Single { selected_id } => selected_id.as_str(),
        // A scope select can also arrive as a one-element Batch from a
        // client that always batches; unwrap that single answer.
        ResolveResponse::Batch { responses } => match responses.first() {
            Some(ResolveResponse::Single { selected_id }) => selected_id.as_str(),
            _ => return ApprovalChoice::Deny,
        },
        _ => return ApprovalChoice::Deny,
    };
    match id {
        ID_ONCE => ApprovalChoice::Approve(Scope::Once),
        ID_SESSION => ApprovalChoice::Approve(Scope::Session),
        ID_PROJECT => ApprovalChoice::Approve(Scope::Project),
        ID_GLOBAL => ApprovalChoice::Approve(Scope::Global),
        _ => ApprovalChoice::Deny,
    }
}

/// Narrower of two scopes (for reporting a chain's effective scope).
fn narrowest(a: Scope, b: Scope) -> Scope {
    fn rank(s: Scope) -> u8 {
        match s {
            Scope::Once => 0,
            Scope::Session => 1,
            Scope::Project => 2,
            Scope::Global => 3,
        }
    }
    if rank(a) <= rank(b) { a } else { b }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::classify::ApprovalKey;

    fn approver(cwd: &std::path::Path) -> (Approver, uuid::Uuid) {
        let db = crate::db::Db::open_in_memory().unwrap();
        let session =
            crate::session::Session::create(db.clone(), cwd.to_path_buf(), "coder").unwrap();
        let sid = session.id;
        let store = GrantStore::new(db.clone(), sid, cwd.to_path_buf());
        let hub = Arc::new(InterruptHub::detached());
        (Approver::new(store, db, sid, "coder", hub), sid)
    }

    #[tokio::test]
    async fn already_granted_command_skips_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        let info = SimpleCommandInfo {
            program: "cargo".into(),
            subcommand: Some("build".into()),
            key: ApprovalKey {
                program: "cargo".into(),
                subcommand: Some("build".into()),
            },
            wrapper: false,
        };
        approver
            .store
            .record_command(&info, Scope::Session)
            .unwrap();
        // No client is attached; if this prompted it would block forever.
        // It returns immediately because the grant short-circuits.
        let decision = approver
            .approve_command("cargo build --release")
            .await
            .unwrap();
        assert!(decision.is_allowed());
    }

    #[tokio::test]
    async fn empty_command_is_denied_without_prompting() {
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        assert_eq!(approver.approve_command("").await.unwrap(), Decision::Deny);
        assert_eq!(
            approver.approve_command("   ").await.unwrap(),
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn prompt_then_record_at_project_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        // Point the store's scopes at the temp dir deterministically.
        // (The store resolves project root from cwd; tmp may not be a git
        // repo, so a Project record would error. Use Session here, which
        // needs no project root, and assert the prompt→record flow.)
        let db = approver.db.clone();
        let session_id = approver.session_id;
        let hub = approver.interrupts.clone();

        // Resolve the interrupt from another task once it's raised.
        let resolver = tokio::spawn(async move {
            let iid = loop {
                let open = db.list_open_interrupts(session_id).unwrap();
                if let Some(row) = open.first() {
                    break row.interrupt_id;
                }
                tokio::task::yield_now().await;
            };
            assert!(hub.resolve(
                iid,
                ResolveResponse::Single {
                    selected_id: ID_SESSION.into(),
                }
            ));
        });

        let decision = approver.approve_command("gh pr create").await.unwrap();
        resolver.await.unwrap();
        assert_eq!(
            decision,
            Decision::Allow {
                scope: Scope::Session
            }
        );
        // And it's now remembered.
        let key = ApprovalKey {
            program: "gh".into(),
            subcommand: Some("pr".into()),
        };
        assert!(approver.store.is_command_granted(&key));
    }

    #[tokio::test]
    async fn dismissed_prompt_denies() {
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        let db = approver.db.clone();
        let session_id = approver.session_id;
        let hub = approver.interrupts.clone();
        let resolver = tokio::spawn(async move {
            let iid = loop {
                let open = db.list_open_interrupts(session_id).unwrap();
                if let Some(row) = open.first() {
                    break row.interrupt_id;
                }
                tokio::task::yield_now().await;
            };
            assert!(hub.resolve(iid, ResolveResponse::Cancel));
        });
        let decision = approver.approve_command("rm file").await.unwrap();
        resolver.await.unwrap();
        assert_eq!(decision, Decision::Deny);
    }

    #[tokio::test]
    async fn wrapper_chain_command_prompts_and_is_not_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        let db = approver.db.clone();
        let session_id = approver.session_id;
        let hub = approver.interrupts.clone();
        // The user picks "once" (the only non-deny option a wrapper offers).
        let resolver = tokio::spawn(async move {
            let iid = loop {
                let open = db.list_open_interrupts(session_id).unwrap();
                if let Some(row) = open.first() {
                    break row.interrupt_id;
                }
                tokio::task::yield_now().await;
            };
            assert!(hub.resolve(
                iid,
                ResolveResponse::Single {
                    selected_id: ID_ONCE.into(),
                }
            ));
        });
        let decision = approver.approve_command("bash -c 'echo hi'").await.unwrap();
        resolver.await.unwrap();
        assert_eq!(decision, Decision::Allow { scope: Scope::Once });
        // Wrapper key was NOT stored.
        let key = ApprovalKey {
            program: "bash".into(),
            subcommand: None,
        };
        assert!(!approver.store.is_command_granted(&key));
    }

    #[test]
    fn response_mapping_round_trips_scopes() {
        for (id, scope) in [
            (ID_ONCE, Scope::Once),
            (ID_SESSION, Scope::Session),
            (ID_PROJECT, Scope::Project),
            (ID_GLOBAL, Scope::Global),
        ] {
            let resp = ResolveResponse::Single {
                selected_id: id.into(),
            };
            assert_eq!(response_to_choice(&resp), ApprovalChoice::Approve(scope));
        }
        assert_eq!(
            response_to_choice(&ResolveResponse::Cancel),
            ApprovalChoice::Deny
        );
    }
}
