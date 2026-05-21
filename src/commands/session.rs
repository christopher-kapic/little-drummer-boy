use anyhow::Result;

use crate::cli::SessionCommand;

pub async fn run(_cmd: SessionCommand) -> Result<()> {
    todo!("cockpit session — list/delete; backed by ~/.local/share/cockpit/cockpit.db")
}
