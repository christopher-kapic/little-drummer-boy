//! Provider/model detection.
//!
//! Resolution order, first wins:
//!   1. `COCKPIT_PROVIDER` + `COCKPIT_MODEL` env vars (or `COCKPIT_MODEL`
//!      alone if it's in `provider/model` form).
//!   2. The first `config.json` found on the layered-config walk that
//!      names a default provider/model.
//!
//! A malformed `config.json` is logged at `warn` and skipped.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::dirs::walk_up_to_stops;

/// Detected (provider, model) pair, or `None` if nothing is configured.
pub fn detect_provider_model(cwd: &Path) -> Option<(String, String)> {
    detect_from_env().or_else(|| detect_from_configs(cwd))
}

fn detect_from_env() -> Option<(String, String)> {
    let provider = env::var("COCKPIT_PROVIDER")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let model = env::var("COCKPIT_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty());

    match (provider, model) {
        (Some(provider), Some(model)) => Some((provider, model)),
        (None, Some(model)) => split_provider_model(&model),
        _ => None,
    }
}

fn detect_from_configs(cwd: &Path) -> Option<(String, String)> {
    let mut selected = None;

    for path in config_candidates(cwd) {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let json: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "malformed cockpit config.json — skipping");
                continue;
            }
        };
        if let Some(pair) = extract_provider_model(&json) {
            selected = Some(pair);
        }
    }

    selected
}

fn config_candidates(cwd: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".cockpit/config.json"));
        candidates.push(home.join(".config/cockpit/config.json"));
    }

    // Project-scoped layers, deepest last so they win on later overwrite.
    let mut layered = walk_up_to_stops(cwd);
    layered.reverse();
    for dir in layered {
        candidates.push(dir.join(".cockpit/config.json"));
    }

    candidates
}

fn extract_provider_model(json: &Value) -> Option<(String, String)> {
    // cockpit-native schema: top-level `active_model: { provider, model }`.
    let active_provider = read_string(json.pointer("/active_model/provider"));
    let active_model = read_string(json.pointer("/active_model/model"));
    if let (Some(provider), Some(model)) = (active_provider, active_model) {
        return Some((provider, model));
    }

    // Fallback for legacy / opencode-flavored shapes.
    let default_provider = read_string(json.pointer("/models/categories/default/provider"));
    let default_model = read_string(json.pointer("/models/categories/default/model"));
    if let (Some(provider), Some(model)) = (default_provider, default_model) {
        return Some((provider, model));
    }

    let top_level_provider = read_string(json.pointer("/provider"));
    let top_level_model = read_string(json.pointer("/model"));
    if let (Some(provider), Some(model)) = (top_level_provider, top_level_model) {
        return Some((provider, model));
    }

    for pointer in ["/default_model", "/models/default_model", "/model"] {
        if let Some(model) = read_string(json.pointer(pointer))
            && let Some(pair) = split_provider_model(&model)
        {
            return Some(pair);
        }
    }

    // Last-resort fallback: surface the first configured provider's
    // first listed model so a freshly-added provider isn't invisible.
    if let Some(providers) = json.get("providers").and_then(Value::as_object) {
        for (pid, entry) in providers {
            if let Some(models) = entry.get("models").and_then(Value::as_array)
                && let Some(model_id) = models
                    .first()
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
            {
                return Some((pid.clone(), model_id.to_string()));
            }
        }
    }

    None
}

fn read_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn split_provider_model(value: &str) -> Option<(String, String)> {
    let (provider, model) = value.split_once('/')?;
    let provider = provider.trim();
    let model = model.trim();
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some((provider.to_string(), model.to_string()))
}
