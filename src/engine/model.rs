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
//! Authentication: we expect the resolved `Authorization` header to be
//! `Bearer <token>` (every template in `src/providers/mod.rs` matches).
//! The bearer is extracted and handed to rig's `api_key`; the rest of
//! the headers cockpit owns aren't passed yet (good enough for v0;
//! provider-specific headers like `OpenAI-Beta` or `anthropic-version`
//! get added when we wire the Anthropic variant).

use anyhow::{Context, Result, bail};
use rig::client::CompletionClient;
use rig::completion::Completion;
use rig::message::{Message, ToolChoice};
use rig::providers::openai;

// `openai::Client` is rig's *Responses API* client (POSTs `/responses`).
// Every OpenAI-compatible provider in `src/providers/mod.rs` (z.ai,
// MiniMax, OpenCode Zen, generic openai-compatible, Ollama) speaks the
// *Chat Completions* API — `/chat/completions`. We have to construct
// the `CompletionsClient` variant instead, or every non-OpenAI-proper
// endpoint 404s on the wrong path.
type OpenAiCompatClient = openai::CompletionsClient;

use crate::config::providers::{ActiveModelRef, ProvidersConfig};
use crate::envref;
use crate::engine::message::{AssistantContent, OneOrMany, ToolDefinition};

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
        let active: &ActiveModelRef = cfg
            .active_model
            .as_ref()
            .context("no active model selected — run /model or set COCKPIT_PROVIDER/COCKPIT_MODEL")?;
        let entry = cfg
            .providers
            .get(&active.provider)
            .with_context(|| format!("provider `{}` is not configured", active.provider))?;

        // Pull the Authorization header, resolve env vars in its value,
        // and strip a leading `Bearer ` so rig's `api_key()` gets just
        // the token. If we can't find one, error loud.
        let auth_header = entry
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("authorization"))
            .with_context(|| {
                format!(
                    "provider `{}` has no Authorization header configured",
                    active.provider
                )
            })?;
        let resolved = envref::resolve(&auth_header.value);
        if resolved.has_missing() {
            bail!(
                "Authorization for provider `{}` references unset env var(s): {}",
                active.provider,
                resolved.missing.join(", ")
            );
        }
        let token = resolved
            .value
            .strip_prefix("Bearer ")
            .or_else(|| resolved.value.strip_prefix("bearer "))
            .unwrap_or(&resolved.value)
            .trim()
            .to_string();

        // rig appends `/chat/completions` to the base URL (see
        // `OpenAICompletionsExt`'s build_uri). The user's templates put
        // the version segment in the base URL already
        // (e.g. `https://api.minimax.io/v1`), giving the right final URL
        // `https://api.minimax.io/v1/chat/completions`.
        let client = openai::CompletionsClient::builder()
            .api_key(token)
            .base_url(&entry.url)
            .build()
            .with_context(|| format!("building openai-compatible client for `{}`", active.provider))?;

        Ok(Model::OpenAi {
            client,
            model_id: active.model.clone(),
        })
    }

    /// Build a one-shot request and send it. Tools, the system prompt,
    /// the prior conversation history, and the new user message all go
    /// in here. Returns the model's `OneOrMany<AssistantContent>` plus
    /// the `message_id` rig surfaced (used as the assistant turn's id
    /// in history).
    ///
    /// Non-streaming for v0 — see [`crate::engine::agent`].
    pub async fn complete(
        &self,
        system: &str,
        history: &[Message],
        prompt: Message,
        tools: &[ToolDefinition],
        params: ModelParams,
    ) -> Result<(Option<String>, OneOrMany<AssistantContent>)> {
        match self {
            Model::OpenAi { client, model_id } => {
                let agent = build_agent(client, model_id, system, tools, &params);

                let mut req = agent.completion(prompt, history.to_vec()).await?;
                if params.tools_required && !tools.is_empty() {
                    req = req.tool_choice(ToolChoice::Required);
                }
                let resp = req.send().await?;
                Ok((resp.message_id, resp.choice))
            }
        }
    }
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
