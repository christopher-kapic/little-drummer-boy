//! Session auto-titling via the utility model (GOALS §17d).
//!
//! When the running estimate of user-authored content (composer
//! prose + `@`-tagged inlined files) crosses
//! [`TITLE_TOKEN_THRESHOLD`], the daemon issues a single completion
//! call against `extended.utility_model` to produce a title for the
//! session. The result is slugified and stored via
//! [`crate::session::Session::set_auto_title`] (which refuses to
//! overwrite a user-set title — see §17d).
//!
//! Forks get an independent pass keyed to post-divergence content;
//! the session-level counter is per-`Session`, not per-tree.
//!
//! Failure paths are silent: a missing `utility_model`, an unset
//! API key, a network blip, or a garbage response all produce a
//! trace log and a no-op. We never block the driver loop.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use std::path::Path;

use crate::config::extended::ExtendedConfig;
use crate::config::providers::ProvidersConfig;
use crate::session::Session;

/// cl100k_base token count for `text`. Re-exported here for callers
/// that already imported this module — new code should call
/// [`crate::tokens::count`] directly.
pub fn estimate_tokens(text: &str) -> usize {
    crate::tokens::count(text)
}

/// Threshold for firing the auto-title pass (GOALS §17d).
pub const TITLE_TOKEN_THRESHOLD: usize = 500;

/// Maximum title length, post-slugification.
pub const TITLE_MAX_CHARS: usize = 60;

/// Timeout for the utility-model call. Titles are best-effort; if
/// the provider takes longer than this, we'd rather drop the title
/// than tie up a daemon task indefinitely.
pub const TITLE_CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Slugify a raw model response into a `[a-z0-9-]+` title. Returns
/// `None` if nothing survives — the caller treats that as "no title
/// this pass; try again at the next threshold crossing."
pub fn slugify_title(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut last_was_hyphen = false;
    for c in raw.trim().chars() {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_was_hyphen = false;
        } else if !last_was_hyphen && !out.is_empty() {
            out.push('-');
            last_was_hyphen = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    let capped: String = trimmed.chars().take(TITLE_MAX_CHARS).collect();
    let capped = capped.trim_end_matches('-').to_string();
    if capped.is_empty() {
        None
    } else {
        Some(capped)
    }
}

/// Fire the auto-titling pass against `session`. Best-effort; never
/// returns an error to the caller. Intended to be spawned in a
/// detached tokio task — the driver loop doesn't wait on it.
pub async fn generate_session_title(
    session: Arc<Session>,
    extended: ExtendedConfig,
    providers: ProvidersConfig,
    content_prefix: String,
) {
    if let Err(e) = generate_inner(session, extended, providers, content_prefix).await {
        tracing::debug!(error = %e, "auto_title: pass ended without a title");
    }
}

async fn generate_inner(
    session: Arc<Session>,
    extended: ExtendedConfig,
    providers: ProvidersConfig,
    content_prefix: String,
) -> Result<()> {
    let Some(model_ref) = extended.utility_model.as_deref() else {
        anyhow::bail!("utility_model is not configured");
    };
    let (provider_id, model_id) = model_ref
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("utility_model `{model_ref}` must be provider:model-id"))?;

    let model = crate::engine::model::Model::for_provider(&providers, provider_id, model_id)?;
    let prompt = build_title_prompt(&content_prefix);
    let response =
        tokio::time::timeout(TITLE_CALL_TIMEOUT, model.text_completion(&prompt)).await??;

    let Some(slug) = slugify_title(&response) else {
        anyhow::bail!("utility model produced no usable title (raw = {response:?})");
    };

    let _ = session
        .set_auto_title(&slug)
        .map_err(|e| tracing::warn!(error = %e, "auto_title: persist failed"));
    Ok(())
}

/// Load the layered `extended-config.json` and `config.json` (providers)
/// from `cwd`, picking the first hit in `discover_config_dirs` order.
/// Best-effort: returns defaults when either file is missing or
/// unparseable. The driver hook calls this from inside the spawned
/// auto-title task so config IO doesn't block the inference loop.
pub fn load_configs_for(cwd: &Path) -> (ExtendedConfig, ProvidersConfig) {
    use crate::config::dirs::discover_config_dirs;
    use crate::config::extended::ExtendedConfigDoc;
    use crate::config::providers::ConfigDoc;

    let dirs = discover_config_dirs(cwd);
    let mut extended = ExtendedConfig::default();
    let mut providers = ProvidersConfig::default();
    for dir in &dirs {
        let path = dir.path.join("extended-config.json");
        if path.exists() {
            if let Ok(doc) = ExtendedConfigDoc::load(&path) {
                extended = doc.config();
                break;
            }
        }
    }
    for dir in &dirs {
        let path = dir.path.join("config.json");
        if path.exists() {
            if let Ok(doc) = ConfigDoc::load(&path) {
                providers = doc.providers();
                break;
            }
        }
    }
    (extended, providers)
}

/// One-shot prompt asking the utility model for a title. Kept terse:
/// the model gets the prefix of user-authored content plus a one-line
/// instruction. Total prompt token cost ≈ (prefix tokens) + ~30 for
/// the instruction.
fn build_title_prompt(content_prefix: &str) -> String {
    format!(
        "Produce a short kebab-case title (2-6 words, lowercase, \
         hyphens only) summarising this conversation. Return ONLY \
         the title — no quotes, no explanation, no trailing punctuation.\n\n\
         <content>\n{content_prefix}\n</content>\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_delegates_to_tiktoken() {
        assert_eq!(estimate_tokens(""), 0);
        // Real cl100k_base counts; just sanity-check that non-empty
        // input produces a positive count and grows with length.
        assert!(estimate_tokens("abcdefgh") > 0);
        assert!(estimate_tokens(&"hello ".repeat(100)) > estimate_tokens("hello"));
    }

    #[test]
    fn slugify_basic_phrase() {
        assert_eq!(
            slugify_title("Fix redact allowlist regression").as_deref(),
            Some("fix-redact-allowlist-regression")
        );
    }

    #[test]
    fn slugify_strips_punctuation_and_lowercases() {
        assert_eq!(
            slugify_title("Add: pixel banner!!!").as_deref(),
            Some("add-pixel-banner")
        );
    }

    #[test]
    fn slugify_collapses_runs() {
        assert_eq!(
            slugify_title("a   b\n\nc").as_deref(),
            Some("a-b-c")
        );
    }

    #[test]
    fn slugify_caps_at_max() {
        let raw = "this is a very long title that should be truncated at exactly the maximum allowed length and not beyond";
        let s = slugify_title(raw).unwrap();
        assert!(s.len() <= TITLE_MAX_CHARS, "{s} (len {})", s.len());
        assert!(!s.ends_with('-'), "trailing hyphen survived the cap: {s}");
    }

    #[test]
    fn slugify_returns_none_for_empty() {
        assert_eq!(slugify_title(""), None);
        assert_eq!(slugify_title("!@#$%^&*()"), None);
        assert_eq!(slugify_title("   "), None);
    }

    #[test]
    fn slugify_trims_leading_garbage() {
        assert_eq!(
            slugify_title("\"some title\"").as_deref(),
            Some("some-title")
        );
    }
}
