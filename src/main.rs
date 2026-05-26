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
mod auth;
mod cli;
mod commands;
mod config;
mod credentials;
mod daemon;
mod engine;
mod envref;
mod git;
mod harness;
mod locks;
mod providers;
mod redact;
mod session;
mod skills;
mod tools;
mod tui;
mod welcome;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.log_level.as_deref(), cli.print_logs);

    if cli.debug_last_message {
        // Resolve `<cwd>/.lastmessage` once at startup so the engine
        // task doesn't depend on `current_dir()` from inside a tokio
        // worker. If cwd resolution fails (rare — chdir to a deleted
        // directory), the flag is a silent no-op and a warning lands
        // in the log.
        match std::env::current_dir() {
            Ok(cwd) => engine::model::enable_debug_last_message(cwd.join(".lastmessage")),
            Err(e) => tracing::warn!(error = %e, "--debug-last-message: cwd unavailable"),
        }
    }

    match cli.command {
        // Bare `cockpit` (no subcommand) launches the TUI in cwd. Mirrors
        // opencode's default behavior.
        None => commands::tui::run(cli.project.as_deref()).await,

        Some(Command::Run(args)) => commands::run::run(args).await,
        Some(Command::Agent(sub)) => commands::agent::run(sub).await,
        Some(Command::Providers(sub)) => commands::providers::run(sub).await,
        Some(Command::Models(args)) => commands::models::run(args).await,
        Some(Command::FetchModels(args)) => commands::fetch_models::run(args).await,
        Some(Command::Daemon(sub)) => commands::daemon::run(sub).await,
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
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "cockpit",
                &mut std::io::stdout(),
            );
            Ok(())
        }
    }
}

fn init_tracing(level: Option<&str>, print_logs: bool) {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = match level {
        Some(l) => EnvFilter::try_new(l).unwrap_or_else(|_| EnvFilter::new("warn")),
        None => EnvFilter::try_from_env("COCKPIT_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
    };

    if print_logs {
        fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
        return;
    }

    // Interactive mode: capture warnings and panics in a file the user
    // can read after closing the TUI. Per `miscellaneous.md` §5 this will
    // grow into a rotating logger under `~/.local/state/cockpit/logs/`;
    // for now a single appended file under the cache dir is enough.
    match open_log_file() {
        Some(file) => {
            fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => {
            fmt()
                .with_env_filter(filter)
                .with_writer(std::io::sink)
                .init();
        }
    }
}

fn open_log_file() -> Option<std::fs::File> {
    let dir = dirs::cache_dir()?.join("cockpit");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("cockpit.log"))
        .ok()
}
