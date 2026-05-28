//! Cheap-model skill auto-selection (GOALS §5).
//!
//! On each user turn the driver consults the configured `utility_model`
//! with the catalog of skill `(name, description)` pairs plus the user
//! message; the model picks zero or one most-relevant skill. When one is
//! selected, its body is loaded (after `!`-processing — Claude/Codex mode
//! per [`crate::skills::render_body`]) and the driver injects it into
//! context before the main agent's turn.
//!
//! Token economy (GOALS §10): the cheap model sees only the catalog,
//! never a body. The body is the sole large payload and only materializes
//! on selection.
//!
//! Graceful degradation mirrors [`crate::auto_title`]: when
//! `utility_model` is unset the pass is skipped (logged once via the
//! caller), never erroring and never falling back to the main model.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::config::extended::ExtendedConfig;
use crate::config::providers::ProvidersConfig;

/// Timeout for the utility-model selection call. Selection is
/// best-effort; if the provider stalls we'd rather skip injection than
/// hold up the user's turn.
pub const SELECT_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Result of an auto-selection pass.
pub enum Selection {
    /// A skill was chosen; carries the rendered (`!`-processed) body and
    /// the skill name for the injected header.
    Skill { name: String, body: String },
    /// No skill was selected this turn (model declined, no skills, or no
    /// utility model). The driver injects nothing.
    None,
}

/// Run one auto-selection pass for `user_text` against the configured
/// skills + utility model. Best-effort: any error (unset utility model,
/// network blip, parse failure) resolves to [`Selection::None`] via the
/// caller's `?`-free wrapper [`select`]. `cwd` scopes both skill
/// discovery and the layered config.
pub async fn select(
    cwd: &Path,
    extended: &ExtendedConfig,
    providers: &ProvidersConfig,
    redact: &crate::redact::RedactionTable,
    user_text: &str,
) -> Selection {
    match select_inner(cwd, extended, providers, redact, user_text).await {
        Ok(sel) => sel,
        Err(e) => {
            tracing::debug!(error = %e, "skills auto-select: pass ended without a skill");
            Selection::None
        }
    }
}

async fn select_inner(
    cwd: &Path,
    extended: &ExtendedConfig,
    providers: &ProvidersConfig,
    redact: &crate::redact::RedactionTable,
    user_text: &str,
) -> Result<Selection> {
    // Unset utility model → skip gracefully. The caller logs the
    // skip-once notice; here we just bail cleanly.
    let Some(model_ref) = extended.utility_model.as_deref() else {
        return Ok(Selection::None);
    };

    let skills = crate::skills::discover(cwd, &extended.skills)?;
    if skills.is_empty() {
        return Ok(Selection::None);
    }

    let (provider_id, model_id) = model_ref
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("utility_model `{model_ref}` must be provider:model-id"))?;
    let model = crate::engine::model::Model::for_provider(providers, provider_id, model_id)?;

    let catalog = crate::skills::catalog_lines(&skills);
    let prompt = build_select_prompt(&catalog, user_text);
    let response =
        tokio::time::timeout(SELECT_CALL_TIMEOUT, model.text_completion(&prompt)).await??;

    let Some(chosen) = parse_choice(&response, &skills) else {
        return Ok(Selection::None);
    };

    let body = crate::skills::load_body(chosen)?;
    let rendered =
        crate::skills::render_body(&body, cwd, extended.skills.auto_bang_commands, redact);
    Ok(Selection::Skill {
        name: chosen.frontmatter.name.clone(),
        body: rendered,
    })
}

/// One-shot prompt asking the utility model to pick zero or one skill.
/// Kept terse: catalog + the user message + a one-line instruction. The
/// model sees only `(name, description)` pairs — never a body.
fn build_select_prompt(catalog: &str, user_text: &str) -> String {
    format!(
        "You route a user message to at most one helper skill. Below is a \
         catalog of available skills as `- name: description` lines, then \
         the user's message. Reply with EXACTLY the name of the single \
         most-relevant skill, or the word NONE if none clearly applies. \
         Reply with only the name (or NONE) — no punctuation, no \
         explanation.\n\n\
         <skills>\n{catalog}</skills>\n\n\
         <message>\n{user_text}\n</message>\n"
    )
}

/// Parse the utility model's reply into a chosen skill. Accepts an exact
/// (case-insensitive, trimmed) name match; `NONE` / empty / no-match all
/// resolve to `None`.
fn parse_choice<'a>(
    response: &str,
    skills: &'a [crate::skills::Skill],
) -> Option<&'a crate::skills::Skill> {
    let raw = response.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
        return None;
    }
    // Take the first whitespace-delimited token in case the model adds
    // stray text despite the instruction.
    let candidate = raw.split_whitespace().next().unwrap_or(raw);
    let candidate = candidate.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
    skills
        .iter()
        .find(|s| s.frontmatter.name.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{Skill, SkillFrontmatter};
    use std::path::PathBuf;

    fn skill(name: &str) -> Skill {
        Skill {
            frontmatter: SkillFrontmatter {
                name: name.into(),
                description: "d".into(),
                model: None,
            },
            source: PathBuf::from(format!("/x/{name}/SKILL.md")),
        }
    }

    #[test]
    fn parse_choice_exact_match() {
        let skills = vec![skill("deploy"), skill("review")];
        let got = parse_choice("deploy", &skills).unwrap();
        assert_eq!(got.frontmatter.name, "deploy");
    }

    #[test]
    fn parse_choice_case_insensitive_and_trimmed() {
        let skills = vec![skill("Deploy")];
        let got = parse_choice("  deploy\n", &skills).unwrap();
        assert_eq!(got.frontmatter.name, "Deploy");
    }

    #[test]
    fn parse_choice_none_keyword() {
        let skills = vec![skill("deploy")];
        assert!(parse_choice("NONE", &skills).is_none());
        assert!(parse_choice("none", &skills).is_none());
        assert!(parse_choice("", &skills).is_none());
    }

    #[test]
    fn parse_choice_unknown_name() {
        let skills = vec![skill("deploy")];
        assert!(parse_choice("ship-it", &skills).is_none());
    }

    #[test]
    fn parse_choice_ignores_trailing_prose() {
        let skills = vec![skill("deploy")];
        let got = parse_choice("deploy — this fits best", &skills).unwrap();
        assert_eq!(got.frontmatter.name, "deploy");
    }

    #[tokio::test]
    async fn select_skips_when_utility_model_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let scan = tmp.path().join("scan");
        std::fs::create_dir_all(&scan.join("deploy")).unwrap();
        std::fs::write(
            scan.join("deploy").join("SKILL.md"),
            "---\nname: deploy\ndescription: d\n---\nBODY",
        )
        .unwrap();

        let mut extended = ExtendedConfig::default();
        extended.skills.scan_dirs = vec![scan.to_string_lossy().into_owned()];
        // utility_model deliberately unset.
        let providers = ProvidersConfig::default();
        let redact = crate::redact::RedactionTable::build(&Default::default(), tmp.path()).unwrap();

        let sel = select(tmp.path(), &extended, &providers, &redact, "deploy please").await;
        assert!(
            matches!(sel, Selection::None),
            "unset utility_model must skip auto-selection without error"
        );
    }
}
