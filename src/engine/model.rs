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
use tokio_util::sync::CancellationToken;

use crate::engine::agent::TurnEvent;
use crate::engine::retry;

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

/// Sentinel error returned by [`Model::complete_captured`] when the
/// in-flight inference was aborted by a user ctrl+c (a `CancelTurn`
/// request). Distinct from a provider/transport failure so the driver
/// can unwind the turn cleanly (back to idle) rather than logging it as
/// a real error. Downcast through the `anyhow` chain to detect it.
#[derive(Debug, thiserror::Error)]
#[error("inference cancelled by user")]
pub struct InferenceCancelled;

/// Returns `true` when `err`'s chain carries an [`InferenceCancelled`]
/// sentinel — i.e. the turn was aborted by the user, not a real failure.
pub fn is_cancelled(err: &anyhow::Error) -> bool {
    err.downcast_ref::<InferenceCancelled>().is_some()
}

/// Sentinel returned at the inference-dispatch chokepoint when the daemon
/// has begun draining (`daemon-graceful-drain-shutdown.md`): no *new*
/// provider request may go out once shutdown starts. In-flight calls that
/// already passed the gate run to completion; this only blocks calls that
/// would start after the drain began. Distinct from a transport failure so
/// the driver unwinds the turn cleanly rather than logging a real error.
#[derive(Debug, thiserror::Error)]
#[error("inference refused: daemon is shutting down")]
pub struct InferenceGated;

/// Returns `true` when `err`'s chain carries an [`InferenceGated`] sentinel
/// — i.e. the call was refused because the daemon began draining.
pub fn is_gated(err: &anyhow::Error) -> bool {
    err.downcast_ref::<InferenceGated>().is_some()
}

/// Sentinel embedded in a [`rig::completion::CompletionError`] when a
/// retry *attempt* is aborted by ctrl+c (as opposed to a transport
/// failure). It is wrapped in `RequestError`, which the retry taxonomy
/// classifies fail-fast, so [`retry::with_retry`] returns at once
/// instead of retrying; `complete_captured` then maps it to
/// [`InferenceCancelled`].
#[derive(Debug, thiserror::Error)]
#[error("inference attempt cancelled by user")]
struct AttemptCancelled;

/// Build the cancellation sentinel as a `CompletionError`.
fn attempt_cancelled() -> rig::completion::CompletionError {
    rig::completion::CompletionError::RequestError(Box::new(AttemptCancelled))
}

/// Detect the [`AttemptCancelled`] sentinel in a `CompletionError`.
fn is_attempt_cancelled(err: &rig::completion::CompletionError) -> bool {
    if let rig::completion::CompletionError::RequestError(inner) = err {
        // Walk the boxed error chain for the marker.
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(inner.as_ref());
        while let Some(e) = current {
            if e.downcast_ref::<AttemptCancelled>().is_some() {
                return true;
            }
            current = e.source();
        }
    }
    false
}

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
        /// Daemon-wide graceful-shutdown gate
        /// (`daemon-graceful-drain-shutdown.md`). Every outbound provider
        /// request consults it; once the daemon begins draining it refuses
        /// new dispatches with [`InferenceGated`]. A model built outside the
        /// daemon (tests, the auto-title / skill-select utility paths) gets
        /// the default never-draining gate. The registry installs the
        /// daemon's shared gate via [`Model::with_shutdown_gate`].
        gate: crate::daemon::shutdown::ShutdownSignal,
    },
}

impl Model {
    /// The shared inference-dispatch gate for this model. The single seam
    /// both [`Self::complete_captured`] and [`Self::text_completion`]
    /// consult before any provider round-trip.
    fn gate(&self) -> &crate::daemon::shutdown::ShutdownSignal {
        match self {
            Model::OpenAi { gate, .. } => gate,
        }
    }

    /// Install the daemon's shared shutdown gate, replacing the default
    /// never-draining one. Called by the registry when it builds a worker's
    /// model so the model dispatches through the daemon's central drain
    /// authority. Consuming-builder style so the registry can wrap the
    /// model in an `Arc` immediately after.
    pub fn with_shutdown_gate(mut self, signal: crate::daemon::shutdown::ShutdownSignal) -> Self {
        match &mut self {
            Model::OpenAi { gate, .. } => *gate = signal,
        }
        self
    }
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
        // Inference-dispatch chokepoint: refuse a *new* provider request once
        // the daemon has begun draining (`daemon-graceful-drain-shutdown.md`).
        if self.gate().is_draining() {
            return Err(anyhow::Error::new(InferenceGated));
        }
        match self {
            Model::OpenAi {
                client, model_id, ..
            } => {
                let agent = client.agent(model_id).build();
                let response = agent
                    .prompt(prompt)
                    .await
                    .context("text_completion: prompt failed")?;
                Ok(response.trim().to_string())
            }
        }
    }

    /// One-shot, non-streaming, single-tool completion that **forces** the
    /// model to answer through `tool` (`tool_choice = required`). Used by
    /// background tasks that need a *structured* verdict rather than free
    /// text — the prompt-injection guard's `risk` tool (GOALS §4i). Sends
    /// only `system` + `prompt` (no conversation history), and returns
    /// every [`ToolCall`] the model emitted so the caller can read the
    /// structured arguments. History-free by construction: the untrusted
    /// text the caller wraps into `prompt` is the only content the model
    /// sees.
    pub async fn tool_completion(
        &self,
        system: &str,
        prompt: &str,
        tool: &ToolDefinition,
    ) -> Result<Vec<crate::engine::message::ToolCall>> {
        use rig::completion::Completion;
        // Inference-dispatch chokepoint: refuse a *new* provider request once
        // the daemon has begun draining (`daemon-graceful-drain-shutdown.md`).
        if self.gate().is_draining() {
            return Err(anyhow::Error::new(InferenceGated));
        }
        match self {
            Model::OpenAi {
                client, model_id, ..
            } => {
                let agent = client.agent(model_id).preamble(system).build();
                let response = agent
                    .completion(Message::user(prompt), Vec::<Message>::new())
                    .await?
                    .tool(tool.clone())
                    .tool_choice(ToolChoice::Required)
                    .send()
                    .await
                    .context("tool_completion: send failed")?;
                Ok(crate::engine::message::collect_tool_calls(&response.choice))
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
    ///
    /// **Also returns the full assembled request body** that was handed
    /// to the provider — exactly what hit the wire, after the driver's
    /// upstream redaction (session-log-export Part A). The caller persists
    /// it via
    /// [`crate::session::Session::record_inference_request`] keyed by the
    /// same `call_id` it uses for the `inference_calls` metadata row.
    ///
    /// The body is assembled here, at the engine→provider boundary,
    /// because this is the only place that knows the post-strip-reasoning
    /// history + resolved model id. We do not (cannot) read rig's exact
    /// serialized HTTP body — rig builds and sends it internally without
    /// exposing the bytes — so the faithful capture is the same
    /// `(model, provider, params, system, tools, history, prompt)` tuple
    /// rig receives (verified via kcl `rig-core`).
    #[allow(clippy::too_many_arguments)]
    pub async fn complete_captured(
        &self,
        system: &str,
        history: &[Message],
        prompt: Message,
        tools: &[ToolDefinition],
        params: ModelParams,
        agent_name: &str,
        event_tx: &mpsc::Sender<TurnEvent>,
        cancel: &CancellationToken,
    ) -> Result<(
        (
            Option<String>,
            OneOrMany<AssistantContent>,
            Option<TokenUsage>,
        ),
        serde_json::Value,
    )> {
        // Strip reasoning content from every prior assistant turn
        // before sending it back to the model. Past thinking blocks
        // bloat the prompt without informing the next turn's output
        // — the user already saw them via the expandable chip; the
        // model doesn't need its own scratch work re-fed for the
        // next inference.
        let history: Vec<Message> = history.iter().map(strip_reasoning).collect();

        // Assemble the as-sent request body once: it's both the
        // `--debug-last-message` dump and the always-on capture payload.
        let captured = assembled_request(
            self.model_id(),
            self.provider_label(),
            system,
            &history,
            &prompt,
            tools,
            &params,
        );

        if let Some(path) = debug_last_message_path() {
            write_dump(path, &captured);
        }

        type CompleteOut = (
            Option<String>,
            OneOrMany<AssistantContent>,
            Option<TokenUsage>,
        );
        // Bail before doing any provider work if cancellation already
        // fired (e.g. the user pressed ctrl+c between turns). Cheap and
        // keeps the cancel path from racing a fresh round-trip.
        if cancel.is_cancelled() {
            return Err(anyhow::Error::new(InferenceCancelled));
        }

        // Inference-dispatch chokepoint (`daemon-graceful-drain-shutdown.md`):
        // once the daemon begins draining, no *new* provider request goes
        // out. A request already past this gate keeps streaming; this refuses
        // only the ones that would start after the drain began. Checked here,
        // before any client work, so the gate is the single real seam — not
        // an advisory flag each call site must remember.
        if self.gate().is_draining() {
            return Err(anyhow::Error::new(InferenceGated));
        }

        // Build a connectivity probe from the provider base URL so a
        // backoff wait short-circuits the moment the link returns. `None`
        // (unparseable URL) falls back to plain backoff — never fatal.
        let probe = match self {
            Model::OpenAi { client, .. } => retry::TcpProbe::from_base_url(client.base_url()),
        };

        // Each attempt builds + drains a *fresh* stream: a failed
        // attempt's partial is discarded, never resumed (prompt edge
        // case). `with_retry` re-invokes this closure on a network/
        // transient failure with jittered, capped backoff; a non-
        // transient error fails fast. Persistence in `agent::turn` runs
        // once, after this whole retry unit settles — so a retried call
        // logs exactly one inference outcome.
        //
        // Cancellation: the select arms below short-circuit a ctrl+c
        // *during an attempt* via [`AttemptCancelled`] (classified
        // fail-fast, so `with_retry` returns at once); cancellation
        // *during a backoff wait* is interrupted immediately by
        // `with_retry`'s own select against `cancel`. Either way we map
        // the final state to the `InferenceCancelled` sentinel below.
        let attempt = || async {
            match self {
                Model::OpenAi {
                    client, model_id, ..
                } => {
                    let agent = build_agent(client, model_id, system, tools, &params);

                    let mut req = agent.completion(prompt.clone(), history.clone()).await?;
                    if params.tools_required && !tools.is_empty() {
                        req = req.tool_choice(ToolChoice::Required);
                    }
                    // Build the stream, racing the build against
                    // cancellation so a ctrl+c during the initial round-
                    // trip aborts promptly.
                    let mut stream = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(attempt_cancelled()),
                        built = req.stream() => built?,
                    };
                    loop {
                        // Race each chunk against cancellation: a ctrl+c
                        // aborts the in-flight stream instead of waiting
                        // for the model to finish. Dropping `stream` on
                        // the cancel arm closes the underlying HTTP body.
                        let item = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return Err(attempt_cancelled()),
                            next = stream.next() => match next {
                                Some(item) => item,
                                None => break,
                            },
                        };
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
                    Ok::<CompleteOut, rig::completion::CompletionError>((
                        stream.message_id.clone(),
                        stream.choice.clone(),
                        usage,
                    ))
                }
            }
        };

        let out = retry::with_retry(agent_name, event_tx, cancel, probe.as_ref(), attempt).await;

        match out {
            Ok(value) => Ok((value, captured)),
            Err(err) => {
                // A ctrl+c (either during an attempt via the
                // `AttemptCancelled` sentinel, or because the token fired
                // during a backoff wait) unwinds the turn cleanly rather
                // than logging a real failure.
                if cancel.is_cancelled() || is_attempt_cancelled(&err) {
                    Err(anyhow::Error::new(InferenceCancelled))
                } else {
                    Err(anyhow::Error::new(err))
                }
            }
        }
    }

    fn model_id(&self) -> &str {
        match self {
            Model::OpenAi { model_id, .. } => model_id,
        }
    }

    /// Provider-flavor label for the captured request body. Coarse —
    /// the exact configured provider id lives on the session row; this
    /// is the wire-flavor the model client speaks.
    fn provider_label(&self) -> &str {
        match self {
            Model::OpenAi { .. } => "openai-compatible",
        }
    }
}

/// Build an OpenAI-compat client using the shared provider resolver so
/// that Copilot's documented env-var fallbacks (and `COPILOT_API_URL`
/// base-URL override) work for inference, not just `/models` fetches.
fn build_openai_model(provider_id: &str, entry: &ProviderEntry, model_id: &str) -> Result<Model> {
    let resolved = models_fetch::resolve_provider_request(provider_id, entry)?;
    // A missing Authorization header means the provider is keyless — a
    // fully-local OpenAI-compatible endpoint (e.g. LM Studio at
    // `http://localhost:1234/v1`). That is not an error: the resolver
    // already errors for an Authorization ref whose env var is unset
    // (`models_fetch::resolve_provider_request`), so here absence means
    // "send no auth". Build the client with an empty api key — rig's
    // OpenAI-compat `CompletionsClient` has no dedicated no-key
    // constructor; an empty string is the documented no-auth form (the
    // local endpoint ignores the empty bearer). A remote endpoint that
    // truly needs a key but got none will surface its own 401.
    let token = resolved
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("authorization"))
        .map(|auth| {
            auth.value
                .strip_prefix("Bearer ")
                .or_else(|| auth.value.strip_prefix("bearer "))
                .unwrap_or(&auth.value)
                .trim()
                .to_string()
        })
        .unwrap_or_default();

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
        // Default never-draining gate; the registry swaps in the daemon's
        // shared gate via `Model::with_shutdown_gate` for worker models.
        gate: crate::daemon::shutdown::ShutdownSignal::new(),
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
///
/// Safe for the Chat Completions variant (reasoning is never replayed
/// there). NOT safe as-is for a native Anthropic variant: stripping the
/// *latest* assistant turn's thinking — or any turn that pairs thinking
/// with `tool_use` — 400s the Messages API. Make this position-aware
/// before wiring native Anthropic. See `miscellaneous.md` §10b.
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

/// Assemble the full as-sent outbound request body. This is the exact
/// tuple rig receives — `(model, provider, params, system, tools,
/// history, prompt)` — serialized to JSON. rig does not expose its own
/// serialized HTTP body (verified via kcl `rig-core`), so this faithful
/// reconstruction is the canonical capture for both the
/// `--debug-last-message` dump and the always-on inference-request store
/// (session-log-export Part A). It is built *after* the driver's upstream
/// `redact::scrub`, so it is the post-redaction, as-sent form — no second
/// redaction pass is ever applied on top.
fn assembled_request(
    model_id: &str,
    provider: &str,
    system: &str,
    history: &[Message],
    prompt: &Message,
    tools: &[ToolDefinition],
    params: &ModelParams,
) -> serde_json::Value {
    json!({
        "model": model_id,
        "provider": provider,
        "system": system,
        "tools": tools,
        "params": {
            "temperature": params.temperature,
            "max_tokens": params.max_tokens,
            "tools_required": params.tools_required,
        },
        "history": history,
        "prompt": prompt,
    })
}

/// Write a pre-assembled request body to `path` for `--debug-last-message`.
/// Best-effort — any error is traced but never propagated, because losing
/// a debug dump must not break a live turn.
fn write_dump(path: &Path, body: &serde_json::Value) {
    let pretty = match serde_json::to_string_pretty(body) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::providers::ProviderEntry;

    #[test]
    fn build_openai_model_succeeds_for_keyless_provider() {
        // Mirror the keyless resolver test
        // (`providers::models_fetch::non_copilot_provider_without_auth_resolves_unauthenticated`):
        // a fully-local OpenAI-compatible endpoint (LM Studio) has no
        // Authorization header. `build_openai_model` must treat absence
        // as "no API key" and build the client unauthenticated rather
        // than erroring with "no Authorization header after resolution".
        let entry = ProviderEntry {
            url: "http://localhost:1234/v1".into(),
            headers: vec![],
            ..ProviderEntry::default()
        };
        let model = build_openai_model("lmstudio", &entry, "local-model")
            .expect("keyless provider must build");
        assert_eq!(model.model_id(), "local-model");
    }

    /// New-request gate after drain (`daemon-graceful-drain-shutdown.md`):
    /// once the daemon's shared gate reports draining, the inference-
    /// dispatch chokepoint refuses *new* provider requests with the
    /// `InferenceGated` sentinel — before any client work. Asserted on both
    /// dispatch entry points (`text_completion` and `complete_captured`).
    #[tokio::test]
    async fn draining_gate_refuses_new_requests() {
        use crate::daemon::shutdown::ShutdownSignal;

        let entry = ProviderEntry {
            url: "http://localhost:1234/v1".into(),
            headers: vec![],
            ..ProviderEntry::default()
        };
        let gate = ShutdownSignal::new();
        let model = build_openai_model("lmstudio", &entry, "local-model")
            .expect("keyless provider must build")
            .with_shutdown_gate(gate.clone());

        // Before drain: the gate permits dispatch (we don't actually round-
        // trip — no server — but the gate must not be the thing refusing).
        assert!(!gate.is_draining());

        // Begin draining: the chokepoint now refuses both entry points.
        assert!(gate.begin_drain());

        let err = model
            .text_completion("hi")
            .await
            .expect_err("text_completion must be gated while draining");
        assert!(
            crate::engine::model::is_gated(&err),
            "text_completion refusal must carry the InferenceGated sentinel, got: {err:#}"
        );

        let (tx, _rx) = mpsc::channel(8);
        let err = model
            .complete_captured(
                "system",
                &[],
                Message::user("hi"),
                &[],
                ModelParams::default(),
                "Build",
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect_err("complete_captured must be gated while draining");
        assert!(
            crate::engine::model::is_gated(&err),
            "complete_captured refusal must carry the InferenceGated sentinel, got: {err:#}"
        );
    }

    /// A trailing `Message::System` (the live instructions-file diff
    /// injection, `instructions-file-live-diff.md`) appended to history
    /// must show up in the captured/as-sent request body's `history`
    /// array, after the prior turns. This is the shape the
    /// `inference_requests` store records, so the audit acceptance check
    /// ("captured body contains a trailing system message with the diff")
    /// holds.
    #[test]
    fn assembled_request_carries_trailing_system_injection() {
        let history = vec![
            Message::user("hello"),
            Message::System {
                content: "Your instructions file (`/p/AGENTS.md`) changed since this \
                          conversation began. Apply the updated version:\n- old\n+ new"
                    .to_string(),
            },
        ];
        let prompt = Message::user("do the thing");
        let body = assembled_request(
            "m",
            "openai-compatible",
            "SYSTEM PROMPT",
            &history,
            &prompt,
            &[],
            &ModelParams::default(),
        );
        // The cached system prefix is untouched — the injection is append-
        // only, riding in `history`, never in `system`.
        assert_eq!(body["system"], "SYSTEM PROMPT");
        let hist = body["history"].as_array().expect("history is an array");
        // The system injection is the LAST history entry (end of history),
        // and serializes with the system role.
        let last = hist.last().expect("non-empty history");
        assert_eq!(last["role"], "system", "got {last}");
        let rendered = serde_json::to_string(last).unwrap();
        assert!(rendered.contains("changed since this conversation began"));
        assert!(rendered.contains("- old"));
        assert!(rendered.contains("+ new"));
    }
}
