//! Per-provider authentication flows that store tokens in
//! [`crate::credentials::CredentialStore`].
//!
//! Today: Codex (OpenAI device-code flow). GitHub Copilot was migrated
//! off the reverse-engineered device-flow + `/copilot_internal/v2/token`
//! swap and now uses GitHub's documented token sources
//! (`COPILOT_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN` /
//! `GITHUB_COPILOT_API_TOKEN`) plus the documented `COPILOT_API_URL`
//! base-URL override; see [`crate::providers::models_fetch::
//! resolve_provider_request`]. Other providers use static API keys plus
//! `$VAR` references in their header values, so they don't need a flow.
//!
//! `copilot.rs` is intentionally not declared as a module — it remains
//! in the tree only as a historical reference for the (unsupported)
//! token-swap flow it implemented. Removing it entirely is the next
//! cleanup step.

pub mod codex;
pub mod copilot_setup;
