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
use serde_json::Value;

use crate::engine::message::ToolDefinition;

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
/// Currently a string; the `truncated` flag tells the §10 spillover
/// path whether to write a full version to disk. The struct exists so
/// we can grow other side-channels (e.g. structured citations from
/// `explore`) without breaking the `Tool` trait.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    /// True when [`content`] is capped (per the §10 truncation marker).
    pub truncated: bool,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: false,
        }
    }

    pub fn truncated_text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: true,
        }
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
