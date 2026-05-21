use anyhow::Result;

use crate::cli::PrArgs;

pub async fn run(_args: PrArgs) -> Result<()> {
    // Wraps `gh pr checkout` and then launches the cockpit TUI in the
    // resulting worktree. Mirrors `opencode pr <number>`.
    todo!("cockpit pr — wrapper around `gh pr checkout` + TUI launch")
}
