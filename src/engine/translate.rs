//! Round-trip utility-model translation (`prompts/utility-translation.md`).
//!
//! Lets a user work in their own language while the coding model works in
//! another: the inbound user prompt is translated into the model's
//! language before it reaches the main agent (after the prompt-injection
//! scan, before outbound redaction); the agent's complete final response
//! is translated back into the user's language before it is shown.
//!
//! Both directions are history-free, one-shot
//! [`Model::text_completion`](crate::engine::model::Model::text_completion)
//! calls against [`ExtendedConfig::utility_model`]. The translation prompt
//! instructs the utility model to translate **only** natural-language
//! prose and leave code blocks, inline code, file paths, identifiers,
//! commands, and CLI flags untouched — this is a coding harness, and
//! mistranslating those would corrupt the agent's input/output.
//!
//! Every failure path degrades: an unset/unavailable/erroring utility
//! model, inactive languages, or a timeout all return the input
//! unchanged rather than blocking the turn.

use std::time::Duration;

use crate::config::extended::ExtendedConfig;
use crate::config::providers::ProvidersConfig;

/// Timeout for one translation call. Translation is best-effort; if the
/// provider stalls we'd rather pass the text through untranslated than
/// hang the turn.
const TRANSLATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Translate the inbound `text` from the user's language into the model's
/// language. Returns the input unchanged when translation is inactive
/// (languages unset/equal), the utility model is unset/unavailable, or
/// the call errors/times out.
pub async fn inbound(text: &str, extended: &ExtendedConfig, providers: &ProvidersConfig) -> String {
    translate_direction(
        text,
        &extended.translation.user_language,
        &extended.translation.model_language,
        extended,
        providers,
    )
    .await
}

/// Translate the agent's complete final response from the model's
/// language back into the user's language. Same degrade contract as
/// [`inbound`].
pub async fn outbound(
    text: &str,
    extended: &ExtendedConfig,
    providers: &ProvidersConfig,
) -> String {
    translate_direction(
        text,
        &extended.translation.model_language,
        &extended.translation.user_language,
        extended,
        providers,
    )
    .await
}

/// Core: translate `text` from `source` into `target` using the utility
/// model. Pass-through (returns `text` owned) on every disabled/degrade
/// path so callers never have to special-case failure.
async fn translate_direction(
    text: &str,
    source: &str,
    target: &str,
    extended: &ExtendedConfig,
    providers: &ProvidersConfig,
) -> String {
    // Inactive feature (unset/equal languages) → no translation.
    if !extended.translation.is_active() {
        return text.to_string();
    }
    // Nothing to translate.
    if text.trim().is_empty() {
        return text.to_string();
    }
    match try_translate(text, source, target, extended, providers).await {
        Some(out) => out,
        None => text.to_string(),
    }
}

/// Attempt the utility-model translation, returning `None` on every
/// failure path (unset/unparseable/unbuildable model, send error, timeout,
/// empty response) so the caller degrades to pass-through.
async fn try_translate(
    text: &str,
    source: &str,
    target: &str,
    extended: &ExtendedConfig,
    providers: &ProvidersConfig,
) -> Option<String> {
    let model_ref = extended.utility_model.as_deref()?;
    let (provider_id, model_id) = model_ref.split_once(':')?;
    let model = match crate::engine::model::Model::for_provider(providers, provider_id, model_id) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "translate: model build failed; passing through");
            return None;
        }
    };

    let prompt = build_translation_prompt(source, target, text);
    let response =
        match tokio::time::timeout(TRANSLATE_TIMEOUT, model.text_completion(&prompt)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "translate: call failed; passing through");
                return None;
            }
            Err(_) => {
                tracing::debug!("translate: call timed out; passing through");
                return None;
            }
        };

    if response.trim().is_empty() {
        return None;
    }
    Some(response)
}

/// Build the one-shot translation prompt. Names the source and target
/// languages and instructs the utility model to translate only natural-
/// language prose, leaving code and machine-readable tokens verbatim. The
/// untrusted text is fenced so the model treats it as content, not
/// instructions, and is told to return only the translation.
pub fn build_translation_prompt(source: &str, target: &str, text: &str) -> String {
    format!(
        "Translate the natural-language prose in the text below from {source} to {target}. \
         This is text from a software-engineering coding tool: leave all code blocks, inline \
         code, file paths, identifiers, commands, and CLI flags exactly as written — translate \
         only the surrounding prose. Return ONLY the translated text, with no preamble, no \
         explanation, and no code fences around the whole answer.\n\n\
         <text>\n{text}\n</text>",
        source = source.trim(),
        target = target.trim(),
    )
}

/// Remove `<think>…</think>` reasoning blocks from `text`, matching what
/// the streamed-text path already shows the user (the TUI routes reasoning
/// onto a separate channel). Used before outbound translation so the
/// utility model never translates — and the finalized entry never shows —
/// the model's inline chain-of-thought. An unterminated `<think>` (no
/// closing tag) drops everything from the open tag onward, mirroring the
/// streamed path's "still inside think" state at end of stream. Leaves
/// text with no think blocks untouched.
pub fn strip_think_blocks(text: &str) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open_idx) = rest.find(OPEN) {
        out.push_str(&rest[..open_idx]);
        let after_open = &rest[open_idx + OPEN.len()..];
        match after_open.find(CLOSE) {
            Some(close_idx) => {
                // Skip the block; drop a single `\n` right after the close so
                // the answer doesn't render with a leading blank line.
                let after_close = &after_open[close_idx + CLOSE.len()..];
                rest = after_close.strip_prefix('\n').unwrap_or(after_close);
            }
            None => {
                // Unterminated block: nothing usable remains.
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Resolve `(ExtendedConfig, ProvidersConfig)` for `cwd` and check whether
/// translation is configured active. A thin convenience over
/// [`crate::auto_title::load_configs_for`] used by call sites that only
/// translate when the feature is on (so they can skip the config load on
/// the common path). Returns the loaded configs alongside the flag so the
/// caller reuses them for the actual call.
pub fn load_if_active(cwd: &std::path::Path) -> Option<(ExtendedConfig, ProvidersConfig)> {
    let (extended, providers) = crate::auto_title::load_configs_for(cwd);
    if extended.translation.is_active() {
        Some((extended, providers))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::extended::TranslationConfig;

    fn cfg_with(user: &str, model: &str, utility: Option<&str>) -> ExtendedConfig {
        ExtendedConfig {
            utility_model: utility.map(|s| s.to_string()),
            translation: TranslationConfig {
                user_language: user.to_string(),
                model_language: model.to_string(),
            },
            ..ExtendedConfig::default()
        }
    }

    #[test]
    fn prompt_includes_languages_and_preserve_instruction() {
        let p = build_translation_prompt("Spanish", "English", "hola mundo");
        assert!(p.contains("Spanish"), "{p}");
        assert!(p.contains("English"), "{p}");
        assert!(p.contains("hola mundo"), "{p}");
        // The defining instruction: leave code/paths/identifiers/commands/
        // flags untouched.
        assert!(p.contains("code blocks"), "{p}");
        assert!(p.contains("inline"), "{p}");
        assert!(p.contains("file paths"), "{p}");
        assert!(p.contains("identifiers"), "{p}");
        assert!(p.contains("commands"), "{p}");
        assert!(p.contains("CLI flags"), "{p}");
    }

    #[tokio::test]
    async fn inactive_languages_pass_through_unchanged() {
        // Equal languages → inactive → no translation even with a utility
        // model set. Degrades to the input verbatim (no network).
        let extended = cfg_with("English", "English", Some("anthropic:claude-haiku-4-5"));
        let providers = ProvidersConfig::default();
        let out = inbound("hello", &extended, &providers).await;
        assert_eq!(out, "hello");
        let out = outbound("hello", &extended, &providers).await;
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn unset_utility_model_passes_through_unchanged() {
        // Active languages but no utility model → degrade to pass-through
        // with no error.
        let extended = cfg_with("Spanish", "English", None);
        let providers = ProvidersConfig::default();
        let out = inbound("hola", &extended, &providers).await;
        assert_eq!(out, "hola");
        let out = outbound("hello", &extended, &providers).await;
        assert_eq!(out, "hello");
    }

    #[test]
    fn strip_think_blocks_removes_reasoning() {
        assert_eq!(
            strip_think_blocks("<think>reasoning here</think>\nThe answer."),
            "The answer."
        );
        // No think block → untouched.
        assert_eq!(strip_think_blocks("just an answer"), "just an answer");
        // Unterminated → everything from the open tag drops.
        assert_eq!(
            strip_think_blocks("before <think>still thinking"),
            "before "
        );
        // Multiple blocks.
        assert_eq!(
            strip_think_blocks("<think>a</think>X<think>b</think>Y"),
            "XY"
        );
    }

    #[tokio::test]
    async fn empty_text_passes_through() {
        let extended = cfg_with("Spanish", "English", Some("anthropic:claude-haiku-4-5"));
        let providers = ProvidersConfig::default();
        assert_eq!(inbound("   ", &extended, &providers).await, "   ");
    }
}
