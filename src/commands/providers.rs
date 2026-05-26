use anyhow::Result;

use crate::cli::ProvidersCommand;

pub async fn run(_cmd: ProvidersCommand) -> Result<()> {
    // OAuth flows here use the same loopback-redirect pattern as
    // mcp2cli-rs's oauth/ module. Tokens persisted to
    // ~/.local/share/cockpit/auth.json.
    anyhow::bail!(
        "cockpit providers is not implemented yet (planned, see opencode-features-review.md §13)"
    )
}
