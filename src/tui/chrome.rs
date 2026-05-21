//! TUI status line / chrome.
//!
//! Per `GOALS.md` §1a, the chrome **always** shows:
//!   - The current working directory (abbreviated if it overflows).
//!   - The git branch (with a leading `` glyph) when the cwd is in a
//!     git repo. When not in a repo, no slot — no placeholder text.
//!
//! Other slots (active agent, model, token count, …) compose around
//! these two.

use std::path::Path;

/// Render the cwd slot. Abbreviates middle path components if the
/// rendered width would exceed `max_width`. Examples (max_width=20):
///   `/home/christopher/projects/device-ai/cockpit-cli`
///     -> `~/p/d/cockpit-cli`
pub fn cwd_label(_cwd: &Path, _max_width: usize) -> String {
    todo!()
}

/// Render the branch slot. Returns `None` when not in a repo.
pub fn branch_label(_cwd: &Path) -> Option<String> {
    todo!("call git::current_branch and prefix with `` symbol")
}
