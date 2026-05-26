use anyhow::Result;

use crate::cli::ExportArgs;

pub async fn run(_args: ExportArgs) -> Result<()> {
    anyhow::bail!("cockpit export is not implemented yet (planned)")
}
