//! One module per top-level subcommand. Each module exposes a single
//! `pub async fn run(...)` that takes the relevant clap args struct.
//!
//! All bodies are stubs (`todo!()`) until the corresponding feature lands.
//! See `GOALS.md` for scope and `opencode-features-review.md` for the
//! feature-by-feature plan.

pub mod agent;
pub mod connect;
pub mod debug;
pub mod export;
pub mod fetch_models;
pub mod import;
pub mod init;
pub mod mcp;
pub mod meta;
pub mod models;
pub mod pr;
pub mod providers;
pub mod run;
pub mod session;
pub mod stats;
pub mod tui;
