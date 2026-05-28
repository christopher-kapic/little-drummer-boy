//! Clap definitions for the `cockpit` CLI surface.
//!
//! The shape mirrors opencode's CLI (per `opencode-features-review.md`)
//! plus the `cockpit`-specific additions: `meta`, `connect`, `--agent-file`.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use crate::agents::AgentMode;

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

    /// **Debugging:** write each outbound inference request (system
    /// prompt, tool definitions, history, new prompt, params) as
    /// pretty-printed JSON to `<cwd>/.lastmessage`. Overwritten on
    /// every turn. The file is the *content* we hand to rig, not the
    /// exact serialized HTTP body — rig wraps it on the wire.
    #[arg(long, global = true)]
    pub debug_last_message: bool,

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

    /// Refresh model lists from every configured provider's /models endpoint.
    FetchModels(FetchModelsArgs),

    /// Manage the background daemon (`start`, `stop`, `status`).
    #[command(subcommand)]
    Daemon(DaemonCommand),

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

    /// Manage the package registry the `docs` agent reads from.
    #[command(subcommand)]
    Packages(PackagesCommand),

    /// One-way import of packages from a local `kcl` install's registry.
    #[command(subcommand)]
    Kcl(KclCommand),

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

    /// Force a fresh ephemeral daemon for this run instead of
    /// attaching to a long-running one. The daemon stops as soon as
    /// the run completes. Useful for CI and clean-state scripts.
    #[arg(long)]
    pub ephemeral: bool,
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

// ---- providers / models ----

#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    #[command(alias = "ls")]
    List,
    /// Run an interactive login flow for a provider (currently only
    /// `codex`; static-API-key providers use `$VAR` references in
    /// their header values).
    Login {
        /// Provider id. Today only `codex` is supported.
        provider: Option<String>,
    },
    /// Remove the stored credential for a provider.
    Logout {
        /// Provider id. Today only `codex` is supported.
        provider: Option<String>,
    },
}

#[derive(Debug, clap::Args)]
pub struct ModelsArgs {
    pub provider: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start the daemon (foreground by default; `--detach` spawns a child).
    Start {
        /// Run in the foreground. Used by the wrapper that spawns the
        /// child — you usually want `--detach` from the command line.
        #[arg(long)]
        foreground: bool,
        /// Spawn a detached background daemon and exit immediately.
        #[arg(long)]
        detach: bool,
    },
    /// Stop the running daemon.
    Stop,
    /// Print whether the daemon is running.
    Status,
}

#[derive(Debug, clap::Args)]
pub struct FetchModelsArgs {
    /// Only refresh this provider id.
    #[arg(long, value_name = "ID")]
    pub provider: Option<String>,

    /// `keep` | `remove` — skip the interactive prompt when configured
    /// models drift from the upstream listing.
    #[arg(long, value_name = "POLICY")]
    pub on_unlisted: Option<String>,
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
    /// Session to export: a 6-char `short_id` or a full UUID. Recurses
    /// the fork tree (target + all descendant forks).
    pub session_id: Option<String>,

    /// Output `.zip` path. Defaults to `./cockpit-session-<short_id>.zip`.
    /// Refuses to overwrite an existing file unless `--force`.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Overwrite the output path if it already exists.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, clap::Args)]
pub struct ImportArgs {
    pub file: PathBuf,
}

/// Scope toggle for `cockpit stats` (GOALS §15a / §15f).
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum StatsProjectScope {
    /// The project rooted at the current working directory (default).
    Current,
    /// Every project recorded on this machine.
    All,
}

/// Range toggle for `cockpit stats` (GOALS §15a / §15f).
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum StatsRangeArg {
    /// The last 7 days (default).
    #[value(name = "7d")]
    SevenDays,
    /// All recorded history.
    All,
}

/// Output format for `cockpit stats` (GOALS §15f).
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum StatsFormat {
    /// Human-readable aligned columns (default).
    Table,
    /// Machine-readable JSON (the full roll-up struct).
    Json,
    /// One CSV stream per section, for scripting.
    Csv,
}

#[derive(Debug, clap::Args)]
pub struct StatsArgs {
    /// Which projects to include. (Field id is `project_scope` to avoid
    /// colliding with the global positional `project` path arg; the
    /// user-facing flag stays `--project` per GOALS §15f.)
    #[arg(long = "project", value_enum, default_value_t = StatsProjectScope::Current)]
    pub project_scope: StatsProjectScope,

    /// Time window.
    #[arg(long, value_enum, default_value_t = StatsRangeArg::SevenDays)]
    pub range: StatsRangeArg,

    /// Output format.
    #[arg(long, value_enum, default_value_t = StatsFormat::Table)]
    pub format: StatsFormat,

    /// Add a per-role (agent) token/cost breakdown.
    #[arg(long)]
    pub by_role: bool,
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
    /// **cockpit-specific:** list recent tool calls that hard-failed
    /// (and optionally those that fired any recovery). Surfaces
    /// candidates for the §12 repair catalog.
    FailedCalls(FailedCallsArgs),
    /// Wait indefinitely (for debugging).
    Wait,
}

#[derive(Debug, clap::Args)]
pub struct FailedCallsArgs {
    /// Only failures within the last N days. Default: 7.
    #[arg(long, default_value_t = 7)]
    pub days: u32,
    /// Only this tool name (e.g. `editunlock`, `bash`).
    #[arg(long)]
    pub tool: Option<String>,
    /// Only this model id.
    #[arg(long)]
    pub model: Option<String>,
    /// Project path (resolves to project_id). Defaults to all projects.
    #[arg(long, value_name = "PATH")]
    pub project: Option<PathBuf>,
    /// Max rows. Default: 50.
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
    /// Also include rows that succeeded after a recovery fired (any
    /// non-NULL `recovery_kind`).
    #[arg(long)]
    pub include_recovered: bool,
    /// Emit NDJSON instead of formatted text.
    #[arg(long)]
    pub json: bool,
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

// ---- packages / kcl import ----

#[derive(Debug, Subcommand)]
pub enum PackagesCommand {
    /// List every registered package.
    #[command(alias = "ls")]
    List,
    /// Register a package: `--git <url>` clones (shallow by default);
    /// `--path <dir>` registers a local directory in place.
    Add(PackagesAddArgs),
}

#[derive(Debug, clap::Args)]
pub struct PackagesAddArgs {
    /// Canonical identifier (e.g. `tokio`, `cargo:tokio`, `@scope/pkg`).
    pub identifier: String,
    /// Clone this Git repo into the cockpit clone dir.
    #[arg(long, value_name = "URL")]
    pub git: Option<String>,
    /// Register this existing local directory (no clone).
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,
    /// Branch to clone (Git only).
    #[arg(long)]
    pub branch: Option<String>,
    /// Full (non-shallow) clone. Default is `--depth 1`.
    #[arg(long)]
    pub shallow: bool,
}

#[derive(Debug, Subcommand)]
pub enum KclCommand {
    /// Import every package cockpit lacks from kcl's registry,
    /// referencing kcl's on-disk clone paths as-is.
    Import,
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
