//! User-configured provider entries inside `config.json`.
//!
//! Schema (under top-level key `providers`, an object keyed by provider id):
//!
//! ```json
//! {
//!   "providers": {
//!     "opencode-zen": {
//!       "name": "OpenCode Zen",
//!       "url": "https://opencode.ai/zen/v1",
//!       "headers": [
//!         { "name": "Authorization", "value": "Bearer $OPENCODE_ZEN_TOKEN" }
//!       ],
//!       "models_fetched_at": "2026-05-26T12:34:56Z",
//!       "favorite": true,
//!       "models": [
//!         {
//!           "id": "claude-opus-4-7",
//!           "name": "Claude Opus 4.7 (via opencode)",
//!           "thinking_modes": ["off", "low", "medium", "high"],
//!           "inputs": { "images": true }
//!         }
//!       ]
//!     }
//!   },
//!   "on_unlisted_models_fetch": "ask"
//! }
//! ```
//!
//! `name`, `models_fetched_at`, `favorite`, `models`, `thinking_modes`,
//! and `inputs` are all optional. Headers carry `$VAR` references that
//! [`crate::envref`] expands at use-time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Top-level config slice that owns the `providers` map and the
/// fetch-policy field. Marshalled in/out of the raw `Value` of
/// `config.json` so we never destroy fields cockpit doesn't know about.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_unlisted_models_fetch: Option<OnUnlistedModelsFetch>,
    /// Currently selected model. Written by `/model` and read by the
    /// launch header + status line. Absent when nothing has been picked
    /// yet (e.g. a freshly-scaffolded config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_model: Option<ActiveModelRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveModelRef {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_mode: Option<ThinkingMode>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// Display name. Omit to fall back to the id key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Base URL. The `/models` endpoint is `{url}/models`; chat lives at
    /// `{url}/chat/completions`. Stored without a trailing slash.
    pub url: String,

    /// HTTP headers to send on every request. Values may contain `$VAR`
    /// env references.
    #[serde(default)]
    pub headers: Vec<HeaderSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_fetched_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorite: Option<bool>,

    /// Optional pointer to a credential record under
    /// `~/.local/state/cockpit/credentials.json`. The credentials file
    /// stores the raw secret; this field just names the record so the
    /// resolver knows which one to attach. Absent on env-var-only
    /// providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,

    /// Auth kind. Mostly informational for the UI — actual auth is
    /// driven by `headers` + `credential_ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthKind>,

    /// Prompt-cache behavior for this provider. Drives the cache-cold
    /// predicate that gates auto-prune (GOALS §10 / `plan.md` T6.f). A
    /// per-model `cache` overrides this. Defaults to `none` because we
    /// do **not** autodetect — explicit config only.
    #[serde(default)]
    pub cache: CacheConfig,

    /// Delegation-shrink behavior for this provider (GOALS §10 /
    /// `prompts/compact-after-delegation.md`). Drives the parent-context
    /// shrink that hides cache-cold cost across a sub-agent delegation. A
    /// per-model `shrink` overrides this. Lives in the same per-model
    /// layer as `cache` so a future per-model context-usage threshold is
    /// an additive field, not a refactor.
    #[serde(default)]
    pub shrink: ShrinkConfig,

    /// Cached model list. Populated by `/fetch-models` (or the wizard).
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

/// Prompt-cache configuration. Set per-provider on [`ProviderEntry`] and
/// optionally overridden per-model on [`ModelEntry`]. Used only by the
/// cache-cold predicate (GOALS §10) that decides whether auto-prune may
/// fire for free. We **never** autodetect mode — absence means `none`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheConfig {
    #[serde(default)]
    pub mode: CacheMode,
    /// Seconds a cached prefix survives between sends. After this much
    /// idle time the provider has dropped the cache, so pruning is free.
    /// Default 300 (5 min). Only meaningful when `mode != none`.
    #[serde(default = "default_cache_ttl_secs")]
    pub ttl_secs: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            mode: CacheMode::default(),
            ttl_secs: default_cache_ttl_secs(),
        }
    }
}

fn default_cache_ttl_secs() -> u64 {
    300
}

/// Delegation-shrink configuration. Set per-provider on [`ProviderEntry`]
/// and optionally overridden per-model on [`ModelEntry`]. Controls how the
/// parent context is shrunk while a sub-agent runs so the parent resumes
/// from the cheapest correct context when the cache went cold
/// (`prompts/compact-after-delegation.md`). The TTL itself is reused from
/// [`CacheConfig::ttl_secs`] — this layer adds only the *strategy* and the
/// *margin* (lead time to finish the shrink before the cache would
/// expire).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShrinkConfig {
    /// Which shrink to run on a cold-at-return delegation. Default
    /// `prune` (lossless, cheap, sync); `compact` opts into LLM
    /// summarization (heavier, lossier, saves more).
    #[serde(default)]
    pub strategy: ShrinkStrategy,
    /// Seconds of lead time before the cache TTL elapses at which the
    /// lazy (cache-capable) shrink is kicked off, so it finishes before
    /// the prefix would expire. The lazy trigger fires at
    /// `ttl_secs - margin_secs`. Ignored for no-cache providers (they
    /// shrink eagerly at delegation start). Default 30s.
    #[serde(default = "default_shrink_margin_secs")]
    pub margin_secs: u64,
}

impl Default for ShrinkConfig {
    fn default() -> Self {
        Self {
            strategy: ShrinkStrategy::default(),
            margin_secs: default_shrink_margin_secs(),
        }
    }
}

fn default_shrink_margin_secs() -> u64 {
    30
}

/// The parent-context shrink strategy used across a sub-agent delegation.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ShrinkStrategy {
    /// Lossless snapshot-dedup via the existing prune action. Cheap,
    /// synchronous, low quality loss — the default (priority #1).
    #[default]
    Prune,
    /// LLM summarization of the parent context (reuses the `/compact`
    /// brief machinery). Heavier and lossier, saves more tokens.
    Compact,
}

/// How a provider caches the prompt prefix. `None` (the default) means
/// no caching — pruning never costs a cache bust there.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    /// No prompt cache (local Ollama / llama.cpp / raw vLLM / most
    /// OpenRouter routes). Pruning is always free.
    #[default]
    None,
    /// Provider caches a (possibly implicit) prefix subject to a TTL
    /// (Anthropic ephemeral, OpenAI automatic prefix caching, Gemini).
    Ephemeral,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeaderSpec {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    /// API key carried by an explicit header (Authorization / x-api-key / etc.).
    ApiKey,
    /// OAuth device-code flow. Not yet implemented in the TUI — the
    /// `cockpit providers login codex` command will drive it.
    DeviceFlow,
    /// No authentication (e.g. a self-hosted ollama server).
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_modes: Vec<ThinkingMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Inputs>,
    /// Maximum tokens this model accepts in a request (context window).
    /// Optional because providers vary on whether `/models` reports it
    /// — populated by `/fetch-models` when the upstream includes it
    /// (OpenRouter, llamafile), set manually otherwise. Drives the
    /// chrome's `N% context (max Mk)` indicator (omitted when `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    /// Toggled by `/favorite`. The `/model` picker pins favorites at
    /// the top of the list.
    #[serde(default, skip_serializing_if = "is_false")]
    pub favorite: bool,
    /// True for entries added by hand on the provider Edit page (for
    /// providers without a `/models` endpoint). Manual entries survive a
    /// `/models` refetch via [`merge_fetched_models`] and win on an id
    /// collision. Defaults to `false`, so configs written before this
    /// field existed load as non-manual (fetched).
    #[serde(default, skip_serializing_if = "is_false")]
    pub manual: bool,
    /// Per-model prompt-cache override. When set, takes precedence over
    /// the provider-level [`ProviderEntry::cache`] for the cache-cold
    /// predicate (GOALS §10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfig>,
    /// Per-model delegation-shrink override. When set, takes precedence
    /// over the provider-level [`ProviderEntry::shrink`]
    /// (`prompts/compact-after-delegation.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shrink: Option<ShrinkConfig>,
    /// Free-form metadata the `/models` endpoint returned but we don't
    /// model explicitly. Preserved verbatim so re-saving doesn't drop
    /// fields the user (or provider) cares about.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Merge a freshly-fetched `/models` list into an existing model list,
/// preserving manually-added entries.
///
/// The fetched list replaces the previously-fetched portion (non-manual
/// entries) wholesale, while every manual entry is retained. Dedupe is by
/// `id`: a manual entry is authoritative, so if a fetch returns an id that
/// matches a manual entry the manual one is kept and the fetched duplicate
/// is dropped (no double row). Manual entries are listed first so they
/// stay stable across refetches; the fetched entries follow in upstream
/// order.
///
/// Used by both the per-provider refetch and the all-providers fetch so
/// the merge logic lives in exactly one place.
pub fn merge_fetched_models(existing: &[ModelEntry], fetched: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let manual: Vec<ModelEntry> = existing.iter().filter(|m| m.manual).cloned().collect();
    let mut merged = manual;
    for mut m in fetched {
        if merged.iter().any(|e| e.id == m.id) {
            // Manual entry wins on id collision — drop the fetched dup.
            continue;
        }
        // Defensive: a fetched entry is never manual.
        m.manual = false;
        merged.push(m);
    }
    merged
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode {
    Off,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inputs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OnUnlistedModelsFetch {
    Ask,
    Keep,
    Remove,
}

impl ProviderEntry {
    /// Display label: the user-set `name`, falling back to the id key.
    pub fn label<'a>(&'a self, id: &'a str) -> &'a str {
        self.name.as_deref().unwrap_or(id)
    }
}

impl ProvidersConfig {
    /// Resolve the effective prompt-cache config for `(provider, model)`:
    /// the model-level override if present, else the provider-level
    /// config, else the default (`none`). Used by the cache-cold
    /// predicate (GOALS §10).
    pub fn resolve_cache(&self, provider: &str, model: &str) -> CacheConfig {
        let Some(entry) = self.providers.get(provider) else {
            return CacheConfig::default();
        };
        entry
            .models
            .iter()
            .find(|m| m.id == model)
            .and_then(|m| m.cache.clone())
            .unwrap_or_else(|| entry.cache.clone())
    }

    /// Resolve the effective delegation-shrink config for
    /// `(provider, model)`: the model-level override if present, else the
    /// provider-level config, else the default (`prune`, 30s margin).
    /// Used by the delegation-shrink decision
    /// (`prompts/compact-after-delegation.md`).
    pub fn resolve_shrink(&self, provider: &str, model: &str) -> ShrinkConfig {
        let Some(entry) = self.providers.get(provider) else {
            return ShrinkConfig::default();
        };
        entry
            .models
            .iter()
            .find(|m| m.id == model)
            .and_then(|m| m.shrink.clone())
            .unwrap_or_else(|| entry.shrink.clone())
    }
}

/// Read+write a `config.json` while preserving the fields cockpit
/// doesn't model. The on-disk JSON is parsed into a `Value`, then the
/// `providers` and `on_unlisted_models_fetch` keys are pulled into
/// [`ProvidersConfig`] for typed editing. [`write`] folds the typed
/// view back into the raw `Value` and re-serializes.
pub struct ConfigDoc {
    pub path: PathBuf,
    raw: Value,
}

impl ConfigDoc {
    pub fn load(path: &Path) -> Result<Self> {
        let raw_str = if path.exists() {
            std::fs::read_to_string(path)
                .with_context(|| format!("reading config.json at {}", path.display()))?
        } else {
            "{}".to_string()
        };
        let raw: Value = if raw_str.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_str(&raw_str)
                .with_context(|| format!("parsing config.json at {}", path.display()))?
        };
        let raw = match raw {
            Value::Object(_) => raw,
            other => {
                anyhow::bail!("expected config.json root to be an object, found {other:?}")
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            raw,
        })
    }

    /// Extract the typed view of `providers` + `on_unlisted_models_fetch`.
    pub fn providers(&self) -> ProvidersConfig {
        let mut cfg = ProvidersConfig::default();
        if let Some(map) = self.raw.get("providers").and_then(Value::as_object) {
            for (id, v) in map {
                match serde_json::from_value::<ProviderEntry>(v.clone()) {
                    Ok(entry) => {
                        cfg.providers.insert(id.clone(), entry);
                    }
                    Err(e) => {
                        tracing::warn!(provider = %id, error = %e, "skipping malformed provider entry");
                    }
                }
            }
        }
        if let Some(s) = self
            .raw
            .get("on_unlisted_models_fetch")
            .and_then(Value::as_str)
            && let Ok(parsed) =
                serde_json::from_value::<OnUnlistedModelsFetch>(Value::String(s.to_string()))
        {
            cfg.on_unlisted_models_fetch = Some(parsed);
        }
        if let Some(v) = self.raw.get("active_model")
            && let Ok(parsed) = serde_json::from_value::<ActiveModelRef>(v.clone())
        {
            cfg.active_model = Some(parsed);
        }
        cfg
    }

    /// Replace the typed providers slice and persist to disk.
    pub fn write(&mut self, cfg: &ProvidersConfig) -> Result<()> {
        let obj = self.raw.as_object_mut().expect("root is an object");
        let providers_value =
            serde_json::to_value(&cfg.providers).context("serializing providers")?;
        obj.insert("providers".to_string(), providers_value);
        match cfg.on_unlisted_models_fetch {
            Some(v) => {
                let s = serde_json::to_value(v).context("serializing on_unlisted_models_fetch")?;
                obj.insert("on_unlisted_models_fetch".to_string(), s);
            }
            None => {
                obj.remove("on_unlisted_models_fetch");
            }
        }
        match &cfg.active_model {
            Some(active) => {
                let s = serde_json::to_value(active).context("serializing active_model")?;
                obj.insert("active_model".to_string(), s);
            }
            None => {
                obj.remove("active_model");
            }
        }
        let pretty = serde_json::to_string_pretty(&self.raw).context("serializing config.json")?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, format!("{pretty}\n"))
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trips_a_provider_entry() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ConfigDoc::load(&path).unwrap();
        let mut cfg = ProvidersConfig::default();
        cfg.providers.insert(
            "opencode-zen".to_string(),
            ProviderEntry {
                name: Some("OpenCode Zen".into()),
                url: "https://opencode.ai/zen/v1".into(),
                headers: vec![HeaderSpec {
                    name: "Authorization".into(),
                    value: "Bearer $OPENCODE_ZEN_TOKEN".into(),
                }],
                models_fetched_at: None,
                favorite: Some(true),
                credential_ref: None,
                auth: Some(AuthKind::ApiKey),
                cache: CacheConfig::default(),
                shrink: ShrinkConfig::default(),
                models: vec![ModelEntry {
                    id: "claude-opus-4-7".into(),
                    name: Some("Claude Opus 4.7".into()),
                    thinking_modes: vec![ThinkingMode::Off, ThinkingMode::High],
                    context_length: None,
                    favorite: false,
                    manual: false,
                    cache: None,
                    shrink: None,
                    inputs: Some(Inputs {
                        images: Some(true),
                        video: None,
                        audio: None,
                    }),
                    extra: Default::default(),
                }],
            },
        );
        cfg.on_unlisted_models_fetch = Some(OnUnlistedModelsFetch::Ask);
        doc.write(&cfg).unwrap();

        let doc2 = ConfigDoc::load(&path).unwrap();
        let cfg2 = doc2.providers();
        let entry = cfg2.providers.get("opencode-zen").unwrap();
        assert_eq!(entry.url, "https://opencode.ai/zen/v1");
        assert_eq!(entry.headers.len(), 1);
        assert_eq!(entry.favorite, Some(true));
        assert_eq!(entry.models[0].id, "claude-opus-4-7");
        assert_eq!(
            cfg2.on_unlisted_models_fetch,
            Some(OnUnlistedModelsFetch::Ask)
        );
    }

    #[test]
    fn preserves_unknown_fields() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"providers":{},"agents":{"foo":"bar"},"misc":[1,2,3]}"#,
        )
        .unwrap();
        let mut doc = ConfigDoc::load(&path).unwrap();
        doc.write(&ProvidersConfig::default()).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("\"agents\""));
        assert!(on_disk.contains("\"misc\""));
    }

    #[test]
    fn skips_malformed_provider_entry_warning_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"providers":{"good":{"url":"https://x"},"bad":42}}"#,
        )
        .unwrap();
        let doc = ConfigDoc::load(&path).unwrap();
        let cfg = doc.providers();
        assert!(cfg.providers.contains_key("good"));
        assert!(!cfg.providers.contains_key("bad"));
    }

    #[test]
    fn label_falls_back_to_id() {
        let entry = ProviderEntry::default();
        assert_eq!(entry.label("my-id"), "my-id");
        let mut entry = ProviderEntry::default();
        entry.name = Some("Pretty".into());
        assert_eq!(entry.label("ignored"), "Pretty");
    }

    #[test]
    fn cache_defaults_to_none() {
        let entry = ProviderEntry::default();
        assert_eq!(entry.cache.mode, CacheMode::None);
        assert_eq!(entry.cache.ttl_secs, 300);
    }

    #[test]
    fn resolve_cache_prefers_model_override() {
        let mut cfg = ProvidersConfig::default();
        let mut entry = ProviderEntry {
            url: "https://x".into(),
            cache: CacheConfig {
                mode: CacheMode::Ephemeral,
                ttl_secs: 600,
            },
            ..ProviderEntry::default()
        };
        entry.models.push(ModelEntry {
            id: "fast".into(),
            name: None,
            thinking_modes: vec![],
            context_length: None,
            favorite: false,
            manual: false,
            cache: Some(CacheConfig {
                mode: CacheMode::None,
                ttl_secs: 300,
            }),
            shrink: None,
            inputs: None,
            extra: Default::default(),
        });
        cfg.providers.insert("p".into(), entry);

        // Model with an override wins.
        let m = cfg.resolve_cache("p", "fast");
        assert_eq!(m.mode, CacheMode::None);
        // Model without an override inherits the provider config.
        let p = cfg.resolve_cache("p", "other");
        assert_eq!(p.mode, CacheMode::Ephemeral);
        assert_eq!(p.ttl_secs, 600);
        // Unknown provider → default (none).
        assert_eq!(cfg.resolve_cache("nope", "x").mode, CacheMode::None);
    }

    /// Minimal `ModelEntry` for the merge tests.
    fn model(id: &str, manual: bool) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            name: None,
            thinking_modes: vec![],
            inputs: None,
            context_length: None,
            favorite: false,
            manual,
            cache: None,
            shrink: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn manual_field_defaults_false_when_absent() {
        // A model row written before the `manual` field existed must
        // load as non-manual.
        let m: ModelEntry = serde_json::from_str(r#"{"id":"legacy"}"#).unwrap();
        assert!(!m.manual);
        // And the field is skipped when serializing a non-manual entry.
        let json = serde_json::to_string(&model("x", false)).unwrap();
        assert!(!json.contains("manual"));
        let json = serde_json::to_string(&model("x", true)).unwrap();
        assert!(json.contains("\"manual\":true"));
    }

    #[test]
    fn merge_retains_manual_entry_across_refetch() {
        let existing = vec![model("fetched-old", false), model("hand-added", true)];
        // A refetch returns a fresh fetched list that no longer includes
        // the old fetched id and never knew about the manual one.
        let fetched = vec![model("fetched-new", false)];
        let merged = merge_fetched_models(&existing, fetched);

        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        // Manual entry survives; stale fetched entry is gone; new fetched
        // entry is present.
        assert!(ids.contains(&"hand-added"));
        assert!(ids.contains(&"fetched-new"));
        assert!(!ids.contains(&"fetched-old"));
        // The manual entry keeps its manual flag.
        assert!(merged.iter().find(|m| m.id == "hand-added").unwrap().manual);
    }

    #[test]
    fn merge_manual_wins_on_id_collision_no_duplicate() {
        let existing = vec![model("shared", true)];
        // The refetch returns an id that collides with the manual entry.
        let fetched = vec![model("shared", false), model("other", false)];
        let merged = merge_fetched_models(&existing, fetched);

        // Exactly one `shared` row, and it's the manual one.
        let shared: Vec<&ModelEntry> = merged.iter().filter(|m| m.id == "shared").collect();
        assert_eq!(shared.len(), 1, "manual entry must dedupe the fetched dup");
        assert!(shared[0].manual);
        // The non-colliding fetched entry is still added.
        assert!(merged.iter().any(|m| m.id == "other" && !m.manual));
    }
}
