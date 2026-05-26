use std::path::Path;

use anyhow::Result;

use crate::welcome;

pub async fn run(_project: Option<&Path>) -> Result<()> {
    // Welcome banner (P-51 by default; rooster when COCKPIT_MEME=1).
    // This runs for the bare `cockpit` (interactive TUI) launch path only.
    welcome::print();

    // Real TUI not yet wired (see GOALS.md §1 and src/tui/).
    // For now the banner is the entire behavior of bare `cockpit`.
    eprintln!("(cockpit TUI stub — real interface coming soon)");
    Ok(())
}
