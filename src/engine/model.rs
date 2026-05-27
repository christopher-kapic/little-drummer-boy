//! Provider-side completion model dispatch.
//!
//! `CompletionModel` in rig isn't object-safe (associated types +
//! `impl Trait` returns + `Self` in return position), so we can't hold a
//! `Box<dyn CompletionModel>`. The pattern upstream now recommends is an
//! enum dispatch — see rig's `examples/enum_dispatch.rs`. v0 ships one
//! variant (`OpenAi`, used for every OpenAI-compatible endpoint in the
//! user's [`crate::providers`] templates) and a stub for adding
//! `Anthropic` / `OpenRouter` / `Ollama` later.
//!
//! Authentication: we delegate to
//! [`crate::providers::models_fetch::resolve_provider_request`], the
//! same resolver `/models` fetches use. For most providers that's just
//! `$VAR` expansion over the configured `Authorization` header; for
//! GitHub Copilot it also honors the documented env-var sources
//! (`COPILOT_GITHUB_TOKEN`/`GH_TOKEN`/`GITHUB_TOKEN`/`GITHUB_COPILOT_API_TOKEN`)
//! and the `COPILOT_API_URL` base-URL override. The bearer token is
//! handed to rig's `api_key`; the rest of the resolved headers aren't
//! passed yet (good enough for v0; provider-specific headers like
//! `OpenAI-Beta` or `anthropic-version` get added when we wire the
//! Anthropic variant).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use futures::StreamExt;
use rig::client::CompletionClient;
use rig::completion::Completion;
use rig::message::{Message, Reasoning, ReasoningContent, ToolChoice};
use rig::providers::openai;
use rig::streaming::StreamedAssistantContent;
use serde_json::json;
use tokio::sync::mpsc;

use crate::engine::agent::TurnEvent;

// `openai::Client` is rig's *Responses API* client (POSTs `/responses`).
// Every OpenAI-compatible provider in `src/providers/mod.rs` (z.ai,
// MiniMax, OpenCode Zen, generic openai-compatible, Ollama) speaks the
// *Chat Completions* API — `/chat/completions`. We have to construct
// the `CompletionsClient` variant instead, or every non-OpenAI-proper
// endpoint 404s on the wrong path.
type OpenAiCompatClient = openai::CompletionsClient;

/// When set (by `--debug-last-message`), every call to [`Model::complete`]
/// writes a pretty-printed JSON dump of the outbound request to this
/// path before invoking rig. The file is overwritten each turn.
///
/// Holds the *target file path*, not just a flag — the resolver does
/// the `cwd/.lastmessage` join once at startup so we don't depend on
/// `std::env::current_dir()` from inside the agent task.
static DEBUG_LAST_MESSAGE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Plumb `--debug-last-message` into the engine. Idempotent — second
/// calls are no-ops because `OnceLock::set` returns `Err` once set.
/// Called from `main.rs` before any agent loop starts.
pub fn enable_debug_last_message(path: PathBuf) {
    let _ = DEBUG_LAST_MESSAGE_PATH.set(path);
}

fn debug_last_message_path() -> Option<&'static Path> {
    DEBUG_LAST_MESSAGE_PATH.get().map(PathBuf::as_path)
}

use crate::config::providers::{ActiveModelRef, ProviderEntry, ProvidersConfig};
use crate::engine::message::{AssistantContent, OneOrMany, ToolDefinition};
use crate::providers::models_fetch;
use crate::tokens::TokenUsage;
use rig::completion::GetTokenUsage;

/// One concrete provider-flavor of completion model. Add variants here
/// as we wire more providers.
pub enum Model {
    /// OpenAI-compatible chat-completions endpoint. Used for the
    /// generic openai-compatible template and every vendor that exposes
    /// `/v1/chat/completions` (z.ai, MiniMax, OpenCode Zen, Ollama,
    /// OpenRouter, …). The model id is what the provider's API
    /// expects (e.g. `claude-opus-4-7`, `glm-4.6`, `gpt-4o-mini`).
    OpenAi {
        client: OpenAiCompatClient,
        model_id: String,
    },
}

impl Model {
    /// Resolve the active model from the user's config + credentials and
    /// build a concrete `Model`. Returns a descriptive error when nothing
    /// is configured or the env var that holds the key isn't set.
    pub fn from_config(cfg: &ProvidersConfig) -> Result<Self> {
        let active: &ActiveModelRef = cfg.active_model.as_ref().context(
            "no active model selected — run /model or set COCKPIT_PROVIDER/COCKPIT_MODEL",
        )?;
        let entry = cfg
            .providers
            .get(&active.provider)
            .with_context(|| format!("provider `{}` is not configured", active.provider))?;
        build_openai_model(&active.provider, entry, &active.model)
    }

    /// Build a `Model` for an arbitrary `(provider, model_id)` pair,
    /// re-using the same auth-header / env-resolve pipeline as
    /// [`Self::from_config`] but bypassing the active-model selection.
    /// Used by background-only flows (auto-titling §17d, prompt-
    /// injection guard §4i) that target the utility model rather than
    /// whatever the user has selected for the foreground turn.
    pub fn for_provider(cfg: &ProvidersConfig, provider_id: &str, model_id: &str) -> Result<Self> {
        let entry = cfg
            .providers
            .get(provider_id)
            .with_context(|| format!("provider `{provider_id}` is not configured"))?;
        build_openai_model(provider_id, entry, model_id)
    }

    /// One-shot, non-streaming, no-tools text completion. Used by
    /// background tasks (auto-titling, prompt-injection guard) that
    /// just want a string back without the streaming + tool-dispatch
    /// machinery of [`Self::complete`]. Returns the assistant's full
    /// text response, trimmed.
    pub async fn text_completion(&self, prompt: &str) -> Result<String> {
        use rig::completion::Prompt;
        match self {
            Model::OpenAi { client, model_id } => {
                let agent = client.agent(model_id).build();
                let response = agent
                    .prompt(prompt)
                    .await
                    .context("text_completion: prompt failed")?;
                Ok(response.trim().to_string())
            }
        }
    }

    /// Build a streaming completion request and aggregate it.
    ///
    /// Streaming is on for every provider variant — rig's
    /// `StreamingCompletionResponse` aggregates `choice` and
    /// `message_id` internally as the stream advances, so by the time
    /// we exhaust the stream we have the same shape the non-streaming
    /// `send()` path would have produced. We emit a
    /// [`TurnEvent::AssistantTextDelta`] for every `Message(...)`
    /// chunk (and drop `Reasoning`/`ReasoningDelta` — the TUI shows
    /// `Thinking…` instead per user spec).
    pub async fn complete(
        &self,
        system: &str,
        history: &[Message],
        prompt: Message,
        tools: &[ToolDefinition],
        params: ModelParams,
        agent_name: &str,
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(
        Option<String>,
        OneOrMany<AssistantContent>,
        Option<TokenUsage>,
    )> {
        // Strip reasoning content from every prior assistant turn
        // before sending it back to the model. Past thinking blocks
        // bloat the prompt without informing the next turn's output
        // — the user already saw them via the expandable chip; the
        // model doesn't need its own scratch work re-fed for the
        // next inference.
        let history: Vec<Message> = history.iter().map(strip_reasoning).collect();

        if let Some(path) = debug_last_message_path() {
            dump_request(
                path,
                self.model_id(),
                system,
                &history,
                &prompt,
                tools,
                &params,
            );
        }

        match self {
            Model::OpenAi { client, model_id } => {
                let agent = build_agent(client, model_id, system, tools, &params);

                let mut req = agent.completion(prompt, history).await?;
                if params.tools_required && !tools.is_empty() {
                    req = req.tool_choice(ToolChoice::Required);
                }
                let mut stream = req.stream().await?;
                while let Some(item) = stream.next().await {
                    match item? {
                        StreamedAssistantContent::Text(text) if !text.text.is_empty() => {
                            let _ = event_tx
                                .send(TurnEvent::AssistantTextDelta {
                                    agent: agent_name.to_string(),
                                    delta: text.text,
                                })
                                .await;
                        }
                        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                            // Capture for the "expand thinking block"
                            // feature; the TUI hides this by default.
                            let _ = event_tx
                                .send(TurnEvent::ReasoningDelta {
                                    agent: agent_name.to_string(),
                                    delta: reasoning,
                                })
                                .await;
                        }
                        StreamedAssistantContent::Reasoning(r) => {
                            let combined = collect_reasoning_text(&r);
                            if !combined.is_empty() {
                                let _ = event_tx
                                    .send(TurnEvent::ReasoningDelta {
                                        agent: agent_name.to_string(),
                                        delta: combined,
                                    })
                                    .await;
                            }
                        }
                        // ToolCallDelta / ToolCall / Final are
                        // aggregated into `stream.choice` /
                        // `stream.message_id` internally; the
                        // post-loop reads pick them up.
                        _ => {}
                    }
                }
                // rig requests `stream_options.include_usage = true`
                // on every OpenAI-compat stream; the final usage chunk
                // lands on `stream.response` (Option, because some
                // providers omit it).
                let usage = stream
                    .response
                    .token_usage()
                    .map(TokenUsage::from)
                    .filter(|u| !u.is_empty());
                Ok((stream.message_id.clone(), stream.choice.clone(), usage))
            }
        }
    }

    fn model_id(&self) -> &str {
        match self {
            Model::OpenAi { model_id, .. } => model_id,
        }
    }
}

/// Build an OpenAI-compat client using the shared provider resolver so
/// that Copilot's documented env-var fallbacks (and `COPILOT_API_URL`
/// base-URL override) work for inference, not just `/models` fetches.
fn build_openai_model(provider_id: &str, entry: &ProviderEntry, model_id: &str) -> Result<Model> {
    let resolved = models_fetch::resolve_provider_request(provider_id, entry)?;
    let auth = resolved
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("authorization"))
        .with_context(|| {
            format!("provider `{provider_id}` produced no Authorization header after resolution")
        })?;
    let token = auth
        .value
        .strip_prefix("Bearer ")
        .or_else(|| auth.value.strip_prefix("bearer "))
        .unwrap_or(&auth.value)
        .trim()
        .to_string();

    // rig appends `/chat/completions` to the base URL (see
    // `OpenAICompletionsExt`'s build_uri). The user's templates put the
    // version segment in the base URL already (e.g. `https://api.minimax.io/v1`),
    // giving the right final URL `https://api.minimax.io/v1/chat/completions`.
    let client = openai::CompletionsClient::builder()
        .api_key(token)
        .base_url(&resolved.base_url)
        .build()
        .with_context(|| format!("building openai-compatible client for `{provider_id}`"))?;
    Ok(Model::OpenAi {
        client,
        model_id: model_id.to_string(),
    })
}

/// Per-turn knobs the agent loop hands to the model.
#[derive(Debug, Clone, Default)]
pub struct ModelParams {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    /// When true, on the first turn force `tool_choice = required` so
    /// the model has to call a tool rather than answer from priors. We
    /// don't use this in v0 (agents may legitimately reply text-only),
    /// but the knob is wired for the future.
    pub tools_required: bool,
}

/// Build a `rig::agent::Agent` (we only use its `.completion()` builder,
/// not its `.prompt()` convenience layer). The construction is identical
/// across providers; only the client type differs, so this lives here
/// rather than in each variant.
///
/// `AgentBuilder` is type-stated — `.tool()` transitions from
/// `NoToolConfig` to `WithBuilderTools`, which is why we use the plural
/// `.tools()` (accepts `Vec<Box<dyn ToolDyn>>`) so the transition is one
/// step and we don't have to reassign across types.
fn build_agent<C: CompletionClient>(
    client: &C,
    model_id: &str,
    system: &str,
    tools: &[ToolDefinition],
    params: &ModelParams,
) -> rig::agent::Agent<C::CompletionModel> {
    let boxed: Vec<Box<dyn rig::tool::ToolDyn>> = tools
        .iter()
        .map(|def| Box::new(StaticTool(def.clone())) as Box<dyn rig::tool::ToolDyn>)
        .collect();
    let mut b = client.agent(model_id).preamble(system).tools(boxed);
    if let Some(t) = params.temperature {
        b = b.temperature(t);
    }
    if let Some(m) = params.max_tokens {
        b = b.max_tokens(m);
    }
    b.build()
}

/// Remove `AssistantContent::Reasoning` items from a message's
/// content vector. Used to scrub past thinking blocks from the
/// history before each outbound request.
fn strip_reasoning(msg: &Message) -> Message {
    match msg {
        Message::Assistant { id, content } => {
            let kept: Vec<AssistantContent> = content
                .iter()
                .filter(|c| !matches!(c, AssistantContent::Reasoning(_)))
                .cloned()
                .collect();
            // `OneOrMany::one_or_many` errors on empty input; preserve
            // the original message verbatim in that pathological case
            // (an assistant turn that contained only reasoning).
            match OneOrMany::many(kept) {
                Ok(new_content) => Message::Assistant {
                    id: id.clone(),
                    content: new_content,
                },
                Err(_) => msg.clone(),
            }
        }
        other => other.clone(),
    }
}

/// Pull every `ReasoningContent::Text` chunk out of a complete
/// `Reasoning` block, joined with newlines. Empty for non-text
/// reasoning content (which rig models internally but we don't
/// display).
fn collect_reasoning_text(r: &Reasoning) -> String {
    r.content
        .iter()
        .filter_map(|c| match c {
            ReasoningContent::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Write the outbound request to `path` for debugging. Best-effort —
/// any error is traced but never propagated, because losing a debug
/// dump must not break a live turn.
fn dump_request(
    path: &Path,
    model_id: &str,
    system: &str,
    history: &[Message],
    prompt: &Message,
    tools: &[ToolDefinition],
    params: &ModelParams,
) {
    let body = json!({
        "model": model_id,
        "system": system,
        "tools": tools,
        "params": {
            "temperature": params.temperature,
            "max_tokens": params.max_tokens,
            "tools_required": params.tools_required,
        },
        "history": history,
        "prompt": prompt,
    });
    let pretty = match serde_json::to_string_pretty(&body) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "debug-last-message: serialization failed");
            return;
        }
    };
    if let Err(e) = std::fs::write(path, format!("{pretty}\n")) {
        tracing::warn!(path = %path.display(), error = %e, "debug-last-message: write failed");
    }
}

/// A `rig::tool::Tool` that exists only to advertise a `ToolDefinition`
/// to the model. The dispatcher never asks rig to *call* this tool — we
/// route through our own [`crate::engine::tool::ToolBox`] — so the
/// `call` impl is unreachable in normal flow. It returns an error if
/// rig ever invokes it, which would mean we somehow plumbed it into
/// the wrong path.
struct StaticTool(ToolDefinition);

#[derive(Debug, thiserror::Error)]
#[error("StaticTool::call should never be invoked — cockpit dispatches through ToolBox")]
struct StaticToolError;

impl rig::tool::Tool for StaticTool {
    const NAME: &'static str = "static-cockpit-tool";

    type Error = StaticToolError;
    type Args = serde_json::Value;
    type Output = String;

    fn name(&self) -> String {
        self.0.name.clone()
    }

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        self.0.clone()
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        Err(StaticToolError)
    }
}
