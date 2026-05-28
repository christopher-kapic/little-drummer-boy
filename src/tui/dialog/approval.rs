//! Command/path-approval wiring over the reusable [`DialogState`]
//! (sandboxing part 1, §3).
//!
//! The thin use-case layer the dialog core was designed to support
//! (mirroring [`super::question`]): it builds the single scope-select
//! page, drives the shared state machine, and maps the resulting
//! [`Answer`] back to an [`ApprovalChoice`] the approval subsystem
//! records. A flagged wrapper gets a restricted page — only "Yes (once)"
//! and deny — plus a one-line note that wrappers can't be remembered.

use std::time::Duration;

use crossterm::event::KeyEvent;
use uuid::Uuid;

use crate::approval::store::Scope;
use crate::tui::dialog::{Answer, DialogOption, DialogOutcome, DialogState, Page};

/// Stable option ids for the scope select. These ride through the
/// interrupt as the selected id and map back to a [`Scope`].
pub const ID_ONCE: &str = "once";
pub const ID_SESSION: &str = "session";
pub const ID_PROJECT: &str = "project";
pub const ID_GLOBAL: &str = "global";

/// The user's choice on an approval prompt. `Deny` is the dismissal
/// path (Esc / cancel); everything else approves at the named scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalChoice {
    Approve(Scope),
    Deny,
}

/// What the host should do once the approval dialog closes.
#[derive(Debug, Clone)]
pub enum ApprovalResult {
    /// Resolve `interrupt_id` with the chosen scope (or deny).
    Resolved {
        interrupt_id: Uuid,
        choice: ApprovalChoice,
    },
}

/// The App-facing approval dialog overlay. Owns a [`DialogState`] built
/// from one scope-select page, plus the interrupt id it resolves.
pub struct ApprovalDialog {
    interrupt_id: Uuid,
    /// Whether this is the wrapper-restricted variant (once/deny only).
    wrapper: bool,
    state: DialogState,
    result: Option<ApprovalResult>,
}

impl ApprovalDialog {
    /// Build the dialog for a raised approval interrupt. `prompt` is the
    /// command or path being requested (already terse, §10). `wrapper`
    /// selects the restricted variant. `lockout` is the anti-misfire
    /// delay shared with the question dialog.
    pub fn new(interrupt_id: Uuid, prompt: String, wrapper: bool, lockout: Duration) -> Self {
        let state = DialogState::new(vec![scope_page(prompt, wrapper)], lockout);
        Self {
            interrupt_id,
            wrapper,
            state,
            result: None,
        }
    }

    /// The dialog state, for the host renderer.
    pub fn state(&self) -> &DialogState {
        &self.state
    }

    /// Whether this is the wrapper-restricted variant — the renderer
    /// shows the "wrappers can't be remembered" note when true.
    pub fn is_wrapper(&self) -> bool {
        self.wrapper
    }

    /// Drain the close result once [`handle_key`](Self::handle_key)
    /// returned `true`.
    pub fn take_result(&mut self) -> Option<ApprovalResult> {
        self.result.take()
    }

    /// Route a key. Returns `true` when the dialog wants to close.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        match self.state.handle_key(key) {
            DialogOutcome::Continue => false,
            DialogOutcome::Cancel => {
                self.result = Some(ApprovalResult::Resolved {
                    interrupt_id: self.interrupt_id,
                    choice: ApprovalChoice::Deny,
                });
                true
            }
            DialogOutcome::Submit(answers) => {
                let choice = answers
                    .first()
                    .map(answer_to_choice)
                    .unwrap_or(ApprovalChoice::Deny);
                self.result = Some(ApprovalResult::Resolved {
                    interrupt_id: self.interrupt_id,
                    choice,
                });
                true
            }
        }
    }
}

/// Build the single scope-select page. Full variant offers all four
/// scopes; wrapper variant offers only the one-time approval.
fn scope_page(prompt: String, wrapper: bool) -> Page {
    let title = if wrapper {
        format!("Run `{prompt}`? (wrapper — can't be remembered)")
    } else {
        format!("Run `{prompt}`?")
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
    Page::select(title, options)
}

fn opt(id: &str, label: &str) -> DialogOption {
    DialogOption {
        id: id.to_string(),
        label: label.to_string(),
    }
}

/// Map the chosen option id to an [`ApprovalChoice`]. An unknown id (only
/// reachable via a free-text answer the host can't normally produce on a
/// scope page) is treated as a deny — the safe default.
fn answer_to_choice(answer: &Answer) -> ApprovalChoice {
    let id = match answer {
        Answer::Single { id } => id.as_str(),
        // A scope page is select-only; a Multi/Text answer shouldn't
        // arise, but if it does, deny rather than guess.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn full_variant_offers_four_scopes() {
        let d = ApprovalDialog::new(Uuid::new_v4(), "gh pr".into(), false, Duration::ZERO);
        // page 0, four options + the custom affordance.
        assert_eq!(d.state().pages()[0].options.len(), 4);
    }

    #[test]
    fn wrapper_variant_offers_only_once() {
        let d = ApprovalDialog::new(Uuid::new_v4(), "bash".into(), true, Duration::ZERO);
        assert_eq!(d.state().pages()[0].options.len(), 1);
        assert!(d.is_wrapper());
    }

    #[test]
    fn selecting_session_resolves_to_session_scope() {
        let iid = Uuid::new_v4();
        let mut d = ApprovalDialog::new(iid, "gh pr".into(), false, Duration::ZERO);
        // Move to the second option (session), enter to choose+submit.
        d.handle_key(press(KeyCode::Char('j')));
        assert!(d.handle_key(press(KeyCode::Enter)));
        match d.take_result() {
            Some(ApprovalResult::Resolved {
                interrupt_id,
                choice,
            }) => {
                assert_eq!(interrupt_id, iid);
                assert_eq!(choice, ApprovalChoice::Approve(Scope::Session));
            }
            None => panic!("expected a result"),
        }
    }

    #[test]
    fn esc_denies() {
        let iid = Uuid::new_v4();
        let mut d = ApprovalDialog::new(iid, "rm".into(), false, Duration::ZERO);
        assert!(d.handle_key(press(KeyCode::Esc)));
        match d.take_result() {
            Some(ApprovalResult::Resolved { choice, .. }) => {
                assert_eq!(choice, ApprovalChoice::Deny);
            }
            None => panic!("expected deny"),
        }
    }

    #[test]
    fn first_option_is_once() {
        let iid = Uuid::new_v4();
        let mut d = ApprovalDialog::new(iid, "ls".into(), false, Duration::ZERO);
        assert!(d.handle_key(press(KeyCode::Enter)));
        match d.take_result() {
            Some(ApprovalResult::Resolved { choice, .. }) => {
                assert_eq!(choice, ApprovalChoice::Approve(Scope::Once));
            }
            None => panic!("expected once"),
        }
    }
}
