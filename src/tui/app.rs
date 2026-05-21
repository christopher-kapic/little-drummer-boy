//! Top-level TUI state and event loop.

use anyhow::Result;

pub struct App {
    // Conversation log, scroll position, active agent, pending tool
    // approvals, etc. Filled in once the providers + session storage land.
}

impl App {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn run(&mut self) -> Result<()> {
        todo!("crossterm raw mode, ratatui Terminal, event loop")
    }
}
