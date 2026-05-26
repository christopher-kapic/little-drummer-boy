use anyhow::Result;

use crate::cli::ModelsArgs;

pub async fn run(_args: ModelsArgs) -> Result<()> {
    anyhow::bail!("cockpit models is not implemented yet (planned)")
}
