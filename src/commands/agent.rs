use anyhow::Result;

use crate::cli::AgentCommand;

pub async fn run(_cmd: AgentCommand) -> Result<()> {
    // `cockpit agent create` and `cockpit agent list`. List walks opencode's
    // standard locations plus extended-config `agent_dirs`.
    todo!("cockpit agent — see opencode-features-review.md §4")
}
