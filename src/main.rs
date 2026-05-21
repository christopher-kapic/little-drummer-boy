//! `cockpit` — entry point.
//!
//! Most actual logic lives in the per-subcommand files in `commands/`. This
//! file only does:
//! 1. Parse argv with clap.
//! 2. Initialize logging.
//! 3. Dispatch to the matching command.
//!
//! See `GOALS.md` for what the binary is for.

mod agents;
mod cli;
mod commands;
mod config;
mod git;
mod harness;
mod redact;
mod skills;
mod tui;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.log_level.as_deref(), cli.print_logs);

    match cli.command {
        // Bare `cockpit` (no subcommand) launches the TUI in cwd. Mirrors
        // opencode's default behavior.
        None => commands::tui::run(cli.project.as_deref()).await,

        Some(Command::Run(args)) => commands::run::run(args).await,
        Some(Command::Agent(sub)) => commands::agent::run(sub).await,
        Some(Command::Providers(sub)) => commands::providers::run(sub).await,
        Some(Command::Models(args)) => commands::models::run(args).await,
        Some(Command::Session(sub)) => commands::session::run(sub).await,
        Some(Command::Export(args)) => commands::export::run(args).await,
        Some(Command::Import(args)) => commands::import::run(args).await,
        Some(Command::Stats(args)) => commands::stats::run(args).await,
        Some(Command::Debug(sub)) => commands::debug::run(sub).await,
        Some(Command::Meta(args)) => commands::meta::run(args).await,
        Some(Command::Mcp) => commands::mcp::run().await,
        Some(Command::Connect(args)) => commands::connect::run(args).await,
        Some(Command::Pr(args)) => commands::pr::run(args).await,
        Some(Command::Init(args)) => commands::init::run(args).await,
        Some(Command::Completion { shell }) => {
            use clap::CommandFactory;
            clap_complete::generate(shell, &mut Cli::command(), "cockpit", &mut std::io::stdout());
            Ok(())
        }
    }
}

fn init_tracing(level: Option<&str>, print_logs: bool) {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = match level {
        Some(l) => EnvFilter::new(l.to_lowercase()),
        None => EnvFilter::try_from_env("COCKPIT_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
    };

    let builder = fmt().with_env_filter(filter).with_writer(std::io::stderr);

    if print_logs {
        builder.init();
    } else {
        // Tracing is silently dropped in interactive mode unless the user
        // asked for `--print-logs`. Per miscellaneous.md §5 we will also
        // mirror to a rotating file in `~/.local/state/cockpit/logs/` once
        // implemented; that wiring goes here.
        builder.with_writer(std::io::sink).init();
    }
}
