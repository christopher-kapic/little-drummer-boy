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
    /// This is the **normal** `llm_mode` form (terse, the token-economy
    /// budget the CI check enforces).
    fn description(&self) -> &str;

    /// The **defensive** `llm_mode` description: explicit, steering prose
    /// for the weak-model target (`prompts/llm-modes-defensive-normal.md`).
    /// `None` (the default) means "no defensive variant — fall back to the
    /// terse [`Self::description`]." Every *built-in* tool overrides this so
    /// the full surface is covered (a registry-driven test enforces it);
    /// the only `None`-keepers are user-config-driven tools (custom-bash),
    /// whose author owns their wording.
    fn defensive_description(&self) -> Option<String> {
        None
    }

    /// JSON Schema for the arguments. Returning `Value::Null` means "no
    /// arguments." See plan.md §12 for the conventions the schema must
    /// follow for the repair catalog to fire. This is the **normal**
    /// `llm_mode` form (noun-phrase parameter descriptions).
    fn parameters(&self) -> Value;

    /// The **defensive** `llm_mode` parameter schema: same structure +
    /// required set as [`Self::parameters`], with explicit steering
    /// parameter descriptions. `None` (the default) reuses
    /// [`Self::parameters`]. Tool *grants* never vary by mode — only how
    /// the schema's descriptions read — so the shape here must match.
    fn defensive_parameters(&self) -> Option<Value> {
        None
    }

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
    /// Command/path approval driver (sandboxing part 2). The `bash` tool
    /// consults it for the run-fail-escalate flow (broadened re-run on a
    /// non-zero sandboxed exit), and the native file/intel tools consult
    /// it via [`crate::tools::sandbox::check_native_access`] to escalate
    /// an out-of-boundary path access. `None` on paths with no client
    /// fan-out (seed-tool re-execution, tool tests): a missing approver
    /// skips the prompt — it never silently denies. Shared `Arc` so one
    /// approver instance backs the whole delegation tree.
    pub approver: Option<Arc<crate::approval::Approver>>,
    /// The current frame's deferred-log buffer (`plan.md §3d`). A subagent's
    /// `defer_to_orchestrator` tool appends out-of-scope asks here; the
    /// driver drains it when the frame pops and folds it into the report the
    /// parent ingests. `Default` (empty) for the root frame and for contexts
    /// with no subagent (tests, seed-tool re-exec) — defer there is a no-op
    /// drain nobody reads.
    pub deferred_log: crate::engine::deferred::DeferredLog,
    /// The current frame's seed collector (GOALS §3c). A re-queryable
    /// read-only noninteractive subagent's `seed` tool appends `{tool, args}`
    /// entries here; the driver drains them on return and injects them into
    /// the caller's transcript. `Default` (empty) for the root frame, the
    /// interactive path, and contexts with no subagent (tests, seed-tool
    /// re-exec) — `seed` there is a no-op drain nobody reads.
    pub seeds: crate::engine::seed_collector::SeedCollector,
}

/// Project the `Tool` trait into a `ToolDefinition` rig understands.
///
/// This is the **single** place the `llm_mode` description-verbosity axis
/// is applied (`prompts/llm-modes-defensive-normal.md`): in
/// [`LlmMode::Defensive`] we render each tool's [`Tool::defensive_description`]
/// / [`Tool::defensive_parameters`] when present, falling back to the terse
/// [`Tool::description`] / [`Tool::parameters`] otherwise; in
/// [`LlmMode::Normal`] we always render the terse form. The switch lives
/// here and nowhere else — no per-tool conditionals at call sites.
pub fn definition_of(tool: &dyn Tool, mode: crate::config::extended::LlmMode) -> ToolDefinition {
    use crate::config::extended::LlmMode;
    let (description, parameters) = match mode {
        LlmMode::Defensive => (
            tool.defensive_description()
                .unwrap_or_else(|| tool.description().to_string()),
            tool.defensive_parameters()
                .unwrap_or_else(|| tool.parameters()),
        ),
        LlmMode::Normal => (tool.description().to_string(), tool.parameters()),
    };
    ToolDefinition {
        name: tool.name().to_string(),
        description,
        parameters,
    }
}

/// Behavioral capabilities gated on the [`LlmMode`] axis.
///
/// [`definition_of`] above is the *description-verbosity* seam — it changes
/// how a tool's schema reads, never what the engine will accept. This is the
/// separate **behavioral** seam: a real capability check the engine consults
/// before *acting*, so a mode can disable a feature outright rather than just
/// rewording its prose. [`Capability::enabled`] is the single predicate; the
/// engine calls it at the point of action (e.g. before minting a re-query
/// handle or honoring a `resume_handle`/`seed`), so a disabled capability is
/// rejected/inert regardless of what the model asked for.
///
/// [`LlmMode`]: crate::config::extended::LlmMode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Re-queryable read-only noninteractive subagents + seeded tool calls
    /// (GOALS §3c): the follow-up handle, `resume_handle` rehydration, and
    /// `seed` injection. `normal`-mode only — the first behavioral gate on
    /// the `LlmMode` axis.
    FollowupSeed,
}

impl Capability {
    /// Whether this capability is available under `mode`. Disabled
    /// capabilities are gated at the engine's point of action, not merely
    /// hidden in description text.
    pub fn enabled(self, mode: crate::config::extended::LlmMode) -> bool {
        use crate::config::extended::LlmMode;
        match self {
            // Follow-up/seed is a strong-model affordance: the weak-model
            // (defensive) target re-spawns cold instead (GOALS §3c).
            Capability::FollowupSeed => matches!(mode, LlmMode::Normal),
        }
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

    /// Project every tool to a `ToolDefinition`, rendering descriptions in
    /// the given `llm_mode`. The `mode` flows from the active
    /// [`crate::config::extended::LlmMode`] through the agent spawn.
    pub fn definitions(&self, mode: crate::config::extended::LlmMode) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|t| definition_of(&**t, mode))
            .collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use crate::config::extended::LlmMode;

    /// The follow-up/seed capability is the first behavioral `LlmMode` gate:
    /// available in normal mode, disabled in defensive (GOALS §3c).
    #[test]
    fn followup_seed_is_normal_only() {
        assert!(Capability::FollowupSeed.enabled(LlmMode::Normal));
        assert!(!Capability::FollowupSeed.enabled(LlmMode::Defensive));
    }
}

#[cfg(test)]
mod llm_mode_tests {
    use super::*;
    use crate::config::extended::LlmMode;
    use crate::tools;

    /// Every built-in tool, in one registry. Drives the full-surface
    /// coverage test so a future tool added to the built-in surface cannot
    /// silently skip its defensive description — add it here and the
    /// coverage assertion forces a defensive variant. `bash` and `task`
    /// build their descriptions at construction, so they appear via their
    /// real constructors. Custom-bash tools (`webfetch`/…) are
    /// user-config-driven — their author owns the wording — so they are
    /// deliberately excluded.
    fn all_builtin_tools() -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(tools::read::ReadTool),
            Arc::new(tools::readlock::ReadlockTool),
            Arc::new(tools::writeunlock::WriteunlockTool),
            Arc::new(tools::unlock::UnlockTool),
            Arc::new(tools::editunlock::EditunlockTool),
            Arc::new(tools::bash::BashTool::new()),
            Arc::new(tools::skill::SkillTool),
            Arc::new(tools::question::QuestionTool),
            Arc::new(tools::defer::DeferTool),
            Arc::new(tools::jobs::JobsTool),
            Arc::new(tools::intel::TreeTool),
            Arc::new(tools::intel::OutlineTool),
            Arc::new(tools::intel::SymbolFindTool),
            Arc::new(tools::intel::WordTool),
            Arc::new(tools::intel::DepsTool),
            Arc::new(tools::intel::HotTool),
            Arc::new(tools::intel::CircularTool),
            Arc::new(tools::intel::SearchTool),
            Arc::new(tools::plan::CreatePlanTool),
            Arc::new(tools::plan::AddStepTool),
            Arc::new(tools::plan::AddDependencyTool),
            Arc::new(tools::plan::SetBranchesTool),
            Arc::new(tools::plan::ListPlansTool),
            Arc::new(tools::session_search::SessionSearchTool),
            Arc::new(tools::session_read::SessionReadTool),
            Arc::new(tools::grep::GrepTool),
            Arc::new(tools::glob::GlobTool),
            Arc::new(tools::task::TaskTool::with_subagents(&["coder", "explore"])),
        ]
    }

    /// FULL-SURFACE COVERAGE: every built-in tool must supply a non-empty
    /// defensive description that is meaningfully more explicit than its
    /// terse one — no terse-fallback gaps, no TODO tools. Registry-driven,
    /// so a future built-in tool can't silently skip.
    #[test]
    fn every_builtin_tool_has_a_defensive_description() {
        for tool in all_builtin_tools() {
            let terse = tool.description().to_string();
            let defensive = tool.defensive_description().unwrap_or_else(|| {
                panic!(
                    "built-in tool `{}` has no defensive_description — full-surface coverage requires one",
                    tool.name()
                )
            });
            assert!(
                !defensive.trim().is_empty(),
                "tool `{}` has an empty defensive description",
                tool.name()
            );
            // Defensive is the *verbose* form: it must be longer than the
            // terse one and not byte-identical (the deliberate token
            // tradeoff). A handful of words wouldn't be "explicit steering."
            assert!(
                defensive.len() > terse.len(),
                "tool `{}` defensive description is not more explicit than terse ({} <= {})",
                tool.name(),
                defensive.len(),
                terse.len()
            );
            assert!(
                defensive.len() >= 80,
                "tool `{}` defensive description is too terse to be steering ({} chars)",
                tool.name(),
                defensive.len()
            );
        }
    }

    /// Defensive parameters, when supplied, keep the SAME shape + required
    /// set as the terse parameters — tool grants never vary by mode, only
    /// how descriptions render. We compare the structural skeleton
    /// (property names + `required` + `enum`s), ignoring `description`.
    #[test]
    fn defensive_parameters_preserve_shape() {
        for tool in all_builtin_tools() {
            let Some(defensive) = tool.defensive_parameters() else {
                continue;
            };
            let terse = tool.parameters();
            assert_eq!(
                skeleton(&terse),
                skeleton(&defensive),
                "tool `{}` defensive parameters changed the schema shape",
                tool.name()
            );
        }
    }

    /// Strip every `description` field from a JSON schema, leaving the
    /// structural skeleton (types, property names, `required`, `enum`s).
    fn skeleton(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, val) in map {
                    if k == "description" {
                        continue;
                    }
                    out.insert(k.clone(), skeleton(val));
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(skeleton).collect())
            }
            other => other.clone(),
        }
    }

    /// The centralized rendering seam: in `Normal` the definition carries
    /// the terse description; in `Defensive` it carries the verbose one.
    /// The switch lives in `definition_of` and nowhere else.
    #[test]
    fn definition_of_switches_description_on_mode() {
        let tool = tools::read::ReadTool;
        let normal = definition_of(&tool, LlmMode::Normal);
        let defensive = definition_of(&tool, LlmMode::Defensive);
        assert_eq!(normal.description, tool.description());
        assert_eq!(defensive.description, tool.defensive_description().unwrap());
        assert_ne!(normal.description, defensive.description);
    }

    /// A tool with no defensive override falls back to the terse form in
    /// BOTH modes (the `None`-keeper path — custom-bash tools rely on this).
    #[test]
    fn definition_of_falls_back_when_no_defensive_variant() {
        struct Terse;
        #[async_trait]
        impl Tool for Terse {
            fn name(&self) -> &str {
                "terse"
            }
            fn description(&self) -> &str {
                "terse one-liner"
            }
            fn parameters(&self) -> Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
                Ok(ToolOutput::text(""))
            }
        }
        let t = Terse;
        assert_eq!(
            definition_of(&t, LlmMode::Normal).description,
            definition_of(&t, LlmMode::Defensive).description,
            "a tool with no defensive variant renders identically in both modes"
        );
    }

    /// NORMAL-MODE BUDGET GUARD: rendered in `Normal`, every built-in tool's
    /// description stays terse (the current token-economy budget the CI check
    /// enforces). Evaluated against `Normal` specifically — defensive's
    /// growth is the intended tradeoff and is exempt. One sentence ≈ under
    /// ~200 chars is the terse bar.
    #[test]
    fn normal_mode_descriptions_stay_within_terse_budget() {
        for tool in all_builtin_tools() {
            let def = definition_of(&*tool, LlmMode::Normal);
            assert!(
                def.description.len() <= 200,
                "tool `{}` normal-mode description exceeds the terse budget ({} chars): {}",
                tool.name(),
                def.description.len(),
                def.description
            );
        }
    }
}
