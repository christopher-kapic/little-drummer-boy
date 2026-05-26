//! Multi-agent conversation driver.
//!
//! Holds a stack of `AgentSession`s — one per active agent in the
//! current invocation tree. The user always talks to the agent on top
//! of the stack. On a `task` tool call, the driver pushes a new
//! subagent; when that subagent finishes (final text + no tool calls
//! and the parent has an outstanding task call), the driver pops it
//! and delivers the subagent's text as the parent's tool result.
//!
//! This is the v0 implementation of GOALS §3b's *interactive subagent*:
//! the primary-agent identity swaps every time the stack height
//! changes, and the user's messages route to whoever's on top.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::engine::agent::{Agent, TurnEvent, TurnOutcome, turn};
use crate::engine::message::Message;
use crate::redact::RedactionTable;
use crate::session::Session;

/// Maximum number of queued user messages to fold into a single
/// follow-up prompt. Generous because the worst case is a user
/// hammering Enter — concat-joining a dozen short messages is fine;
/// concat-joining a hundred would bloat the next inference. If we
/// hit this cap, extras stay in the channel for the *next* fold.
const MAX_FOLD: usize = 16;

/// One agent's slice of state on the driver stack.
pub struct AgentSession {
    pub agent: Arc<Agent>,
    pub history: Vec<Message>,
    /// When this session was pushed by a parent's `task` tool, the
    /// parent's outstanding tool-call id (we have to answer it when we
    /// pop). `None` for the root session.
    pub answering: Option<PendingTaskCall>,
}

#[derive(Debug, Clone)]
pub struct PendingTaskCall {
    pub call_id: String,
    pub function_call_id: Option<String>,
}

pub struct Driver {
    pub session: Arc<Session>,
    pub locks: Arc<crate::locks::LockManager>,
    pub redact: Arc<RedactionTable>,
    pub cwd: std::path::PathBuf,
    pub stack: Vec<AgentSession>,
}

impl Driver {
    pub fn new(
        session: Arc<Session>,
        locks: Arc<crate::locks::LockManager>,
        redact: Arc<RedactionTable>,
        cwd: std::path::PathBuf,
        root: Arc<Agent>,
    ) -> Self {
        Self {
            session,
            locks,
            redact,
            cwd,
            stack: vec![AgentSession {
                agent: root,
                history: Vec::new(),
                answering: None,
            }],
        }
    }

    /// Name of the agent currently holding the user's conversation.
    /// Used by the TUI for the active-agent slot.
    pub fn active_agent(&self) -> &str {
        self.stack.last().map(|a| a.agent.name.as_str()).unwrap_or("")
    }

    /// Long-running main loop: pulls user input from `input_rx` and
    /// drives it through the agent stack, **folding queued user
    /// messages** (GOALS §1c) at every inference boundary. The fold
    /// runs `try_recv` until the channel is empty, joins the
    /// collected texts with a blank line, and uses that as the next
    /// inference's user content.
    ///
    /// Per GOALS §1c, the queue is delivered at the *next inference
    /// call* — not the next user turn. Mid-tool-loop: the next
    /// tool-result → inference round-trip carries the queue alongside
    /// the tool result. End-of-turn: the queue is delivered as the
    /// first content of the next request. Empty queue: standard
    /// behavior.
    pub async fn run_main_loop(
        &mut self,
        mut input_rx: mpsc::Receiver<String>,
        tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        while let Some(text) = input_rx.recv().await {
            // Fold anything else that's already queued behind the
            // first message (rare but harmless).
            let mut batch = vec![text];
            drain_queue(&mut input_rx, &mut batch);
            let folded = self.redact.scrub(&fold_messages(batch));
            self.run_user_input(folded, &mut input_rx, tx).await?;
        }
        Ok(())
    }

    /// Drive one user message through the stack. Between inference
    /// rounds we drain any queued messages and fold them — see
    /// [`Self::run_main_loop`] for the contract.
    pub async fn run_user_input(
        &mut self,
        user_text: String,
        input_rx: &mut mpsc::Receiver<String>,
        tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        let mut next_prompt = Message::user(user_text);

        loop {
            let agent = {
                let top = self.stack.last().expect("stack never empty");
                top.agent.clone()
            };

            let outcome = {
                let top = self.stack.last_mut().expect("stack never empty");
                turn(
                    &agent,
                    &mut top.history,
                    next_prompt,
                    self.session.clone(),
                    self.locks.clone(),
                    self.redact.clone(),
                    self.cwd.clone(),
                    tx,
                )
                .await?
            };

            match outcome {
                TurnOutcome::Continue => {
                    let top = self.stack.last_mut().expect("stack never empty");
                    let last_tool_result = top
                        .history
                        .pop()
                        .expect("Continue with empty history is unreachable");

                    // Fold any queued user messages onto the upcoming
                    // inference. The tool result still has to be
                    // delivered, so push it back onto history and use
                    // the queued user content as the next prompt.
                    let mut queued: Vec<String> = Vec::new();
                    drain_queue(input_rx, &mut queued);
                    if queued.is_empty() {
                        next_prompt = last_tool_result;
                    } else {
                        top.history.push(last_tool_result);
                        next_prompt = Message::user(self.redact.scrub(&fold_messages(queued)));
                    }
                    continue;
                }
                TurnOutcome::Done => {
                    if self.stack.len() > 1 {
                        let child = self.stack.pop().unwrap();
                        let report = collect_final_text(&child.history);
                        let _ = tx
                            .send(TurnEvent::SubagentReport {
                                agent: child.agent.name.clone(),
                                report: report.clone(),
                            })
                            .await;
                        if let Some(pending) = child.answering {
                            // The task call's tool_result becomes the
                            // parent's next prompt. The parent's
                            // history already ends with the assistant
                            // turn that emitted the task call.
                            next_prompt = Message::tool_result_with_call_id(
                                pending.call_id,
                                pending.function_call_id,
                                report,
                            );
                            continue;
                        }
                    }
                    // Root agent is done with this user message. Before
                    // we wait for the next user input, check if more
                    // landed in the queue while we were busy — fold
                    // them and start a new run with the combined text.
                    let mut queued: Vec<String> = Vec::new();
                    drain_queue(input_rx, &mut queued);
                    if !queued.is_empty() {
                        next_prompt = Message::user(self.redact.scrub(&fold_messages(queued)));
                        continue;
                    }
                    return Ok(());
                }
                TurnOutcome::SpawnSubagent {
                    child_agent,
                    prompt: brief,
                    task_call_id,
                    task_function_call_id,
                } => {
                    let child = crate::engine::builtin::load(&child_agent, &self.spawn_args())?;
                    self.stack.push(AgentSession {
                        agent: Arc::new(child),
                        history: Vec::new(),
                        answering: Some(PendingTaskCall {
                            call_id: task_call_id,
                            function_call_id: task_function_call_id,
                        }),
                    });
                    next_prompt = Message::user(self.redact.scrub(&brief));
                    continue;
                }
                TurnOutcome::SpawnNoninteractive {
                    child_agent,
                    prompt: brief,
                    task_call_id,
                    task_function_call_id,
                } => {
                    // Emit a single ToolStart/ToolEnd pair so the
                    // user sees one row in the orchestrator's history
                    // — never a separate agent stream.
                    let args_json = serde_json::json!({
                        "agent": child_agent,
                        "prompt": brief.clone(),
                    });
                    let _ = tx
                        .send(TurnEvent::ToolStart {
                            agent: self.stack.last().unwrap().agent.name.clone(),
                            call_id: task_call_id.clone(),
                            tool: format!("task→{child_agent}"),
                            args: args_json,
                        })
                        .await;
                    let child = crate::engine::builtin::load(&child_agent, &self.spawn_args())?;
                    let report = match run_noninteractive(
                        child,
                        self.redact.scrub(&brief),
                        self.session.clone(),
                        self.locks.clone(),
                        self.redact.clone(),
                        self.cwd.clone(),
                    )
                    .await
                    {
                            Ok(text) => text,
                            Err(e) => format!("Error: {e:#}"),
                        };
                    let _ = tx
                        .send(TurnEvent::ToolEnd {
                            agent: self.stack.last().unwrap().agent.name.clone(),
                            call_id: task_call_id.clone(),
                            tool: format!("task→{child_agent}"),
                            output: report.clone(),
                            truncated: false,
                        })
                        .await;
                    // Deliver the result as the parent's next prompt.
                    next_prompt = Message::tool_result_with_call_id(
                        task_call_id,
                        task_function_call_id,
                        report,
                    );
                    continue;
                }
            }
        }
    }

    fn spawn_args(&self) -> crate::engine::builtin::SpawnArgs {
        crate::engine::builtin::SpawnArgs {
            model: self.stack[0].agent.model.clone(),
            params: self.stack[0].agent.params.clone(),
        }
    }
}

/// Drain queued user messages from the channel without blocking. Stops
/// at the [`MAX_FOLD`] cap; anything beyond stays for a later fold.
fn drain_queue(rx: &mut mpsc::Receiver<String>, into: &mut Vec<String>) {
    while into.len() < MAX_FOLD {
        match rx.try_recv() {
            Ok(s) => into.push(s),
            Err(_) => break,
        }
    }
}

/// Concatenate multiple user messages into a single inference payload
/// per GOALS §1c: blank-line separator, no special framing or
/// numbering. The user composed them as separate thoughts; the model
/// sees one coherent message.
fn fold_messages(messages: Vec<String>) -> String {
    messages.join("\n\n")
}

/// Run a child agent's loop to completion synchronously. Used for
/// noninteractive subagents — explore primarily. Drops the child's
/// per-turn events on the floor (the parent's history already has a
/// ToolStart/End representing this call); only the final text comes
/// back. Limited to `MAX_NONINTERACTIVE_TURNS` to bound runaway loops.
const MAX_NONINTERACTIVE_TURNS: usize = 12;

async fn run_noninteractive(
    child: Agent,
    brief: String,
    session: Arc<Session>,
    locks: Arc<crate::locks::LockManager>,
    redact: Arc<RedactionTable>,
    cwd: std::path::PathBuf,
) -> Result<String> {
    use crate::engine::agent::turn;

    // The child needs an event channel; we drain and discard.
    let (sink_tx, mut sink_rx) = mpsc::channel::<TurnEvent>(64);
    let drain = tokio::spawn(async move { while sink_rx.recv().await.is_some() {} });

    let agent = Arc::new(child);
    let mut history: Vec<Message> = Vec::new();
    let mut next_prompt = Message::user(brief);

    for _ in 0..MAX_NONINTERACTIVE_TURNS {
        let outcome = turn(
            &agent,
            &mut history,
            next_prompt,
            session.clone(),
            locks.clone(),
            redact.clone(),
            cwd.clone(),
            &sink_tx,
        )
        .await?;
        match outcome {
            TurnOutcome::Continue => {
                next_prompt = history
                    .pop()
                    .expect("Continue with empty history is unreachable");
            }
            TurnOutcome::Done => {
                drop(sink_tx);
                let _ = drain.await;
                return Ok(collect_final_text(&history));
            }
            TurnOutcome::SpawnSubagent { .. } | TurnOutcome::SpawnNoninteractive { .. } => {
                // explore is a leaf; this shouldn't happen, but if it
                // does we treat it as a turn boundary so the loop
                // doesn't spin.
                drop(sink_tx);
                let _ = drain.await;
                anyhow::bail!("noninteractive agent `{}` attempted to delegate via task", agent.name);
            }
        }
    }
    drop(sink_tx);
    let _ = drain.await;
    anyhow::bail!("noninteractive agent `{}` exceeded {MAX_NONINTERACTIVE_TURNS} turns", agent.name)
}

fn collect_final_text(history: &[Message]) -> String {
    // The last assistant message in the history is the subagent's
    // final text. Walk back to find it.
    for msg in history.iter().rev() {
        if let Message::Assistant { content, .. } = msg {
            let text = crate::engine::message::extract_text(content);
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    String::new()
}
