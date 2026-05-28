//! Tool abstraction for cockpit.
//!
//! Why we wrap rig's `Tool`/`ToolDyn` rather than using them directly:
//! the §12 repair layer needs a seam between rig's JSON-deserialized
//! arguments and the typed dispatcher. We pin `type Args = Value` on
//! every tool — rig's `ToolDyn` just `serde_json::from_str`s into
//! `Args`, which is infallible for `Value` — so by the time `call()`
//! runs we have a `serde_json::Value` we can mutate in place via
//! [`crate::engine::repair`].
//!
//! Concrete tools implement [`Tool`]; the dispatcher holds a
//! `BTreeMap<String, Arc<dyn Tool>>`.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::message::ToolDefinition;

/// Why a tool call failed. Surfaced to the TUI so it can tell a bad
/// *call* (the model's fault) from a bad *outcome* (the tool's fault).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailKind {
    /// The model constructed the call badly — a missing / wrong-type
    /// required argument, or a value the tool can't satisfy (e.g. an
    /// `old_string` that isn't in the file) — and the §12 repair layer
    /// couldn't fix it. The model is at fault.
    Invocation,
    /// The tool ran but failed for an environmental reason: an I/O
    /// error, a non-zero command exit surfaced as an error, a lock
    /// conflict, etc.
    Execution,
}

/// Marker error a tool returns when the *arguments* were the problem
/// (see [`ToolFailKind::Invocation`]). The dispatcher downcasts to this
/// to classify the failure; build it with [`invalid_input`].
#[derive(Debug)]
pub struct InvalidToolInput(pub String);

impl std::fmt::Display for InvalidToolInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidToolInput {}

/// Build an [`InvalidToolInput`] error. Tools use this for missing /
/// wrong-type required args and for argument values that can't be
/// satisfied — anything that's the model's fault rather than the
/// environment's.
pub fn invalid_input(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(InvalidToolInput(msg.into()))
}

/// Classify a dispatch error: an [`InvalidToolInput`] anywhere in the
/// chain means the model built the call badly; everything else is an
/// execution failure.
pub fn classify_failure(err: &anyhow::Error) -> ToolFailKind {
    if err.downcast_ref::<InvalidToolInput>().is_some() {
        ToolFailKind::Invocation
    } else {
        ToolFailKind::Execution
    }
}

/// A locked-down tool whose argument type is always `serde_json::Value`.
///
/// Implementors get the args **after** §12 repair has run; the caller's
/// `ctx` is opaque and threaded for cross-cutting state (lock manager,
/// session reference, redaction table, etc.). The output is rendered to
/// a string for the model — JSON, markdown, raw text, whatever fits.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    /// One-sentence description per GOALS §10. Keep it under ~80 chars.
    fn description(&self) -> &str;

    /// JSON Schema for the arguments. Returning `Value::Null` means "no
    /// arguments." See plan.md §12 for the conventions the schema must
    /// follow for the repair catalog to fire.
    fn parameters(&self) -> Value;

    /// Run the tool. The args have already passed through §12 repair (or
    /// validate-clean) before this call; the implementor only needs to
    /// look up the fields it cares about.
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput>;
}

/// Tool output shape.
///
/// `content` is what the model sees on the next turn. `truncated` tells
/// the §10 spillover path whether to write a full version to disk.
///
/// `recovery` and `canonical_args` let a tool communicate that the call
/// it received was *recoverable* — it ran successfully, but only after
/// the tool normalized the args in a way the model should learn from.
/// The edit cascade (GOALS §13c) is the only v0 user: when an edit
/// matches at stage > 1, the tool sets `recovery = EditCascade { stage,
/// path: "old_string" }` and `canonical_args = <original args with
/// old_string replaced by the matched bytes>`. The dispatcher uses
/// these to persist the canonical form to the audit row's
/// `wire_input_json` and to rewrite the in-memory assistant message so
/// the next inference call carries canonical bytes.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    /// True when [`content`] is capped (per the §10 truncation marker).
    pub truncated: bool,
    /// Optional recovery annotation. `None` means the tool ran without
    /// any normalization. The dispatcher prefers this over any
    /// shape-repair recovery that fired earlier in the same call.
    pub recovery: Option<crate::engine::repair::Recovery>,
    /// Optional canonical args. When `Some`, the dispatcher uses this
    /// as `wire_input_json` for the audit row and as the rewritten
    /// arguments in the assistant message's `ToolCall` in history.
    pub canonical_args: Option<serde_json::Value>,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: false,
            recovery: None,
            canonical_args: None,
        }
    }

    pub fn truncated_text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: true,
            recovery: None,
            canonical_args: None,
        }
    }

    /// Attach a recovery annotation and the canonical arg form. See the
    /// struct docs for the contract.
    pub fn with_recovery(
        mut self,
        recovery: crate::engine::repair::Recovery,
        canonical_args: serde_json::Value,
    ) -> Self {
        self.recovery = Some(recovery);
        self.canonical_args = Some(canonical_args);
        self
    }
}

/// State threaded into every tool call.
///
/// Holding `Arc`s here means the dispatcher can clone-and-stash this
/// without copying the lock manager / session contents. Tools must not
/// hold references across `.await` points past the borrow this gives
/// them.
#[derive(Clone)]
pub struct ToolCtx {
    pub agent_id: String,
    pub locks: Arc<crate::locks::LockManager>,
    pub session: Arc<crate::session::Session>,
    pub cwd: std::path::PathBuf,
    /// The redaction chokepoint (GOALS §7). Tools that return strings
    /// destined for the model context don't have to call this
    /// themselves — `engine::agent::turn` scrubs every tool result
    /// before it lands in history. Threaded here too for tools that
    /// want to scrub *before* a long output is even allocated (e.g.
    /// `bash` capping output and only scrubbing what fits).
    pub redact: Arc<crate::redact::RedactionTable>,
    /// Interrupt wakeup hub (GOALS §3b). Structural tools that block on
    /// a human answer — today only `question` — raise an interrupt
    /// through this and await the resolution that arrives, out of band,
    /// on the daemon worker's `ResolveInterrupt` path. Threaded as an
    /// `Arc` so the same hub instance is shared with the worker.
    pub interrupts: Arc<crate::engine::interrupt::InterruptHub>,
    /// Per-turn cancellation token (user ctrl+c → `CancelTurn`). Long-
    /// running tools — today `bash` — race their subprocess against
    /// `cancel.cancelled()` and kill it (process group on Unix) when the
    /// user aborts the turn, so a runaway test run dies promptly instead
    /// of holding the turn open. Fresh per turn; cancelling it never
    /// affects a later turn.
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Project the `Tool` trait into a `ToolDefinition` rig understands.
pub fn definition_of(tool: &dyn Tool) -> ToolDefinition {
    ToolDefinition {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameters: tool.parameters(),
    }
}

/// Registry of tools available to an agent. Keyed by name for O(log n)
/// dispatch. Use [`ToolBox::with`] to add tools.
#[derive(Default, Clone)]
pub struct ToolBox {
    tools: std::collections::BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolBox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(tool.name().to_string(), tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| definition_of(&**t)).collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
