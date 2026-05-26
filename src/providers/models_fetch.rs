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

use crate::config::providers::{HeaderSpec, ModelEntry, ThinkingMode};
use crate::envref;

/// Resolved view of a `HeaderSpec` after `$VAR` expansion.
#[derive(Debug, Clone)]
pub struct ResolvedHeader {
    pub name: String,
    pub value: String,
}

/// Apply `$VAR` resolution to every header, collecting any missing-env
/// references into one list. Caller decides whether to abort or warn.
pub fn resolve_headers(headers: &[HeaderSpec]) -> (Vec<ResolvedHeader>, Vec<String>) {
    let mut out = Vec::with_capacity(headers.len());
    let mut missing: Vec<String> = Vec::new();
    for h in headers {
        let r = envref::resolve(&h.value);
        for m in r.missing {
            if !missing.iter().any(|n| n == &m) {
                missing.push(m);
            }
        }
        out.push(ResolvedHeader {
            name: h.name.clone(),
            value: r.value,
        });
    }
    (out, missing)
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
    headers: &[HeaderSpec],
    timeout: Option<Duration>,
) -> Result<FetchOutcome> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let (resolved, missing) = resolve_headers(headers);
    if !missing.is_empty() {
        anyhow::bail!(
            "cannot fetch models: env var(s) not set: {}",
            missing.join(", ")
        );
    }

    let mut builder = reqwest::Client::builder();
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    let client = builder.build().context("building reqwest client")?;

    let mut req = client.get(&url).header("Accept", "application/json");
    for h in resolved {
        req = req.header(h.name, h.value);
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
                    "id" | "name" | "display_name" | "thinking_modes" | "inputs"
                ) {
                    continue;
                }
                extra.insert(k.clone(), v.clone());
            }

            Some(ModelEntry {
                id,
                name,
                thinking_modes,
                inputs,
                extra,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
