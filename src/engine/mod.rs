//! The agent loop — cockpit's conversation engine.
//!
//! Drives a manual rig conversation loop (the `manual_tool_calls.rs`
//! pattern, not `agent.prompt()`): we build [`rig::completion::CompletionRequest`]
//! values ourselves, dispatch tool calls through the [`tool`] layer,
//! and persist `original_input` / `wire_input` / `recovery` on each
//! tool-call row per GOALS §14.
//!
//! Layering:
//!
//! - [`message`] — type aliases over rig's `rig::message` so the rest
//!   of the codebase doesn't import rig directly.
//! - [`tool`] — our [`Tool`](tool::Tool) trait with `Args = Value`,
//!   giving §12 repair a place to live between deserialization and
//!   dispatch.
//! - [`model`] — provider enum (`OpenAi` v0; `Anthropic`, `OpenRouter`,
//!   `Ollama` queued).
//! - [`repair`] — the §12 catalog.
//! - [`agent`] — [`Agent`](agent::Agent) + [`turn`](agent::turn).
//! - [`driver`] — multi-agent stack with interactive primary handoff
//!   (GOALS §3b).
//! - [`builtin`] — embedded `coder.md` + `build.md`.

pub mod agent;
pub mod builtin;
pub mod compact;
pub mod deferred;
pub mod deleg_shrink;
pub mod docs_pipeline;
pub mod driver;
pub mod exec;
pub mod guidance_diff;
pub mod interrupt;
pub mod jobs;
pub mod message;
pub mod model;
pub mod prune;
pub mod repair;
pub mod retry;
pub mod tool;

pub use agent::TurnEvent;
pub use driver::Driver;

/// Whether the conversation is at a point where context-reduction
/// (`/prune` auto-fire, auto-`/compact`) may run without corrupting the
/// wire/user transcript split (`plan.md` T6.e). The boundary is safe
/// when no tool call is mid-flight, no interactive subagent is active,
/// and no user interaction is pending:
///
/// ```text
/// tool_call_in_flight.is_none()
///     && active_subagents.is_empty()
///     && !pending_user_interaction
/// ```
///
/// The driver evaluates this at the inference boundary (between tool
/// loops). Mid-tool-call or mid-subagent state must defer the reduction
/// and re-evaluate after the next significant state change, never prune
/// in place. A `false` here means "queue and retry."
pub fn is_at_safe_compaction_boundary(
    tool_call_in_flight: bool,
    active_subagents: bool,
    pending_user_interaction: bool,
) -> bool {
    !tool_call_in_flight && !active_subagents && !pending_user_interaction
}
