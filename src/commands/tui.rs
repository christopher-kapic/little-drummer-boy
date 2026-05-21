use std::path::Path;

use anyhow::Result;

pub async fn run(_project: Option<&Path>) -> Result<()> {
    // Launches the codex-style TUI. See `tui/` for the ratatui app.
    // Composer: vim mode default-on. Status line: cwd + git branch always.
    todo!("cockpit TUI — see GOALS.md §1 and src/tui/")
}
