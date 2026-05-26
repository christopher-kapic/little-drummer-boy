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
//! - [`builtin`] — embedded `coder.md` + `orchestrator-build.md`.

pub mod agent;
pub mod builtin;
pub mod driver;
pub mod message;
pub mod model;
pub mod repair;
pub mod tool;

pub use agent::TurnEvent;
pub use driver::Driver;
