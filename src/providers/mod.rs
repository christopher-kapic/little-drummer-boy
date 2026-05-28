#![allow(dead_code)]
//! Built-in provider templates.
//!
//! The Add-Provider wizard offers these as prefill choices, in addition
//! to the catch-all `openai-compatible` template. Adapted from
//! `mixer-rs/src/providers/{glm,minimax,opencode,codex}.rs` — the URLs,
//! display names, and auth shape match what mixer ships with.
//!
//! These are *templates*, not provider implementations: a user that
//! picks `z.ai` ends up with a regular [`crate::config::providers::ProviderEntry`]
//! whose URL and headers are pre-populated. No special code path runs at
//! request time.

pub mod models_fetch;

use crate::config::providers::{AuthKind, HeaderSpec};

/// One picker entry in the Add Provider wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTemplate {
    /// Stable id used as the config-map key.
    pub id: &'static str,
    /// Human-readable label shown in the picker.
    pub display: &'static str,
    /// Pre-filled base URL.
    pub url: &'static str,
    /// Auth model — drives wizard prompts (env-var name, OAuth flow, etc.).
    pub auth: AuthKind,
    /// Suggested env-var name for API-key providers. Used to seed the
    /// Authorization header value with `Bearer $NAME`.
    pub default_env_var: Option<&'static str>,
    /// Headers to write into config. `value` may contain `$VAR`
    /// references; the wizard auto-fills `$default_env_var` if present.
    pub default_headers: &'static [(&'static str, &'static str)],
    /// Whether the upstream exposes a `/models` endpoint we can hit.
    pub supports_models_endpoint: bool,
    /// One-liner shown under the URL field — typically a link to the
    /// vendor's API-key page.
    pub hint: Option<&'static str>,
    /// If `true`, the template's `id` may be used as the default when
    /// adding. The OpenAI-compatible template is `false` because the
    /// user is expected to add several of them (one per vendor) and
    /// they must each have distinct ids.
    pub use_id_as_default: bool,
}

/// The catalog the wizard cycles through. `openai-compatible` is first
/// (per the user spec) so it's the default landing entry in the picker.
pub const TEMPLATES: &[ProviderTemplate] = &[
    ProviderTemplate {
        id: "openai-compatible",
        display: "OpenAI-compatible",
        url: "",
        auth: AuthKind::ApiKey,
        default_env_var: None,
        default_headers: &[("Authorization", "Bearer $API_KEY")],
        supports_models_endpoint: true,
        hint: Some(
            "Generic OpenAI-compatible endpoint. You can add as many of these as you want; each one needs a unique id.",
        ),
        use_id_as_default: false,
    },
    ProviderTemplate {
        id: "z-ai",
        display: "z.ai (GLM)",
        url: "https://api.z.ai/api/paas/v4",
        auth: AuthKind::ApiKey,
        default_env_var: Some("Z_AI_API_KEY"),
        default_headers: &[("Authorization", "Bearer $Z_AI_API_KEY")],
        supports_models_endpoint: false,
        hint: Some("Generate a key at https://z.ai/manage-apikey/apikey-list"),
        use_id_as_default: true,
    },
    ProviderTemplate {
        id: "minimax",
        display: "MiniMax",
        url: "https://api.minimax.io/v1",
        auth: AuthKind::ApiKey,
        default_env_var: Some("MINIMAX_API_KEY"),
        default_headers: &[("Authorization", "Bearer $MINIMAX_API_KEY")],
        supports_models_endpoint: true,
        hint: Some("Generate a key at https://platform.minimaxi.com/"),
        use_id_as_default: true,
    },
    ProviderTemplate {
        id: "opencode-zen",
        display: "OpenCode Zen",
        url: "https://opencode.ai/zen/v1",
        auth: AuthKind::ApiKey,
        default_env_var: Some("OPENCODE_ZEN_TOKEN"),
        default_headers: &[("Authorization", "Bearer $OPENCODE_ZEN_TOKEN")],
        supports_models_endpoint: true,
        hint: Some("Generate a token at https://opencode.ai/zen"),
        use_id_as_default: true,
    },
    ProviderTemplate {
        id: "codex",
        display: "Codex (ChatGPT Plus/Pro)",
        url: "https://chatgpt.com/backend-api/codex",
        auth: AuthKind::DeviceFlow,
        default_env_var: None,
        default_headers: &[],
        supports_models_endpoint: false,
        hint: Some(
            "Codex uses OAuth device flow. Run `cockpit providers login codex` from a terminal.",
        ),
        use_id_as_default: true,
    },
    ProviderTemplate {
        id: "copilot",
        display: "GitHub Copilot",
        url: "https://api.githubcopilot.com",
        auth: AuthKind::ApiKey,
        default_env_var: Some("COPILOT_GITHUB_TOKEN"),
        default_headers: &[("Authorization", "Bearer $COPILOT_GITHUB_TOKEN")],
        supports_models_endpoint: true,
        hint: Some(
            "Auth uses GitHub's documented tokens. Set COPILOT_GITHUB_TOKEN, GH_TOKEN, or GITHUB_TOKEN to a GitHub OAuth/App/fine-grained token with Copilot access (a token from the `copilot` CLI works). COPILOT_API_URL overrides the base URL.",
        ),
        use_id_as_default: true,
    },
    ProviderTemplate {
        id: "openrouter",
        display: "OpenRouter",
        url: "https://openrouter.ai/api/v1",
        auth: AuthKind::ApiKey,
        default_env_var: Some("OPENROUTER_API_KEY"),
        default_headers: &[("Authorization", "Bearer $OPENROUTER_API_KEY")],
        supports_models_endpoint: true,
        hint: Some("Generate a key at https://openrouter.ai/keys"),
        use_id_as_default: true,
    },
    ProviderTemplate {
        id: "deepseek",
        display: "DeepSeek",
        url: "https://api.deepseek.com/v1",
        auth: AuthKind::ApiKey,
        default_env_var: Some("DEEPSEEK_API_KEY"),
        default_headers: &[("Authorization", "Bearer $DEEPSEEK_API_KEY")],
        supports_models_endpoint: true,
        hint: Some("Generate a key at https://platform.deepseek.com/api_keys"),
        use_id_as_default: true,
    },
    ProviderTemplate {
        id: "anthropic",
        display: "Anthropic (Claude API)",
        url: "https://api.anthropic.com/v1",
        auth: AuthKind::ApiKey,
        default_env_var: Some("ANTHROPIC_API_KEY"),
        default_headers: &[
            ("x-api-key", "$ANTHROPIC_API_KEY"),
            ("anthropic-version", "2023-06-01"),
        ],
        supports_models_endpoint: true,
        hint: Some(
            "Sanctioned API-key path. Generate a key at https://console.anthropic.com/settings/keys. Anthropic Pro/Max OAuth passthrough is intentionally not offered (see GOALS §20).",
        ),
        use_id_as_default: true,
    },
    ProviderTemplate {
        id: "xiaomi-mimo",
        display: "Xiaomi MiMo",
        url: "https://platform.xiaomimimo.com/api/v1",
        auth: AuthKind::ApiKey,
        default_env_var: Some("XIAOMI_MIMO_API_KEY"),
        default_headers: &[("Authorization", "Bearer $XIAOMI_MIMO_API_KEY")],
        supports_models_endpoint: true,
        hint: Some(
            "Xiaomi MiMo open platform. Generate a key at https://platform.xiaomimimo.com/. Flagship is MiMo-V2.5-Pro (1M context); MiMo-V2-Flash is the cheap-fast tier.",
        ),
        use_id_as_default: true,
    },
];

pub fn template_by_id(id: &str) -> Option<&'static ProviderTemplate> {
    TEMPLATES.iter().find(|t| t.id == id)
}

/// Materialize the template's default headers into an owned `Vec`.
pub fn default_headers_for(template: &ProviderTemplate) -> Vec<HeaderSpec> {
    template
        .default_headers
        .iter()
        .map(|(n, v)| HeaderSpec {
            name: (*n).to_string(),
            value: (*v).to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_compatible_is_first() {
        assert_eq!(TEMPLATES[0].id, "openai-compatible");
    }

    #[test]
    fn every_template_has_a_display_label() {
        for t in TEMPLATES {
            assert!(!t.display.is_empty(), "template {} missing display", t.id);
        }
    }

    #[test]
    fn lookup_by_id() {
        assert!(template_by_id("z-ai").is_some());
        assert!(template_by_id("minimax").is_some());
        assert!(template_by_id("openrouter").is_some());
        assert!(template_by_id("deepseek").is_some());
        assert!(template_by_id("anthropic").is_some());
        assert!(template_by_id("xiaomi-mimo").is_some());
        assert!(template_by_id("nope").is_none());
    }

    #[test]
    fn default_headers_materialize() {
        let t = template_by_id("opencode-zen").unwrap();
        let h = default_headers_for(t);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].name, "Authorization");
        assert_eq!(h[0].value, "Bearer $OPENCODE_ZEN_TOKEN");
    }
}
