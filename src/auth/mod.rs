//! Per-provider authentication flows that store tokens in
//! [`crate::credentials::CredentialStore`].
//!
//! Today only Codex (OpenAI device-code flow) lives here. Other
//! providers use static API keys + `$VAR` references in their
//! header values, so they don't need a flow.

pub mod codex;
