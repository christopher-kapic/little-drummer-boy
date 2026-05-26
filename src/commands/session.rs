use anyhow::Result;

use crate::cli::SessionCommand;

pub async fn run(_cmd: SessionCommand) -> Result<()> {
    anyhow::bail!(
        "cockpit session is not implemented yet (planned; backed by ~/.local/share/cockpit/cockpit.db)"
    )
}
