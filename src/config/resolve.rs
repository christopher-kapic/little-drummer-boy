//! Well-known cockpit paths.
//!
//! Centralized so all callers (loaders, debug commands, init) agree on
//! where files live. Directory discovery for layered configs lives in
//! `config::dirs`; this module is only for the fixed system-level paths.

use std::path::PathBuf;

use anyhow::Result;

/// `~/.local/share/cockpit/` (or platform equivalent).
pub fn cockpit_data_dir() -> Result<PathBuf> {
    todo!()
}

/// `~/.local/state/cockpit/`.
pub fn cockpit_state_dir() -> Result<PathBuf> {
    todo!()
}
