use anyhow::Result;

use crate::cli::InitArgs;

/// Mirrors opencode's `/init` slash command: runs an agent that explores
/// the project and writes the agent-guidance file (whichever name
/// `extended.agent_guidance_files[0]` resolves to, default `AGENTS.md`).
///
/// Deliberately does **not** touch `extended-config.json` or set up
/// providers. Extended config is created lazily by the cockpit-specific
/// commands that need it (`cockpit harness add`, `cockpit redact disable`, …).
pub async fn run(_args: InitArgs) -> Result<()> {
    anyhow::bail!(
        "cockpit init is not implemented yet (planned: run an agent to write AGENTS.md; \
         do NOT write extended-config.json here)"
    )
}
