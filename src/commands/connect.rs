use anyhow::Result;

use crate::cli::ConnectArgs;

pub async fn run(_args: ConnectArgs) -> Result<()> {
    // Future: open a WebSocket to the hosted relay so a phone client can
    // mirror this TUI. Not implemented in v1. See GOALS.md §8.
    anyhow::bail!("cockpit connect is not implemented yet (planned, see GOALS.md §8)")
}
