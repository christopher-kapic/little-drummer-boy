//! Path resolution for the layered config system.
//!
//! Centralized so all callers (loader, debug commands, init) agree on
//! where files live.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// `~/.config/opencode/` (XDG-style on every platform; see
/// `miscellaneous.md` §1b).
pub fn opencode_global_config_dir() -> Result<PathBuf> {
    todo!("dirs::config_dir().join(\"opencode\")")
}

/// `<project>/.opencode/`.
pub fn opencode_project_dir(_project: &Path) -> PathBuf {
    todo!()
}

/// `~/.local/share/cockpit/` (or platform equivalent).
pub fn cockpit_data_dir() -> Result<PathBuf> {
    todo!()
}

/// `~/.local/state/cockpit/`.
pub fn cockpit_state_dir() -> Result<PathBuf> {
    todo!()
}
