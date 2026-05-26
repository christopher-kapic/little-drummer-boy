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
use crate::session::Session;

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
    pub cwd: std::path::PathBuf,
    pub stack: Vec<AgentSession>,
}

impl Driver {
    pub fn new(
        session: Arc<Session>,
        locks: Arc<crate::locks::LockManager>,
        cwd: std::path::PathBuf,
        root: Arc<Agent>,
    ) -> Self {
        Self {
            session,
            locks,
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

    /// Drive one user message through the stack. Runs as many model
    /// turns as needed; suspends only when (a) the active agent's turn
    /// ends with prose and no tool calls (waiting for the next user
    /// message), or (b) an error propagates.
    ///
    /// The `tx` channel emits granular [`TurnEvent`]s; the caller
    /// (TUI) drains it for display.
    ///
    /// Loop invariant: `next_prompt` is what rig's `agent.completion(
    /// prompt, history)` should carry as its `prompt`. Per the
    /// `manual_tool_calls.rs` example, after a `Continue` outcome we
    /// pop the last message from history (the latest tool result) so
    /// the *new* request's prompt is the tool result and history ends
    /// at the assistant turn that called the tool. This avoids
    /// duplicating messages between `prompt` and `history`.
    pub async fn run_user_input(
        &mut self,
        user_text: String,
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
                    self.cwd.clone(),
                    tx,
                )
                .await?
            };

            match outcome {
                TurnOutcome::Continue => {
                    // turn() pushed prompt + assistant + N tool results;
                    // the next inference call wants the latest result as
                    // its `prompt`, with history ending at the assistant
                    // turn. Pop the last from history.
                    let top = self.stack.last_mut().expect("stack never empty");
                    next_prompt = top
                        .history
                        .pop()
                        .expect("Continue with empty history is unreachable");
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
                    next_prompt = Message::user(brief);
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
