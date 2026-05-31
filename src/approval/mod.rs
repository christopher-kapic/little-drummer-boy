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
use crate::approval::store::{GrantStore, LoopVerdict, Scope};
use crate::daemon::proto::{
    CharSpan, CommandDetail, InterruptOption, InterruptQuestion, InterruptQuestionSet,
    ResolveResponse,
};
use crate::engine::interrupt::InterruptHub;
use crate::tui::dialog::approval::{
    ID_GLOBAL, ID_LOOP_ACCEPT_ONCE, ID_LOOP_ACCEPT_PROJECT, ID_LOOP_ACCEPT_SESSION,
    ID_LOOP_REJECT_ONCE, ID_LOOP_REJECT_PROJECT, ID_LOOP_REJECT_SESSION, ID_ONCE, ID_PROJECT,
    ID_SESSION,
};

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

/// The loop-guard's verdict on a back-to-back identical tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatDecision {
    /// Run the repeated call (one-off accept or an always-accept rule).
    Accept,
    /// Block the repeated call; the dispatcher returns the guidance error
    /// as the tool result so the model changes course.
    Reject,
}

impl RepeatDecision {
    pub fn is_accept(&self) -> bool {
        matches!(self, RepeatDecision::Accept)
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

        // Pre-compute which constituents will actually prompt (a wrapper, or
        // one not already granted). `step_count` (M) is that count; each
        // prompting constituent's 1-based position within the sequence is
        // its `step` (N). Already-granted constituents are allowed silently
        // and don't advance the step counter — matching the spec's
        // "M = constituents that actually trigger a prompt".
        let step_count = simple_commands
            .iter()
            .filter(|info| self.will_prompt(info))
            .count() as u32;

        // Track the broadest scope we settled on, for the caller's info.
        // A chain is only as "remembered" as its narrowest decision; we
        // report `Once` if any command was only approved once.
        let mut widest = Scope::Global;
        let mut step: u32 = 0;
        for info in &simple_commands {
            let prompts = self.will_prompt(info);
            if prompts {
                step += 1;
            }
            let decision = self.approve_one(info, command, step, step_count).await?;
            match decision {
                Decision::Deny => return Ok(Decision::Deny),
                Decision::Allow { scope } => {
                    widest = narrowest(widest, scope);
                }
            }
        }
        Ok(Decision::Allow { scope: widest })
    }

    /// Whether this constituent will raise a prompt rather than being
    /// allowed silently: a wrapper (never persistable) always prompts;
    /// otherwise it prompts only when not already granted.
    fn will_prompt(&self, info: &SimpleCommandInfo) -> bool {
        info.wrapper || !self.store.is_command_granted(&info.key)
    }

    /// Decide one simple command: granted → allow; else prompt. `step` /
    /// `step_count` describe this constituent's position among the
    /// prompting constituents (for the dialog's `step N of M`); they are
    /// only meaningful when this constituent prompts.
    async fn approve_one(
        &self,
        info: &SimpleCommandInfo,
        full_command: &str,
        step: u32,
        step_count: u32,
    ) -> Result<Decision> {
        if !info.wrapper && self.store.is_command_granted(&info.key) {
            // Already remembered at some applicable scope.
            return Ok(Decision::Allow {
                scope: Scope::Session,
            });
        }
        // The heading still shows the approval key — the exact thing a grant
        // would cover (`gh pr`, `cargo build`, `ls`) — so a "remember" choice
        // records the key, not the arg-laden command line. The full command
        // rides alongside as presentational detail (`CommandDetail`).
        let label = info.key.as_storage_str();
        let detail = command_detail(info, full_command, step, step_count);
        let choice = self.prompt(&label, info.wrapper, detail).await?;
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

    /// Escalate a single non-command tool call to the user (the
    /// command-safety gate's `auto` mode for `webfetch`/`mcp_invoke`, and
    /// its fail-closed path). Unlike [`Self::approve_command`] there is no
    /// command line to classify and no persistable key — the call's
    /// arguments vary per invocation — so this prompts **once-only** (no
    /// "remember" scopes), mirroring the wrapper-command prompt shape.
    /// `label` is the human description shown in the prompt (e.g.
    /// `` `webfetch` `` plus the URL). Returns `Allow { Once }` on approval,
    /// `Deny` on dismissal.
    pub async fn approve_tool_call(&self, label: &str) -> Result<Decision> {
        // `wrapper = true` makes the prompt offer only "Yes, once" — the
        // right shape for a non-persistable per-call approval. Nothing is
        // recorded; a later identical call prompts again.
        let choice = self.prompt(label, true, None).await?;
        match choice {
            ApprovalChoice::Deny => Ok(Decision::Deny),
            ApprovalChoice::Approve(_) => Ok(Decision::Allow { scope: Scope::Once }),
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
        let choice = self
            .prompt(&path.display().to_string(), false, None)
            .await?;
        match choice {
            ApprovalChoice::Deny => Ok(Decision::Deny),
            ApprovalChoice::Approve(Scope::Once) => Ok(Decision::Allow { scope: Scope::Once }),
            ApprovalChoice::Approve(scope) => {
                self.store.record_path(path, scope)?;
                Ok(Decision::Allow { scope })
            }
        }
    }

    /// Decide a back-to-back identical tool call (the loop guard, GOALS
    /// §1/§12). The dispatcher calls this only once the same `(tool,
    /// wire_input)` signature has repeated to the configured threshold.
    ///
    /// Resolution order:
    /// 1. An always-* rule for this exact signature (session > project >
    ///    global, per [`GrantStore::loop_rule`]) is honored without
    ///    prompting.
    /// 2. Headless (no interactive client that can answer): **reject** —
    ///    never block waiting for input, and never silently re-run a
    ///    likely loop.
    /// 3. Otherwise raise the six-option approval prompt (reusing the
    ///    `question`-tool interrupt path) and act on the answer, recording
    ///    a session/project rule when the user chose an "always" option.
    ///
    /// `tool` + `wire_input` are the canonical post-repair call; the
    /// signature is derived from them so a rule keys on the exact call,
    /// never the tool name alone.
    pub async fn approve_repeat(
        &self,
        tool: &str,
        wire_input: &serde_json::Value,
        interactive: bool,
    ) -> Result<RepeatDecision> {
        let signature = GrantStore::loop_signature(tool, wire_input);

        // 1. Standing rule wins, at any scope.
        if let Some(verdict) = self.store.loop_rule(&signature) {
            return Ok(match verdict {
                LoopVerdict::Accept => RepeatDecision::Accept,
                LoopVerdict::Reject => RepeatDecision::Reject,
            });
        }

        // 2. No human to ask → reject the repeat (the guidance error lets
        //    the model change course; re-running would bleed the window).
        if !interactive {
            return Ok(RepeatDecision::Reject);
        }

        // 3. Prompt with the six choices and act on the answer.
        let choice = self.prompt_repeat(tool).await?;
        match choice {
            RepeatChoice::AcceptOnce => Ok(RepeatDecision::Accept),
            RepeatChoice::RejectOnce => Ok(RepeatDecision::Reject),
            RepeatChoice::Always { verdict, scope } => {
                // Record BEFORE returning, mirroring the command/path
                // approval contract. A record failure (e.g. Project scope
                // with no git root) must not strand the call: fall back to
                // applying the verdict this once and surface the error in
                // the log rather than aborting the turn.
                if let Err(e) = self.store.record_loop_rule(&signature, verdict, scope) {
                    tracing::warn!(error = %e, tool, ?scope, "recording loop-guard rule failed; applying once");
                }
                Ok(match verdict {
                    LoopVerdict::Accept => RepeatDecision::Accept,
                    LoopVerdict::Reject => RepeatDecision::Reject,
                })
            }
        }
    }

    /// Raise the loop-guard approval prompt (six options) and block until
    /// the user answers, reusing the `question`-tool interrupt path
    /// verbatim. A dismissal (Esc/cancel) reads as reject-once — the safe
    /// default for a likely loop.
    async fn prompt_repeat(&self, tool: &str) -> Result<RepeatChoice> {
        let question = repeat_question(tool);
        let set = InterruptQuestionSet {
            questions: vec![question],
        };
        let description = format!("Repeated `{tool}` call — likely a loop. Allow it?");

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
        Ok(response_to_repeat_choice(&response))
    }

    /// Raise an approval interrupt and block until the user answers,
    /// reusing the `question`-tool interrupt path verbatim. Returns the
    /// chosen scope, or `Deny` on dismissal. `detail` carries the optional
    /// bash command-detail block (the full verbatim command + highlight +
    /// step N/M); `None` for path approvals.
    async fn prompt(
        &self,
        label: &str,
        wrapper: bool,
        detail: Option<CommandDetail>,
    ) -> Result<ApprovalChoice> {
        let description = prompt_description(label, wrapper, detail.as_ref());
        let question = scope_question(label, wrapper, detail);
        let set = InterruptQuestionSet {
            questions: vec![question],
        };

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

/// The user's choice on a loop-guard prompt. `Always` carries the verdict
/// and the scope to persist it at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepeatChoice {
    AcceptOnce,
    RejectOnce,
    Always { verdict: LoopVerdict, scope: Scope },
}

/// Build the six-option loop-guard question. The options ride through the
/// generic interrupt; the answering dialog renders them with no
/// special-casing, exactly like a `question`-tool prompt.
fn repeat_question(tool: &str) -> InterruptQuestion {
    InterruptQuestion::Single {
        prompt: format!("`{tool}` repeated the previous call exactly — likely a loop. Run it?"),
        options: vec![
            opt(ID_LOOP_ACCEPT_ONCE, "Accept (once)"),
            opt(ID_LOOP_REJECT_ONCE, "Reject (once)"),
            opt(ID_LOOP_ACCEPT_SESSION, "Always accept for this session"),
            opt(ID_LOOP_REJECT_SESSION, "Always reject for this session"),
            opt(ID_LOOP_ACCEPT_PROJECT, "Always accept for this project"),
            opt(ID_LOOP_REJECT_PROJECT, "Always reject for this project"),
        ],
        // Fixed choices; no free-text.
        allow_freetext: false,
        // The loop-guard prompt carries no bash command-detail block.
        command_detail: None,
    }
}

/// Map a resolved interrupt response back to a loop-guard choice. An
/// unknown id, a non-`Single` response, or a `Cancel` reads as
/// reject-once — the safe default for a likely loop.
fn response_to_repeat_choice(response: &ResolveResponse) -> RepeatChoice {
    let id = match response {
        ResolveResponse::Single { selected_id } => selected_id.as_str(),
        ResolveResponse::Batch { responses } => match responses.first() {
            Some(ResolveResponse::Single { selected_id }) => selected_id.as_str(),
            _ => return RepeatChoice::RejectOnce,
        },
        _ => return RepeatChoice::RejectOnce,
    };
    match id {
        ID_LOOP_ACCEPT_ONCE => RepeatChoice::AcceptOnce,
        ID_LOOP_REJECT_ONCE => RepeatChoice::RejectOnce,
        ID_LOOP_ACCEPT_SESSION => RepeatChoice::Always {
            verdict: LoopVerdict::Accept,
            scope: Scope::Session,
        },
        ID_LOOP_REJECT_SESSION => RepeatChoice::Always {
            verdict: LoopVerdict::Reject,
            scope: Scope::Session,
        },
        ID_LOOP_ACCEPT_PROJECT => RepeatChoice::Always {
            verdict: LoopVerdict::Accept,
            scope: Scope::Project,
        },
        ID_LOOP_REJECT_PROJECT => RepeatChoice::Always {
            verdict: LoopVerdict::Reject,
            scope: Scope::Project,
        },
        _ => RepeatChoice::RejectOnce,
    }
}

/// Build the single scope-select question. Full variant offers all four
/// scopes; wrapper variant offers only one-time approval (the dialog
/// shows the "can't be remembered" note). Option ids are shared with the
/// TUI dialog so the resolution maps back cleanly.
fn scope_question(label: &str, wrapper: bool, detail: Option<CommandDetail>) -> InterruptQuestion {
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
        command_detail: detail,
    }
}

/// Build the presentational command-detail block for one constituent.
/// `step`/`step_count` give the `step N of M` indicator; the highlight span
/// is omitted for a single-prompt command (no step indicator) so the dialog
/// shows the full command without an inline highlight. The span is also
/// dropped if it doesn't lie within the command's char length (defensive:
/// a stale/degenerate span must never produce a wrong highlight — the
/// silent-corruption hazard the project forbids).
fn command_detail(
    info: &SimpleCommandInfo,
    full_command: &str,
    step: u32,
    step_count: u32,
) -> Option<CommandDetail> {
    // Only highlight when there's more than one prompting constituent;
    // a lone prompt shows the full command with no step/highlight.
    let highlight = if step_count > 1 {
        info.span.and_then(|s| {
            let char_len = full_command.chars().count();
            if s.start <= s.end && s.end <= char_len {
                Some(CharSpan {
                    start: s.start as u32,
                    end: s.end as u32,
                })
            } else {
                None
            }
        })
    } else {
        None
    };
    Some(CommandDetail {
        full_command: full_command.to_string(),
        highlight,
        step,
        step_count,
    })
}

fn prompt_description(label: &str, wrapper: bool, detail: Option<&CommandDetail>) -> String {
    // Include the full command (and step indicator) so headless / log
    // surfaces aren't worse off than the TUI.
    let suffix = match detail {
        Some(cd) if cd.step_count > 1 => {
            format!(
                " — `{}` (step {} of {})",
                cd.full_command, cd.step, cd.step_count
            )
        }
        Some(cd) => format!(" — `{}`", cd.full_command),
        None => String::new(),
    };
    if wrapper {
        format!("Approve wrapper `{label}` (once only){suffix}?")
    } else {
        format!("Approve `{label}`{suffix}?")
    }
}

fn opt(id: &str, label: &str) -> InterruptOption {
    InterruptOption {
        id: id.to_string(),
        label: label.to_string(),
        description: None,
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
            span: None,
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

    /// Pull the command-detail off the open interrupt with `iid`.
    fn open_command_detail(
        db: &crate::db::Db,
        sid: uuid::Uuid,
        iid: uuid::Uuid,
    ) -> Option<CommandDetail> {
        let open = db.list_open_interrupts(sid).unwrap();
        let row = open.iter().find(|r| r.interrupt_id == iid)?;
        let set = row.questions.as_ref()?;
        match set.questions.first()? {
            InterruptQuestion::Single { command_detail, .. } => command_detail.clone(),
            _ => None,
        }
    }

    #[tokio::test]
    async fn compound_prompts_carry_step_count_and_full_command() {
        // Neither constituent granted: two prompts, each with the full
        // command verbatim, `step 1 of 2` / `step 2 of 2`, and the active
        // constituent's highlight span. A "session" grant on the second
        // records the KEY (`cargo build`), not the full command.
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        let db = approver.db.clone();
        let sid = approver.session_id;
        let hub = approver.interrupts.clone();
        let cmd = "git push origin main && cargo build";

        let resolver = tokio::spawn(async move {
            // First prompt: step 1 of 2, highlight over "git push origin main".
            let iid = loop {
                let open = db.list_open_interrupts(sid).unwrap();
                if let Some(row) = open.first() {
                    break row.interrupt_id;
                }
                tokio::task::yield_now().await;
            };
            let cd = open_command_detail(&db, sid, iid).expect("first prompt has command_detail");
            assert_eq!(cd.full_command, cmd);
            assert_eq!((cd.step, cd.step_count), (1, 2));
            let h = cd.highlight.expect("step 1 highlighted");
            let slice: String = cmd
                .chars()
                .skip(h.start as usize)
                .take((h.end - h.start) as usize)
                .collect();
            assert_eq!(slice, "git push origin main");
            // Approve once.
            assert!(hub.resolve(
                iid,
                ResolveResponse::Single {
                    selected_id: ID_ONCE.into(),
                }
            ));

            // Second prompt: step 2 of 2, highlight over "cargo build".
            let iid2 = loop {
                let open = db.list_open_interrupts(sid).unwrap();
                // Wait for a *different* (the second) interrupt.
                if let Some(row) = open.iter().find(|r| r.interrupt_id != iid) {
                    break row.interrupt_id;
                }
                tokio::task::yield_now().await;
            };
            let cd2 =
                open_command_detail(&db, sid, iid2).expect("second prompt has command_detail");
            assert_eq!(cd2.full_command, cmd);
            assert_eq!((cd2.step, cd2.step_count), (2, 2));
            let h2 = cd2.highlight.expect("step 2 highlighted");
            let slice2: String = cmd
                .chars()
                .skip(h2.start as usize)
                .take((h2.end - h2.start) as usize)
                .collect();
            assert_eq!(slice2, "cargo build");
            // Remember for the session.
            assert!(hub.resolve(
                iid2,
                ResolveResponse::Single {
                    selected_id: ID_SESSION.into(),
                }
            ));
        });

        let decision = approver.approve_command(cmd).await.unwrap();
        resolver.await.unwrap();
        assert!(decision.is_allowed());

        // The grant recorded the KEY for the remembered constituent only.
        let cargo_key = ApprovalKey {
            program: "cargo".into(),
            subcommand: Some("build".into()),
        };
        assert!(
            approver.store.is_command_granted(&cargo_key),
            "remembered `cargo build` key"
        );
        // The once-approved git push was NOT remembered.
        let git_key = ApprovalKey {
            program: "git".into(),
            subcommand: Some("push".into()),
        };
        assert!(
            !approver.store.is_command_granted(&git_key),
            "`git push` was once-only, not stored"
        );
    }

    #[tokio::test]
    async fn granted_first_half_prompts_once_as_step_1_of_1() {
        // `git push origin main && cargo build` with `git push` already
        // granted: exactly one prompt, labelled step 1 of 1 (M counts only
        // prompting constituents), full command shown, no highlight.
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        // Pre-grant `git push`.
        let git_info = SimpleCommandInfo {
            program: "git".into(),
            subcommand: Some("push".into()),
            key: ApprovalKey {
                program: "git".into(),
                subcommand: Some("push".into()),
            },
            wrapper: false,
            span: None,
        };
        approver
            .store
            .record_command(&git_info, Scope::Session)
            .unwrap();

        let db = approver.db.clone();
        let sid = approver.session_id;
        let hub = approver.interrupts.clone();
        let cmd = "git push origin main && cargo build";
        let resolver = tokio::spawn(async move {
            let iid = loop {
                let open = db.list_open_interrupts(sid).unwrap();
                if let Some(row) = open.first() {
                    break row.interrupt_id;
                }
                tokio::task::yield_now().await;
            };
            let cd = open_command_detail(&db, sid, iid).expect("command_detail present");
            assert_eq!(cd.full_command, cmd);
            assert_eq!(
                (cd.step, cd.step_count),
                (1, 1),
                "M counts only prompting steps"
            );
            // Single prompting step → no highlight.
            assert!(cd.highlight.is_none(), "lone prompt is not highlighted");
            assert!(hub.resolve(
                iid,
                ResolveResponse::Single {
                    selected_id: ID_ONCE.into(),
                }
            ));
        });
        let decision = approver.approve_command(cmd).await.unwrap();
        resolver.await.unwrap();
        assert!(decision.is_allowed());
    }

    #[tokio::test]
    async fn wrapper_prompt_shows_full_command_once_only() {
        // A wrapper (`bash -c …`) offers only "Yes, once" and still shows
        // the full command in the detail block.
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        let db = approver.db.clone();
        let sid = approver.session_id;
        let hub = approver.interrupts.clone();
        let cmd = "bash -c 'echo hi'";
        let resolver = tokio::spawn(async move {
            let iid = loop {
                let open = db.list_open_interrupts(sid).unwrap();
                if let Some(row) = open.first() {
                    break row.interrupt_id;
                }
                tokio::task::yield_now().await;
            };
            // Wrapper → single option, full command in the detail block.
            let open = db.list_open_interrupts(sid).unwrap();
            let set = open[0].questions.as_ref().unwrap();
            match set.questions.first().unwrap() {
                InterruptQuestion::Single {
                    options,
                    command_detail,
                    ..
                } => {
                    assert_eq!(options.len(), 1, "wrapper offers only `Yes, once`");
                    assert_eq!(
                        command_detail.as_ref().unwrap().full_command,
                        cmd,
                        "wrapper shows the full command"
                    );
                }
                _ => panic!("expected Single"),
            }
            assert!(hub.resolve(
                iid,
                ResolveResponse::Single {
                    selected_id: ID_ONCE.into(),
                }
            ));
        });
        let decision = approver.approve_command(cmd).await.unwrap();
        resolver.await.unwrap();
        assert_eq!(decision, Decision::Allow { scope: Scope::Once });
    }

    #[test]
    fn prompt_description_includes_full_command() {
        // The persisted/headless description carries the full command + step.
        let detail = CommandDetail {
            full_command: "git push && cargo build".into(),
            highlight: None,
            step: 2,
            step_count: 2,
        };
        let desc = prompt_description("cargo build", false, Some(&detail));
        assert!(
            desc.contains("git push && cargo build"),
            "full command in desc"
        );
        assert!(desc.contains("step 2 of 2"), "step indicator in desc");
        // Single-step: no step indicator, but still the full command.
        let lone = CommandDetail {
            full_command: "cd /tmp".into(),
            highlight: None,
            step: 1,
            step_count: 1,
        };
        let desc = prompt_description("cd", false, Some(&lone));
        assert!(desc.contains("cd /tmp"));
        assert!(!desc.contains("step "));
    }

    #[test]
    fn command_detail_drops_out_of_range_span() {
        // A span beyond the command length is dropped, never used to slice
        // (defensive: no wrong highlight).
        let info = SimpleCommandInfo {
            program: "x".into(),
            subcommand: None,
            key: ApprovalKey {
                program: "x".into(),
                subcommand: None,
            },
            wrapper: false,
            span: Some(crate::approval::classify::CharSpan { start: 2, end: 999 }),
        };
        let cd = command_detail(&info, "x && y", 2, 2).unwrap();
        assert!(cd.highlight.is_none(), "out-of-range span dropped");
        assert_eq!((cd.step, cd.step_count), (2, 2));
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

    // ---- loop guard ------------------------------------------------------

    #[test]
    fn repeat_response_mapping_round_trips() {
        use crate::tui::dialog::approval::{
            ID_LOOP_ACCEPT_ONCE, ID_LOOP_ACCEPT_PROJECT, ID_LOOP_ACCEPT_SESSION,
            ID_LOOP_REJECT_ONCE, ID_LOOP_REJECT_PROJECT, ID_LOOP_REJECT_SESSION,
        };
        let single = |id: &str| ResolveResponse::Single {
            selected_id: id.into(),
        };
        assert_eq!(
            response_to_repeat_choice(&single(ID_LOOP_ACCEPT_ONCE)),
            RepeatChoice::AcceptOnce
        );
        assert_eq!(
            response_to_repeat_choice(&single(ID_LOOP_REJECT_ONCE)),
            RepeatChoice::RejectOnce
        );
        assert_eq!(
            response_to_repeat_choice(&single(ID_LOOP_ACCEPT_SESSION)),
            RepeatChoice::Always {
                verdict: LoopVerdict::Accept,
                scope: Scope::Session
            }
        );
        assert_eq!(
            response_to_repeat_choice(&single(ID_LOOP_REJECT_SESSION)),
            RepeatChoice::Always {
                verdict: LoopVerdict::Reject,
                scope: Scope::Session
            }
        );
        assert_eq!(
            response_to_repeat_choice(&single(ID_LOOP_ACCEPT_PROJECT)),
            RepeatChoice::Always {
                verdict: LoopVerdict::Accept,
                scope: Scope::Project
            }
        );
        assert_eq!(
            response_to_repeat_choice(&single(ID_LOOP_REJECT_PROJECT)),
            RepeatChoice::Always {
                verdict: LoopVerdict::Reject,
                scope: Scope::Project
            }
        );
        // A dismissal reads as reject-once (safe default for a loop).
        assert_eq!(
            response_to_repeat_choice(&ResolveResponse::Cancel),
            RepeatChoice::RejectOnce
        );
    }

    #[tokio::test]
    async fn headless_repeat_with_no_rule_auto_rejects() {
        // No interactive client + no standing rule → reject without ever
        // raising a prompt (a detached hub would block forever if it did).
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        let decision = approver
            .approve_repeat("read", &serde_json::json!({"path": "x"}), false)
            .await
            .unwrap();
        assert_eq!(decision, RepeatDecision::Reject);
    }

    #[tokio::test]
    async fn headless_repeat_honors_always_accept_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        let input = serde_json::json!({"path": "x"});
        let sig = GrantStore::loop_signature("read", &input);
        approver
            .store
            .record_loop_rule(&sig, LoopVerdict::Accept, Scope::Session)
            .unwrap();
        // Headless, but a session always-accept rule applies → accept.
        let decision = approver
            .approve_repeat("read", &input, false)
            .await
            .unwrap();
        assert_eq!(decision, RepeatDecision::Accept);
    }

    #[tokio::test]
    async fn headless_repeat_honors_always_reject_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let (approver, _) = approver(tmp.path());
        let input = serde_json::json!({"path": "y"});
        let sig = GrantStore::loop_signature("bash", &input);
        approver
            .store
            .record_loop_rule(&sig, LoopVerdict::Reject, Scope::Session)
            .unwrap();
        let decision = approver
            .approve_repeat("bash", &input, false)
            .await
            .unwrap();
        assert_eq!(decision, RepeatDecision::Reject);
    }

    #[tokio::test]
    async fn interactive_repeat_accept_once_runs_but_records_no_rule() {
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
            assert!(hub.resolve(
                iid,
                ResolveResponse::Single {
                    selected_id: crate::tui::dialog::approval::ID_LOOP_ACCEPT_ONCE.into(),
                }
            ));
        });
        let input = serde_json::json!({"path": "z"});
        let decision = approver.approve_repeat("read", &input, true).await.unwrap();
        resolver.await.unwrap();
        assert_eq!(decision, RepeatDecision::Accept);
        // Accept-once records no rule: a fresh query still has none.
        let sig = GrantStore::loop_signature("read", &input);
        assert!(approver.store.loop_rule(&sig).is_none());
    }

    #[tokio::test]
    async fn interactive_repeat_always_reject_session_records_rule() {
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
            assert!(hub.resolve(
                iid,
                ResolveResponse::Single {
                    selected_id: crate::tui::dialog::approval::ID_LOOP_REJECT_SESSION.into(),
                }
            ));
        });
        let input = serde_json::json!({"command": "spin"});
        let decision = approver.approve_repeat("bash", &input, true).await.unwrap();
        resolver.await.unwrap();
        assert_eq!(decision, RepeatDecision::Reject);
        // The always-reject-session rule was persisted, so a later
        // (even headless) repeat of the exact signature auto-rejects with
        // no prompt.
        let sig = GrantStore::loop_signature("bash", &input);
        assert_eq!(approver.store.loop_rule(&sig), Some(LoopVerdict::Reject));
        let again = approver
            .approve_repeat("bash", &input, false)
            .await
            .unwrap();
        assert_eq!(again, RepeatDecision::Reject);
    }
}
