use anyhow::Result;

use crate::cli::StatsArgs;

pub async fn run(_args: StatsArgs) -> Result<()> {
    anyhow::bail!(
        "cockpit stats is not implemented yet (planned; token/cost roll-up from session DB)"
    )
}
