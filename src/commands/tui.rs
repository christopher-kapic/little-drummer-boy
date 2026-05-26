use std::io::{IsTerminal, stdin, stdout};
use std::path::Path;

use anyhow::Result;

use crate::tui::app::App;
use crate::welcome;

pub async fn run(project: Option<&Path>) -> Result<()> {
    if !stdin().is_terminal() || !stdout().is_terminal() {
        welcome::print(project);
        return Ok(());
    }

    let mut app = App::new(project);
    app.run().await
}
