//! ratatui TUI app.
//!
//! Modeled on codex (see `kcl ask codex`). Key components:
//!   - `app`           top-level state machine + event loop
//!   - `composer`      bottom input area; vim mode default-on (GOALS §1b)
//!   - `chrome`        status line — always shows cwd + git branch (GOALS §1a)
//!   - `chat`          scrollback of user/assistant turns
//!   - `slash`         leader-less slash menu
//!   - `theme`         color palette, opencode-compatible
//!
//! Implementation guidance: codex's `bottom_pane/textarea.rs` has a
//! battle-tested vim state machine — port the structure rather than
//! reinventing it.

pub mod agent_runner;
pub mod app;
pub mod chat;
pub mod chrome;
pub mod composer;
pub mod daemon_prompt;
pub mod file_tag;
pub mod geometry;
pub mod history;
pub mod markdown;
pub mod model_picker;
pub mod settings;
pub mod slash;
pub mod textfield;
pub mod theme;
