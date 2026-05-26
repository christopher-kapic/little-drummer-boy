//! Per-provider authentication flows that store tokens in
//! [`crate::credentials::CredentialStore`].
//!
//! Today: Codex (OpenAI device-code flow) and GitHub Copilot (device
//! flow + internal token swap). Other providers use static API keys +
//! `$VAR` references in their header values, so they don't need a flow.

pub mod codex;
pub mod copilot;
