//! `GET {url}/models` against an OpenAI-compatible endpoint.
//!
//! Returns either:
//!   - `Ok(Some(entries))` — a parsed list (envelope or bare-array).
//!   - `Ok(None)` — the endpoint replied 404, so the provider doesn't
//!     ship one. The caller treats this as a no-op (the `/fetch-models`
//!     workflow leaves the configured model list alone).
//!   - `Err(...)` — any other failure surfaces, including 401 with a
//!     hint to fix the credential.
//!
//! The body parser is tolerant: it accepts the canonical
//! `{"data": [...]}` envelope and the bare-array shape some compat
//! gateways emit. Entries missing an `id` are dropped rather than
//! erroring (matches mixer-rs's behavior; see
//! `mixer-rs/src/providers/common/models_list.rs`).

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::StatusCode;
use serde_json::{Map, Value};

use crate::config::providers::{HeaderSpec, ModelEntry, ProviderEntry, ThinkingMode};
use crate::envref;

const COPILOT_TOKEN_ENV_VARS: [&str; 3] = ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];
const COPILOT_DIRECT_API_TOKEN_ENV: &str = "GITHUB_COPILOT_API_TOKEN";
const COPILOT_API_URL_ENV: &str = "COPILOT_API_URL";

/// Resolved view of a `HeaderSpec` after `$VAR` expansion.
#[derive(Debug, Clone)]
pub struct ResolvedHeader {
    pub name: String,
    pub value: String,
}

/// Fully resolved provider request inputs after applying `$VAR`
/// expansion plus GitHub Copilot's documented token fallbacks.
#[derive(Debug, Clone)]
pub struct ResolvedRequest {
    pub base_url: String,
    pub headers: Vec<ResolvedHeader>,
}

/// Apply `$VAR` resolution to every header, collecting any missing-env
/// references into one list. Caller decides whether to abort or warn.
pub fn resolve_headers(headers: &[HeaderSpec]) -> (Vec<ResolvedHeader>, Vec<String>) {
    let mut out = Vec::with_capacity(headers.len());
    let mut missing: Vec<String> = Vec::new();
    for h in headers {
        let r = envref::resolve(&h.value);
        push_missing(&mut missing, &r.missing);
        out.push(ResolvedHeader {
            name: h.name.clone(),
            value: r.value,
        });
    }
    (out, missing)
}

/// Resolve a provider entry into concrete request inputs. For most
/// providers this is just `$VAR` expansion over `headers`; GitHub
/// Copilot also accepts documented token sources in the same priority
/// order as GitHub's SDK docs.
pub fn resolve_provider_request(
    provider_id: &str,
    entry: &ProviderEntry,
) -> Result<ResolvedRequest> {
    let is_copilot = is_github_copilot_provider(provider_id, entry);
    let mut headers: Vec<ResolvedHeader> = Vec::with_capacity(entry.headers.len() + 1);
    let mut missing_other: Vec<String> = Vec::new();
    let mut auth_header: Option<ResolvedHeader> = None;
    let mut auth_missing: Vec<String> = Vec::new();

    for h in &entry.headers {
        let resolved = envref::resolve(&h.value);
        if h.name.eq_ignore_ascii_case("authorization") {
            if resolved.has_missing() {
                push_missing(&mut auth_missing, &resolved.missing);
            } else {
                auth_header = Some(ResolvedHeader {
                    name: h.name.clone(),
                    value: resolved.value,
                });
            }
            continue;
        }

        push_missing(&mut missing_other, &resolved.missing);
        headers.push(ResolvedHeader {
            name: h.name.clone(),
            value: resolved.value,
        });
    }

    if !missing_other.is_empty() {
        anyhow::bail!(
            "provider `{provider_id}` references unset env var(s): {}",
            missing_other.join(", ")
        );
    }

    if let Some(auth) = auth_header {
        headers.push(auth);
    } else if is_copilot {
        match resolve_copilot_token()? {
            Some(token) => headers.push(ResolvedHeader {
                name: "Authorization".to_string(),
                value: format!("Bearer {token}"),
            }),
            None => {
                let configured = if auth_missing.is_empty() {
                    String::new()
                } else {
                    format!(
                        " Configured Authorization refs were unset: {}.",
                        auth_missing.join(", ")
                    )
                };
                anyhow::bail!(
                    "GitHub Copilot requires an official GitHub token. \
                     Export one of COPILOT_GITHUB_TOKEN, GH_TOKEN, or GITHUB_TOKEN; \
                     or use the documented direct API pair \
                     GITHUB_COPILOT_API_TOKEN + COPILOT_API_URL.{configured}"
                );
            }
        }
    } else if !auth_missing.is_empty() {
        anyhow::bail!(
            "Authorization for provider `{provider_id}` references unset env var(s): {}",
            auth_missing.join(", ")
        );
    }
    // No Authorization header at all (and not Copilot): fetch
    // unauthenticated. Fully-local endpoints like LM Studio don't
    // require auth; a provider that actually needs it surfaces a clear
    // 401 from `fetch_models`.

    Ok(ResolvedRequest {
        base_url: resolve_provider_base_url(provider_id, entry),
        headers,
    })
}

/// Outcome of [`fetch_models`].
#[derive(Debug)]
pub enum FetchOutcome {
    /// The endpoint returned a model list.
    Models(Vec<ModelEntry>),
    /// The provider doesn't expose `/models` (404).
    Unsupported,
}

pub async fn fetch_models(
    base_url: &str,
    headers: &[ResolvedHeader],
    timeout: Option<Duration>,
) -> Result<FetchOutcome> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));

    let mut builder = reqwest::Client::builder();
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    let client = builder.build().context("building reqwest client")?;

    let mut req = client.get(&url).header("Accept", "application/json");
    for h in headers {
        req = req.header(&h.name, &h.value);
    }

    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == StatusCode::NOT_FOUND {
        return Ok(FetchOutcome::Unsupported);
    }
    if status == StatusCode::UNAUTHORIZED {
        anyhow::bail!("{url} returned 401 — credentials rejected. Verify the API key and headers.");
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(256).collect();
        anyhow::bail!("{url} returned {status}: {snippet}");
    }

    let body = resp.text().await.context("reading /models response body")?;
    let entries = parse_models_body(&body)?;
    Ok(FetchOutcome::Models(entries))
}

pub fn parse_models_body(body: &str) -> Result<Vec<ModelEntry>> {
    let parsed: Value =
        serde_json::from_str(body).with_context(|| format!("parsing /models response: {body}"))?;
    let entries: Vec<Value> = match parsed {
        Value::Array(xs) => xs,
        Value::Object(mut m) => match m.remove("data") {
            Some(Value::Array(xs)) => xs,
            _ => return Err(anyhow!("models response lacks a `data` array")),
        },
        _ => return Err(anyhow!("unexpected models response root")),
    };

    Ok(entries
        .into_iter()
        .filter_map(|raw| {
            let obj = raw.as_object()?;
            let id = obj.get("id").and_then(Value::as_str)?.to_string();

            let name = obj
                .get("display_name")
                .or_else(|| obj.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);

            let thinking_modes = obj
                .get("thinking_modes")
                .and_then(Value::as_array)
                .map(|xs| {
                    xs.iter()
                        .filter_map(|v| match v.as_str()? {
                            "off" => Some(ThinkingMode::Off),
                            "low" => Some(ThinkingMode::Low),
                            "medium" => Some(ThinkingMode::Medium),
                            "high" => Some(ThinkingMode::High),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();

            let inputs = obj.get("inputs").and_then(|v| {
                serde_json::from_value::<crate::config::providers::Inputs>(v.clone()).ok()
            });

            // Stash every remaining field into `extra` so re-saving
            // doesn't lose provider-specific metadata.
            let mut extra = Map::new();
            for (k, v) in obj {
                if matches!(
                    k.as_str(),
                    "id" | "name"
                        | "display_name"
                        | "thinking_modes"
                        | "inputs"
                        | "context_length"
                        | "max_tokens"
                ) {
                    continue;
                }
                extra.insert(k.clone(), v.clone());
            }

            // Several OpenAI-compat providers (OpenRouter, llamafile,
            // some self-hosted shims) include `context_length`. Pick
            // it up here so `/fetch-models` populates the field
            // automatically. `max_tokens` is the alt name a few use.
            let context_length = obj
                .get("context_length")
                .or_else(|| obj.get("max_tokens"))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok());

            Some(ModelEntry {
                id,
                name,
                thinking_modes,
                inputs,
                context_length,
                favorite: false,
                cache: None,
                extra,
            })
        })
        .collect())
}

fn is_github_copilot_provider(provider_id: &str, entry: &ProviderEntry) -> bool {
    provider_id.eq_ignore_ascii_case("copilot")
        || entry.credential_ref.as_deref() == Some("copilot")
        || entry.url.contains("githubcopilot.com")
}

fn resolve_provider_base_url(provider_id: &str, entry: &ProviderEntry) -> String {
    if is_github_copilot_provider(provider_id, entry)
        && let Some(url) = env_var_nonempty(COPILOT_API_URL_ENV)
    {
        return url.trim_end_matches('/').to_string();
    }
    entry.url.trim_end_matches('/').to_string()
}

fn resolve_copilot_token() -> Result<Option<String>> {
    for name in COPILOT_TOKEN_ENV_VARS {
        if let Some(token) = env_var_nonempty(name) {
            validate_copilot_token(name, &token)?;
            return Ok(Some(token));
        }
    }

    if let Some(token) = env_var_nonempty(COPILOT_DIRECT_API_TOKEN_ENV) {
        validate_copilot_token(COPILOT_DIRECT_API_TOKEN_ENV, &token)?;
        return Ok(Some(token));
    }

    Ok(None)
}

fn validate_copilot_token(source: &str, token: &str) -> Result<()> {
    if token.starts_with("ghp_") {
        anyhow::bail!(
            "{source} looks like a classic GitHub PAT (`ghp_...`). \
             GitHub Copilot expects a GitHub OAuth token (`gho_`/`ghu_`), \
             a GitHub App installation token, or a fine-grained PAT \
             (`github_pat_...`) issued to an account with Copilot access."
        );
    }
    Ok(())
}

fn env_var_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn push_missing(dst: &mut Vec<String>, src: &[String]) {
    for name in src {
        if !dst.iter().any(|existing| existing == name) {
            dst.push(name.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Cargo runs tests in parallel by default. Several tests below
    /// mutate process-wide env vars (`COPILOT_GITHUB_TOKEN` and friends)
    /// to exercise resolver fallbacks, so they must serialize against
    /// one another to avoid spurious failures.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_copilot_env() {
        unsafe {
            std::env::remove_var("COPILOT_GITHUB_TOKEN");
            std::env::remove_var("GH_TOKEN");
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("GITHUB_COPILOT_API_TOKEN");
            std::env::remove_var("COPILOT_API_URL");
        }
    }

    #[test]
    fn parses_canonical_envelope() {
        let body = r#"{
            "object":"list",
            "data":[
                {"id":"gpt-5.2","object":"model","created":1},
                {"id":"gpt-5.2-mini","object":"model","created":2}
            ]
        }"#;
        let entries = parse_models_body(body).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "gpt-5.2");
        assert!(entries[0].extra.contains_key("created"));
    }

    #[test]
    fn parses_bare_array() {
        let body = r#"[{"id":"foo"},{"id":"bar"}]"#;
        let entries = parse_models_body(body).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn skips_entries_without_id() {
        let body = r#"{"data":[{"id":"ok"},{"object":"model"}]}"#;
        let entries = parse_models_body(body).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "ok");
    }

    #[test]
    fn captures_thinking_modes_and_inputs() {
        let body = r#"{"data":[{
            "id":"x",
            "thinking_modes":["off","high"],
            "inputs":{"images":true}
        }]}"#;
        let entries = parse_models_body(body).unwrap();
        assert_eq!(entries[0].thinking_modes.len(), 2);
        assert_eq!(entries[0].inputs.as_ref().unwrap().images, Some(true));
    }

    #[test]
    fn resolve_headers_collects_missing_once() {
        let h = vec![
            HeaderSpec {
                name: "Authorization".into(),
                value: "Bearer $NONEXISTENT_VAR_123".into(),
            },
            HeaderSpec {
                name: "x-second".into(),
                value: "$NONEXISTENT_VAR_123".into(),
            },
        ];
        let (resolved, missing) = resolve_headers(&h);
        assert_eq!(resolved.len(), 2);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], "NONEXISTENT_VAR_123");
    }

    #[test]
    fn copilot_falls_back_to_gh_token_when_default_header_var_is_missing() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_copilot_env();
        let entry = ProviderEntry {
            url: "https://api.githubcopilot.com".into(),
            headers: vec![HeaderSpec {
                name: "Authorization".into(),
                value: "Bearer $COPILOT_GITHUB_TOKEN".into(),
            }],
            ..ProviderEntry::default()
        };
        unsafe {
            std::env::set_var("GH_TOKEN", "ghu_test");
        }
        let resolved = resolve_provider_request("copilot", &entry).unwrap();
        let auth = resolved
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("authorization"))
            .unwrap();
        assert_eq!(auth.value, "Bearer ghu_test");
        clear_copilot_env();
    }

    #[test]
    fn copilot_uses_direct_api_url_override() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_copilot_env();
        let entry = ProviderEntry {
            url: "https://api.githubcopilot.com".into(),
            headers: vec![],
            ..ProviderEntry::default()
        };
        unsafe {
            std::env::set_var("GITHUB_COPILOT_API_TOKEN", "token");
            std::env::set_var("COPILOT_API_URL", "https://copilot-proxy.example/v1/");
        }
        let resolved = resolve_provider_request("copilot", &entry).unwrap();
        assert_eq!(resolved.base_url, "https://copilot-proxy.example/v1");
        clear_copilot_env();
    }

    #[test]
    fn copilot_rejects_classic_pat() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_copilot_env();
        let entry = ProviderEntry {
            url: "https://api.githubcopilot.com".into(),
            headers: vec![],
            ..ProviderEntry::default()
        };
        unsafe {
            std::env::set_var("COPILOT_GITHUB_TOKEN", "ghp_legacy");
        }
        let err = resolve_provider_request("copilot", &entry).unwrap_err();
        assert!(err.to_string().contains("classic GitHub PAT"));
        clear_copilot_env();
    }

    #[test]
    fn copilot_detected_via_url_when_provider_id_differs() {
        // A user might add a Copilot endpoint under a custom id; the
        // resolver still picks up the documented env-var fallbacks.
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_copilot_env();
        let entry = ProviderEntry {
            url: "https://api.githubcopilot.com".into(),
            headers: vec![],
            ..ProviderEntry::default()
        };
        unsafe {
            std::env::set_var("COPILOT_GITHUB_TOKEN", "gho_via_url");
        }
        let resolved = resolve_provider_request("my-copilot", &entry).unwrap();
        let auth = resolved
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("authorization"))
            .unwrap();
        assert_eq!(auth.value, "Bearer gho_via_url");
        clear_copilot_env();
    }

    #[test]
    fn copilot_priority_prefers_copilot_github_token_over_gh_token() {
        // With both vars set the highest-priority source wins.
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_copilot_env();
        let entry = ProviderEntry {
            url: "https://api.githubcopilot.com".into(),
            headers: vec![],
            ..ProviderEntry::default()
        };
        unsafe {
            std::env::set_var("COPILOT_GITHUB_TOKEN", "gho_primary");
            std::env::set_var("GH_TOKEN", "gho_secondary");
            std::env::set_var("GITHUB_TOKEN", "gho_tertiary");
        }
        let resolved = resolve_provider_request("copilot", &entry).unwrap();
        let auth = resolved
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("authorization"))
            .unwrap();
        assert_eq!(auth.value, "Bearer gho_primary");
        clear_copilot_env();
    }

    #[test]
    fn copilot_errors_when_no_env_var_set() {
        // Sanity check: with no headers and no env vars, the resolver
        // emits the documented-token guidance instead of falling back
        // to the legacy device-code path.
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_copilot_env();
        let entry = ProviderEntry {
            url: "https://api.githubcopilot.com".into(),
            headers: vec![],
            ..ProviderEntry::default()
        };
        let err = resolve_provider_request("copilot", &entry).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("COPILOT_GITHUB_TOKEN"));
        assert!(msg.contains("GH_TOKEN"));
        assert!(msg.contains("GITHUB_TOKEN"));
        // Critically, the message must not point users at the old
        // device-code login path.
        assert!(!msg.contains("device-code"));
        assert!(!msg.contains("copilot_internal"));
    }

    #[test]
    fn non_copilot_provider_with_missing_auth_env_errors() {
        // A non-Copilot provider whose `Authorization` references an
        // unset var must NOT silently fall back to Copilot env vars.
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_copilot_env();
        let entry = ProviderEntry {
            url: "https://api.example.com/v1".into(),
            headers: vec![HeaderSpec {
                name: "Authorization".into(),
                value: "Bearer $TOTALLY_UNSET_VAR_PROBE".into(),
            }],
            ..ProviderEntry::default()
        };
        unsafe {
            std::env::remove_var("TOTALLY_UNSET_VAR_PROBE");
            // Even if a Copilot fallback is set, a non-Copilot
            // provider must not pick it up.
            std::env::set_var("COPILOT_GITHUB_TOKEN", "gho_should_not_leak");
        }
        let err = resolve_provider_request("some-vendor", &entry).unwrap_err();
        assert!(err.to_string().contains("TOTALLY_UNSET_VAR_PROBE"));
        clear_copilot_env();
    }

    #[test]
    fn non_copilot_provider_without_auth_resolves_unauthenticated() {
        // A fully-local endpoint (e.g. LM Studio) has no Authorization
        // header. That must resolve cleanly so /models can be fetched
        // unauthenticated rather than erroring out.
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_copilot_env();
        let entry = ProviderEntry {
            url: "http://localhost:1234/v1".into(),
            headers: vec![],
            ..ProviderEntry::default()
        };
        let resolved = resolve_provider_request("lmstudio", &entry).unwrap();
        assert!(
            !resolved
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("authorization"))
        );
    }

    #[test]
    fn copilot_template_is_apikey_with_documented_default_env() {
        // The Add-Provider wizard should no longer offer a device-code
        // flow for Copilot. Pin the template's shape so it can't
        // regress.
        let t = crate::providers::template_by_id("copilot").expect("copilot template");
        assert!(matches!(t.auth, crate::config::providers::AuthKind::ApiKey));
        assert_eq!(t.default_env_var, Some("COPILOT_GITHUB_TOKEN"));
        assert_eq!(t.default_headers.len(), 1);
        assert_eq!(t.default_headers[0].0, "Authorization");
        assert_eq!(t.default_headers[0].1, "Bearer $COPILOT_GITHUB_TOKEN");
    }
}
