//! Clap definitions for the `cockpit` CLI surface.
//!
//! The shape mirrors opencode's CLI (per `opencode-features-review.md`)
//! plus the `cockpit`-specific additions: `meta`, `connect`, `--agent-file`.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

#[derive(Debug, Parser)]
#[command(
    name = "cockpit",
    version,
    about = "AI coding harness with a codex-style TUI",
    propagate_version = true
)]
pub struct Cli {
    /// Optional project path (path to start cockpit in). Mirrors opencode's
    /// positional `[project]`.
    #[arg(global = true)]
    pub project: Option<PathBuf>,

    /// Print logs to stderr instead of dropping them.
    #[arg(long, global = true)]
    pub print_logs: bool,

    /// Log filter: trace / debug / info / warn / error, or a tracing
    /// `EnvFilter` string. Overrides `$COCKPIT_LOG`.
    #[arg(long, global = true, value_name = "LEVEL")]
    pub log_level: Option<String>,

    /// Disable plugins and other external extensions. Accepted for
    /// opencode CLI compatibility; cockpit has no plugins so this is a
    /// no-op.
    #[arg(long, global = true, hide = true)]
    pub pure: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a one-shot prompt non-interactively (matches `opencode run`).
    Run(RunArgs),

    /// Manage agents.
    #[command(subcommand)]
    Agent(AgentCommand),

    /// Manage AI providers and credentials.
    #[command(subcommand, alias = "auth")]
    Providers(ProvidersCommand),

    /// List available models for a provider.
    Models(ModelsArgs),

    /// Manage sessions.
    #[command(subcommand)]
    Session(SessionCommand),

    /// Export session data as JSON.
    Export(ExportArgs),

    /// Import session data from a JSON file.
    Import(ImportArgs),

    /// Show token usage and cost statistics.
    Stats(StatsArgs),

    /// Debug / introspection commands.
    #[command(subcommand)]
    Debug(DebugCommand),

    /// Meta-harness: invoke other harnesses on this device, manage ralph loops.
    Meta(MetaArgs),

    /// MCP is intentionally not supported. See `GOALS.md`.
    #[command(hide = true)]
    Mcp,

    /// Open a remote control session over WebSocket (paid feature; planned).
    Connect(ConnectArgs),

    /// Fetch and check out a GitHub PR, then launch cockpit in the worktree.
    Pr(PrArgs),

    /// Initialize cockpit in this project (writes AGENTS.md and an
    /// extended-config.json skeleton).
    Init(InitArgs),

    /// Generate shell completion script.
    Completion { shell: Shell },
}

// ---- shared arg shapes ----

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable formatted output (default).
    Default,
    /// Newline-delimited JSON events.
    Json,
}

#[derive(Debug, Clone, clap::Args)]
pub struct RunArgs {
    /// Message to send. If absent, read from stdin.
    pub message: Vec<String>,

    /// Use a specific agent. Overrides the project's default.
    #[arg(long)]
    pub agent: Option<String>,

    /// **cockpit-specific:** load an agent definition from an arbitrary file
    /// path. The file does not need to live in `~/.config/opencode/agents/`.
    #[arg(long, value_name = "PATH")]
    pub agent_file: Option<PathBuf>,

    /// Override the model: `provider/model-id`.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Continue the last session.
    #[arg(short, long)]
    pub continue_session: bool,

    /// Continue a specific session id.
    #[arg(short, long, value_name = "ID")]
    pub session: Option<String>,

    /// Fork instead of continuing in place.
    #[arg(long)]
    pub fork: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Default)]
    pub format: OutputFormat,

    /// File(s) to attach to the message.
    #[arg(short, long, value_name = "PATH")]
    pub file: Vec<PathBuf>,

    /// Show thinking blocks.
    #[arg(long)]
    pub thinking: bool,
}

// ---- agent subcommands ----

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Create a new agent file.
    Create {
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, value_enum)]
        mode: Option<AgentMode>,
        /// Comma-separated tool list.
        #[arg(long)]
        tools: Option<String>,
        #[arg(short, long)]
        model: Option<String>,
    },
    /// List all available agents (project + global + extended `agent_dirs`).
    List,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AgentMode {
    All,
    Primary,
    Subagent,
}

// ---- providers / models ----

#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    #[command(alias = "ls")]
    List,
    Login {
        url: Option<String>,
    },
    Logout,
}

#[derive(Debug, clap::Args)]
pub struct ModelsArgs {
    pub provider: Option<String>,
}

// ---- sessions ----

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    List,
    Delete {
        #[arg(value_name = "SESSION_ID")]
        session_id: String,
    },
}

#[derive(Debug, clap::Args)]
pub struct ExportArgs {
    pub session_id: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct ImportArgs {
    pub file: PathBuf,
}

#[derive(Debug, clap::Args)]
pub struct StatsArgs {
    #[arg(long)]
    pub days: Option<u32>,
    #[arg(long)]
    pub tools: Option<u32>,
    #[arg(long)]
    pub models: Option<u32>,
    #[arg(long, value_name = "PATH")]
    pub project: Option<String>,
}

// ---- debug ----

#[derive(Debug, Subcommand)]
pub enum DebugCommand {
    /// Show the resolved configuration.
    Config,
    /// Show the resolved global paths.
    Paths,
    /// List all known projects.
    Scrap,
    /// List all available skills.
    Skill,
    /// Show details for a specific agent.
    Agent { name: String },
    /// File-system debugging utilities.
    File,
    /// **cockpit-specific:** dump the redaction table that would apply to the
    /// next request.
    Redact,
    /// **cockpit-specific:** dump the full prompt (system + tools + history)
    /// that would be sent for the next turn, with token counts. Lets users
    /// audit cockpit's context overhead. See `GOALS.md` §10.
    Context,
    /// Wait indefinitely (for debugging).
    Wait,
}

// ---- meta / connect / pr / init ----

#[derive(Debug, clap::Args)]
pub struct MetaArgs {
    /// Message to seed the meta-harness with. If absent, drop into the TUI.
    pub message: Vec<String>,

    /// Use a specific harness as the meta agent's executor (defaults to cockpit).
    #[arg(long)]
    pub harness: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct ConnectArgs {
    /// Override the relay URL (defaults to the hosted relay).
    #[arg(long)]
    pub relay: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct PrArgs {
    pub number: u32,

    /// Repo override (`owner/name`); defaults to the current repo.
    #[arg(long)]
    pub repo: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Skip prompts and write defaults.
    #[arg(long)]
    pub non_interactive: bool,
    /// Overwrite existing AGENTS.md / extended-config.json.
    #[arg(long)]
    pub force: bool,
}
