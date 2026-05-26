//! Configuration loaders for `cockpit`.
//!
//! cockpit reads its own config files in its own locations — see
//! `CLAUDE.md` "Design rules" and the [[config_layering]] plan. It does
//! **not** parse `opencode.json` or any `.opencode/` directory.
//!
//! Layers:
//!
//! - `config.json` in a discovered `.cockpit/` directory — see
//!   `dirs::discover_config_dirs` for the walk order.
//! - `extended-config.json` — the cockpit-only superset described in
//!   `GOALS.md` §4. Schema lives in `extended.rs`.

pub mod dirs;
pub mod extended;
pub mod provider;
pub mod resolve;
