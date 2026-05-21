use anyhow::Result;

use crate::cli::ImportArgs;

pub async fn run(_args: ImportArgs) -> Result<()> {
    // File path only — opencode also accepts share URLs, but cockpit doesn't
    // do hosted sharing (see GOALS.md non-goals).
    todo!("cockpit import — JSON file only, not share URLs")
}
