use anyhow::Result;

use crate::cli::ProvidersCommand;

pub async fn run(_cmd: ProvidersCommand) -> Result<()> {
    // OAuth flows here use the same loopback-redirect pattern as
    // mcp2cli-rs's oauth/ module. Tokens persisted to opencode's
    // ~/.local/share/opencode/auth.json for compatibility.
    todo!("cockpit providers — see opencode-features-review.md §13")
}
