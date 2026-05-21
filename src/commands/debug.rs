use anyhow::Result;

use crate::cli::DebugCommand;

pub async fn run(_cmd: DebugCommand) -> Result<()> {
    // `cockpit debug redact` is cockpit-specific (see GOALS.md §7): dumps the
    // env/.env values that would be substituted out of the next outbound
    // prompt.
    //
    // `cockpit debug context` is cockpit-specific (see GOALS.md §10): dumps the
    // exact prompt + tool descriptions + history that would be sent for
    // the next turn, with per-section token counts. Lets users audit
    // cockpit's context overhead.
    todo!("cockpit debug — config / paths / skill / agent / file / redact / context / wait")
}
