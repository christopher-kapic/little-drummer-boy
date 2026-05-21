use anyhow::Result;

use crate::cli::MetaArgs;

pub async fn run(_args: MetaArgs) -> Result<()> {
    // The meta-harness loads its own agent file (shipped with cockpit) and
    // exposes a small set of built-in tools:
    //   - harness_invoke(name, prompt, agent_file?, model?)
    //   - ralph_list / ralph_show / ralph_run / ralph_resume / ralph_cancel
    //   - cockpit_subagent(prompt, agent?)
    // See GOALS.md §6.
    todo!("cockpit meta — meta-harness over other CLIs and ralph loops")
}
