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
    /// Per-model prompt-cache override. When set, takes precedence over
    /// the provider-level [`ProviderEntry::cache`] for the cache-cold
    /// predicate (GOALS §10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfig>,
    /// Free-form metadata the `/models` endpoint returned but we don't
    /// model explicitly. Preserved verbatim so re-saving doesn't drop
    /// fields the user (or provider) cares about.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

fn is_false(b: &bool) -> bool {
    !*b
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
                models: vec![ModelEntry {
                    id: "claude-opus-4-7".into(),
                    name: Some("Claude Opus 4.7".into()),
                    thinking_modes: vec![ThinkingMode::Off, ThinkingMode::High],
                    context_length: None,
                    favorite: false,
                    cache: None,
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
            cache: Some(CacheConfig {
                mode: CacheMode::None,
                ttl_secs: 300,
            }),
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
}
