use anyhow::Result;

use crate::cli::RunArgs;

pub async fn run(_args: RunArgs) -> Result<()> {
    // Plan:
    // 1. Resolve project root (cwd or --project).
    // 2. Load config (opencode + extended-config) for that project.
    // 3. Resolve agent: --agent-file > --agent > project default.
    // 4. Build the prompt (message argv + stdin + --file attachments + AGENTS.md).
    // 5. Pass through `redact::scrub()`.
    // 6. Send to provider (streaming).
    // 7. Render per --format.
    todo!("cockpit run — see GOALS.md §2 and opencode-features-review.md §1")
}
