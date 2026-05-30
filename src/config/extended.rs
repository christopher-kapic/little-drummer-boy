//! Loader for `extended-config.json` — the cockpit-only config layer.
//!
//! Lives alongside `config.json` in each discovered `.cockpit/` directory
//! (see `config::dirs`). Schema reference: `GOALS.md` §4. All fields are
//! optional; a missing file is fine (defaults apply).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendedConfig {
    #[serde(default)]
    pub harnesses: HashMap<String, HarnessConfig>,

    /// Ordered list of agent-guidance file names. The first file from this
    /// list that exists in the cwd (or its ancestors up to the git root)
    /// is loaded. Default: `["AGENTS.md", "CLAUDE.md"]`.
    #[serde(default = "default_agent_guidance_files")]
    pub agent_guidance_files: Vec<String>,

    /// Concurrency model when an agent fans out: `"subagents"` (in-process)
    /// or `"fork"` (separate cockpit/other-harness subprocess per sub-task).
    #[serde(default)]
    pub concurrency: Concurrency,

    /// Extra directories to search for agent definition files. Paths are
    /// tilde-expanded.
    #[serde(default)]
    pub agent_dirs: Vec<PathBuf>,

    #[serde(default)]
    pub redact: RedactConfig,

    #[serde(default)]
    pub tui: TuiConfig,

    /// User's display name. When set, the startup logo shows
    /// `Welcome, {name}` between the title line and the provider line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Where the docs agent stores its package snapshots. Tilde-expanded
    /// at read time. Absent means the agent picks its own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages_directory: Option<PathBuf>,

    /// User-defined bash-command templates surfaced as built-in tools
    /// (webfetch, websearch, …). Keyed by tool name.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tools: HashMap<String, ToolCommandTemplate>,

    /// Opt-in to fetching remote `.well-known/cockpit` configs.
    #[serde(default)]
    pub allow_remote_config: bool,

    /// Utility model used for background work that doesn't need the
    /// primary model: session auto-titling (GOALS §17d), the
    /// prompt-injection guard when enabled, and similar small tasks.
    /// Identifier format mirrors the primary model selector
    /// (`"<provider>:<model-id>"`). Unset disables every
    /// utility-model-dependent feature — auto-titling is skipped and
    /// sessions display their short id as the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utility_model: Option<String>,

    /// Prompt-injection guard config (GOALS §4i). Off by default; v1
    /// scope is user-authored input only.
    #[serde(default)]
    pub prompt_injection_guard: PromptInjectionGuardConfig,

    /// System-prompt injection knobs (GOALS §17g, §4k).
    #[serde(default)]
    pub system_prompt: SystemPromptConfig,

    /// Async-jobs subsystem knobs (GOALS §22).
    #[serde(default)]
    pub jobs: JobsConfig,

    /// Loop-guard knobs: the back-to-back identical tool-call threshold.
    #[serde(default)]
    pub loop_guard: LoopGuardConfig,

    /// Answering-dialog knobs (GOALS §3b) — shared by the `question`
    /// tool today and tool-approval prompts later.
    #[serde(default)]
    pub dialog: DialogConfig,

    /// Skills subsystem knobs (GOALS §5): scan directories and the
    /// auto-`!`-command toggle.
    #[serde(default)]
    pub skills: SkillsConfig,

    /// Branch-name prefix for suggested plan branches (`plan.md` §4.1).
    /// The planning flow (`Plan`) suggests a plan's target
    /// branch as `${planBranchRoot}/<feature-branch>`. Default
    /// `"cockpit-plan"`.
    #[serde(rename = "planBranchRoot", default = "default_plan_branch_root")]
    pub plan_branch_root: String,

    /// Global default filesystem-isolation mode for new plans (`plan.md`
    /// §4.1 Q4c, resolved by prompt 4). `worktree` (the default) runs each
    /// parallel step in its own git worktree behind a serial merge queue;
    /// `shared_tree` is the per-plan opt-out that runs all steps in one tree
    /// serialized by the file-lock manager. The authoring flow seeds a new
    /// plan's `isolation_mode` from this; `/settings` exposes the toggle.
    #[serde(rename = "defaultIsolationMode", default)]
    pub default_isolation_mode: IsolationModeSetting,

    /// The LLM-strength steering axis (`prompts/llm-modes-defensive-normal.md`).
    /// `defensive` (the default) renders explicit, steering tool/parameter
    /// descriptions, selects `defensive.md` per-mode agent prompts, and routes
    /// multi-part work through interactive subagents — tuned for the weak-model
    /// target (GOALS §1). `normal` keeps the terse token-economy descriptions,
    /// `normal.md` prompts, and episode-sequencing delegation. Distinct from
    /// [`crate::agents::AgentMode`] (`primary`/`subagent`/`all` reachability) —
    /// not auto-inferred from model identity. An unknown value is rejected with
    /// the offending value backticked and the valid set listed.
    #[serde(default, deserialize_with = "deserialize_llm_mode")]
    pub llm_mode: LlmMode,
}

/// The LLM-strength steering axis (`prompts/llm-modes-defensive-normal.md`).
/// The only thing called a *mode* in cockpit's agent surface; `Plan` and
/// `Build` are agents, not modes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum LlmMode {
    /// Cheaper/weaker ~120k-context models (the default, GOALS §1 target):
    /// explicit steering descriptions, `defensive.md` prompts, interactive-
    /// subagent decomposition.
    #[default]
    Defensive,
    /// Strong/expensive models: terse descriptions, `normal.md` prompts,
    /// episode-sequencing delegation.
    Normal,
}

impl LlmMode {
    /// The on-disk per-mode agent-prompt file name (`<name>/<mode>.md`).
    pub fn prompt_file(self) -> &'static str {
        match self {
            LlmMode::Defensive => "defensive.md",
            LlmMode::Normal => "normal.md",
        }
    }

    /// The lowercase config/serde spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            LlmMode::Defensive => "defensive",
            LlmMode::Normal => "normal",
        }
    }

    /// Flip between the two values — the `/llm-mode toggle` action.
    pub fn toggled(self) -> Self {
        match self {
            LlmMode::Defensive => LlmMode::Normal,
            LlmMode::Normal => LlmMode::Defensive,
        }
    }
}

/// Reject an unknown `llm_mode` with the offending value backticked and
/// the valid set listed — mirrors [`deserialize_vim_mode_setting`]'s
/// error style.
fn deserialize_llm_mode<'de, D>(d: D) -> Result<LlmMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Null => Ok(LlmMode::default()),
        serde_json::Value::String(s) => match s.as_str() {
            "defensive" => Ok(LlmMode::Defensive),
            "normal" => Ok(LlmMode::Normal),
            other => Err(D::Error::custom(format!(
                "unknown llm_mode `{other}` (expected defensive|normal)"
            ))),
        },
        _ => Err(D::Error::custom("llm_mode must be a string")),
    }
}

fn default_plan_branch_root() -> String {
    "cockpit-plan".to_string()
}

/// Global default plan isolation mode (`plan.md` §4.1 Q4c). Mirrors
/// [`crate::db::plans::IsolationMode`] at the config layer; the authoring
/// flow translates this into the per-plan stored mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IsolationModeSetting {
    /// One git worktree per parallel step + a serial merge queue (default —
    /// the resolved Q4c decision).
    #[default]
    Worktree,
    /// All steps share one working tree, serialized by the file-lock
    /// manager; no worktrees, no merge queue.
    SharedTree,
}

impl IsolationModeSetting {
    /// Flip between the two values — the `/settings` toggle action.
    pub fn toggled(self) -> Self {
        match self {
            IsolationModeSetting::Worktree => IsolationModeSetting::SharedTree,
            IsolationModeSetting::SharedTree => IsolationModeSetting::Worktree,
        }
    }
}

impl From<IsolationModeSetting> for crate::db::plans::IsolationMode {
    fn from(s: IsolationModeSetting) -> Self {
        match s {
            IsolationModeSetting::Worktree => crate::db::plans::IsolationMode::Worktree,
            IsolationModeSetting::SharedTree => crate::db::plans::IsolationMode::SharedTree,
        }
    }
}

/// The two scan-dir entries a brand-new install ships pre-seeded with
/// (the "fresh install" defaults). These are materialized as ordinary,
/// editable/removable rows the first time skills config is loaded with no
/// `extended-config.json` anywhere on disk — they are **not** an
/// implicit resolve-time fallback. An empty `scan_dirs` always resolves
/// to zero directories. The relative `./.agents/skills` entry resolves
/// against cwd (and, with [`SkillsConfig::ancestor_walk`] on, every
/// ancestor up to the git worktree root).
pub const SEEDED_SCAN_DIRS: [&str; 2] = ["~/.agents/skills", "./.agents/skills"];

/// Skills subsystem config (GOALS §5).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillsConfig {
    /// Directories scanned for `<name>/SKILL.md`. Each entry supports `~`
    /// home expansion, `$VAR` references, and relative paths resolved
    /// against cwd. The list ships pre-seeded on a fresh install with
    /// [`SEEDED_SCAN_DIRS`] (`~/.agents/skills` + `./.agents/skills`) as
    /// ordinary editable rows; an empty list scans **nothing** — there
    /// is no implicit "empty = defaults" fallback. Relative entries
    /// resolve against cwd, or against cwd plus every ancestor up to the
    /// git worktree root when [`Self::ancestor_walk`] is enabled.
    #[serde(default)]
    pub scan_dirs: Vec<String>,

    /// Auto-`!`-command toggle. `true` = Claude mode (inline
    /// `` !`command` `` directives in a skill body run, their stdout
    /// replaces the directive — scrubbed before entering context).
    /// `false` (default) = Codex mode (directives injected verbatim; the
    /// command never runs). Default disabled: auto-running shell is a
    /// footgun; correctness/safety over convenience.
    #[serde(default)]
    pub auto_bang_commands: bool,

    /// Ancestor-walk toggle for **relative** scan-dir entries. `false`
    /// (default): a relative entry resolves against cwd only. `true`:
    /// each relative entry expands at resolve time to cwd **plus** every
    /// ancestor directory up to and including the git worktree root, so a
    /// repo-root `./.agents/skills` is found from any subdirectory.
    /// Absolute / `~` / `$VAR`-rooted entries are unaffected.
    #[serde(default)]
    pub ancestor_walk: bool,
}

/// Answering-dialog config (GOALS §3b). Governs the reusable selectable-
/// pages dialog that the `question` tool — and, later, tool-approval
/// prompts — present over the composer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialogConfig {
    /// Anti-misfire lockout: how long (milliseconds) the dialog ignores
    /// input after it appears, so a user who was mid-typing in the
    /// composer can't accidentally answer. The border renders grey
    /// during the lockout and white once it elapses. Default 1500 ms.
    #[serde(default = "default_dialog_lockout_ms")]
    pub lockout_ms: u64,
}

impl Default for DialogConfig {
    fn default() -> Self {
        Self {
            lockout_ms: default_dialog_lockout_ms(),
        }
    }
}

fn default_dialog_lockout_ms() -> u64 {
    1500
}

/// Async-jobs subsystem config (GOALS §22).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobsConfig {
    /// Cap on concurrently-running async jobs per session. Guards against
    /// accidental fan-out (the fork-can't-spawn rule prevents recursion).
    #[serde(default = "default_max_concurrent_jobs")]
    pub max_concurrent: usize,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_max_concurrent_jobs(),
        }
    }
}

fn default_max_concurrent_jobs() -> usize {
    crate::engine::jobs::DEFAULT_MAX_CONCURRENT_JOBS
}

/// Loop-guard config: the approval prompt that fires on back-to-back
/// identical tool calls. A model that re-issues the *exact same* call
/// (tool name + canonical `wire_input`) as the immediately-preceding one
/// is likely stuck in a loop; cockpit pauses for approval rather than
/// burning the context window re-running it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopGuardConfig {
    /// Number of consecutive identical tool calls before the approval
    /// prompt fires. Counts the run that triggers it: `2` (the default)
    /// fires on the first exact repeat. A value `< 2` is clamped to `2`
    /// at read time ([`Self::effective_threshold`]) — the guard is only
    /// meaningful for a *repeat*.
    #[serde(default = "default_loop_guard_threshold")]
    pub repeat_threshold: u32,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            repeat_threshold: default_loop_guard_threshold(),
        }
    }
}

impl LoopGuardConfig {
    /// The threshold actually applied, clamped to a minimum of 2. The
    /// guard compares against the immediately-preceding call only, so a
    /// threshold below 2 (which would "fire on the first call ever") is
    /// nonsensical and floored to 2.
    pub fn effective_threshold(&self) -> u32 {
        self.repeat_threshold.max(MIN_LOOP_GUARD_THRESHOLD)
    }
}

/// Minimum (and default) consecutive-call count before the loop-guard
/// prompt fires. `2` = fire on the first exact repeat.
pub const MIN_LOOP_GUARD_THRESHOLD: u32 = 2;

fn default_loop_guard_threshold() -> u32 {
    MIN_LOOP_GUARD_THRESHOLD
}

/// Prompt-injection guard config. The substance is deferred (see
/// `flagged-for-christopher.md`); this struct exists so the config
/// schema is forward-compatible with v1.5.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptInjectionGuardConfig {
    /// Master enable. Defaults false.
    #[serde(default)]
    pub enabled: bool,
    /// Model used for the classification call. When None, falls back
    /// to [`ExtendedConfig::utility_model`]; if both are unset and
    /// `enabled = true`, the guard logs a one-time warning and
    /// behaves as disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// System-prompt assembly knobs (GOALS §17g).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemPromptConfig {
    /// Minimum gap (in minutes) between `[time: ...]` preludes on
    /// user messages. The first user message always carries a
    /// prelude; subsequent messages get one only when this many
    /// minutes have elapsed since the last. The system prompt
    /// itself never carries the time.
    #[serde(default = "default_time_injection_interval")]
    pub time_injection_interval_minutes: u32,
}

impl Default for SystemPromptConfig {
    fn default() -> Self {
        Self {
            time_injection_interval_minutes: default_time_injection_interval(),
        }
    }
}

fn default_time_injection_interval() -> u32 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub prompt_mode: PromptMode,
    #[serde(default)]
    pub model_args: Vec<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub supports_skills: bool,
    #[serde(default)]
    pub supports_agent_file: bool,
    #[serde(default)]
    pub agent_file_args: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    #[default]
    Arg,
    Stdin,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Concurrency {
    #[default]
    Subagents,
    Fork,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactConfig {
    pub enabled: bool,
    pub scan_environment: bool,
    pub scan_dotenv: bool,
    #[serde(default)]
    pub extra_dotenv_paths: Vec<PathBuf>,
    pub min_secret_length: usize,
    pub placeholder: String,
    /// User-supplied literal values that must *always* be redacted, even
    /// if shorter than `min_secret_length` or sourced from an
    /// allowlisted env var. Per spec §2b merging.
    #[serde(default)]
    pub denylist: Vec<String>,
    /// User-supplied env var names to *exclude* from the redaction
    /// table on top of the built-in `ENV_ALLOWLIST` in `redact::mod`.
    #[serde(default)]
    pub allowlist: Vec<String>,
}

impl Default for RedactConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_environment: true,
            scan_dotenv: true,
            extra_dotenv_paths: vec![],
            min_secret_length: 8,
            placeholder: "**REDACTED BY COCKPIT - DO NOT TRY TO OBTAIN BY WORKAROUND**".to_string(),
            denylist: vec![],
            allowlist: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    #[serde(default, deserialize_with = "deserialize_vim_mode_setting")]
    pub vim_mode: VimModeSetting,
    #[serde(default)]
    pub thinking: ThinkingDisplay,
    /// Render assistant output through the markdown emitter. Default
    /// on — chat models routinely emit fenced code, bullets, bold.
    #[serde(default = "default_true")]
    pub render_agent_markdown: bool,
    /// Render the user's own message bubble through the markdown
    /// emitter. Default off — most user prompts are plain prose; turning
    /// this on is opt-in for users who paste markdown into the composer.
    #[serde(default)]
    pub render_user_markdown: bool,
    pub show_cwd: bool,
    pub show_branch: bool,
    /// Pixel banner on TUI startup (GOALS §1g). Default on; suppressed
    /// when stdout is not a TTY, `NO_COLOR` is set, the window is
    /// narrower than the art, or `COCKPIT_ROOSTER=1` preempts it.
    #[serde(default)]
    pub banner: BannerConfig,
    /// How `edit` / `editunlock` (and, later, `write` /
    /// `writeunlock`) tool calls render their changes in the history
    /// pane. SideBySide degrades to Inline when the terminal is
    /// narrower than 80 columns.
    #[serde(default)]
    pub diff_style: DiffStyle,
    /// Capture mouse events. With capture on we get click-to-position
    /// in the composer, drag-select in chat history, and clickable
    /// chips. Native terminal selection requires holding the
    /// terminal's bypass modifier (Shift / Option / Fn) while
    /// capture is on; we provide in-app drag-select + Ctrl+Shift+C
    /// for the common path.
    #[serde(default = "default_true")]
    pub mouse_capture: bool,
    /// Allow `Ctrl+Shift+Y` to copy the focused agent message as
    /// rich text (HTML to the system clipboard via the local OS
    /// clipboard layer; falls back to plain text over SSH).
    #[serde(default = "default_true")]
    pub rich_text_copy: bool,
    /// Lines of conversation tail to dump back into terminal
    /// scrollback at TUI exit (GOALS §1d). Default 100. `0` disables
    /// the dump entirely; `-1` dumps the whole session.
    #[serde(default = "default_exit_tail_lines")]
    pub exit_tail_lines: i32,
    /// Use emoji glyphs in the chat (tool-call boxes, the rooster
    /// splash, …). Default off — many terminals can't render emoji and
    /// show tofu boxes instead, so cockpit ships text-only and lets the
    /// user opt in.
    #[serde(default)]
    pub use_emojis: bool,
    /// `/caffeinate` display scope. When `true`, an active caffeination
    /// also keeps the display awake; default `false` keeps only the
    /// machine awake (and prevents lid-close suspend) while letting the
    /// display turn off — saves screen wear/power on overnight runs.
    /// System-idle + lid-close prevention are always on while caffeinated
    /// regardless of this; the setting only governs the display.
    #[serde(default)]
    pub caffeinate_display_awake: bool,
}

/// Sleep scope `/caffeinate` keeps awake — derived from the
/// `caffeinate_display_awake` UI setting. System-idle + lid-close are
/// always suppressed while caffeinated; this only governs the display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepScope {
    /// Keep the machine awake + prevent lid-close suspend; let the display
    /// turn off (default).
    SystemOnly,
    /// Also keep the display on.
    SystemAndDisplay,
}

impl TuiConfig {
    /// The `/caffeinate` sleep scope implied by the display-awake setting.
    pub fn sleep_scope(&self) -> SleepScope {
        if self.caffeinate_display_awake {
            SleepScope::SystemAndDisplay
        } else {
            SleepScope::SystemOnly
        }
    }
}

fn default_exit_tail_lines() -> i32 {
    100
}

/// Diff rendering mode for edit/write tool calls.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DiffStyle {
    /// Two columns: old text on the left, new on the right, separated
    /// by a vertical rule. Falls back to [`Self::Inline`] dynamically
    /// when the terminal is narrower than 80 columns.
    #[default]
    SideBySide,
    /// Unified diff: `-` red for removed lines, `+` green for added,
    /// ` ` for context.
    Inline,
    /// Show only a one-line summary (`edited {path} (+N -M)`).
    Hidden,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BannerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for BannerConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// How reasoning/thinking is surfaced in the chat pane.
///
/// `Condensed` (default) — show a clickable "thought for Xs" chip that
/// expands to the full reasoning on click.
/// `Hidden` — show only the live "Thinking…" placeholder; once the turn
/// finalizes, the chip and reasoning are not rendered at all.
/// `Verbose` — always render the full reasoning text inline (as if every
/// entry were pre-expanded).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    #[default]
    Condensed,
    Hidden,
    Verbose,
}

/// One user-defined bash-command tool. Placeholder substitution uses
/// `{name}` markers (matched against the tool's declared arg list at
/// dispatch time). Stored under `tools.<tool-name>` in extended-config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCommandTemplate {
    /// Enable/disable this tool without deleting its config.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Bash command template with `{placeholder}` substitution.
    /// E.g. `curl -sSL --max-time 15 {url}` for `webfetch`.
    pub command: String,
    /// One-sentence description shown to the model. Kept terse to
    /// respect the token-economy rule (CLAUDE.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Tri-state vim mode: `hint` (default; vim enabled, hint shown on
/// entry to Normal), `enabled` (vim on, no hint), `disabled` (vim off).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum VimModeSetting {
    #[default]
    Hint,
    Enabled,
    Disabled,
}

impl VimModeSetting {
    pub fn vim_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub fn show_hint(self) -> bool {
        matches!(self, Self::Hint)
    }
}

/// Accept the legacy `vim_mode: bool` schema as well as the new
/// string enum. `true` maps to `Hint` (the default), `false` to
/// `Disabled`. Lets us roll the schema forward without breaking
/// existing configs on disk.
fn deserialize_vim_mode_setting<'de, D>(d: D) -> Result<VimModeSetting, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Bool(true) => Ok(VimModeSetting::Hint),
        serde_json::Value::Bool(false) => Ok(VimModeSetting::Disabled),
        serde_json::Value::String(s) => match s.as_str() {
            "hint" => Ok(VimModeSetting::Hint),
            "enabled" => Ok(VimModeSetting::Enabled),
            "disabled" => Ok(VimModeSetting::Disabled),
            other => Err(D::Error::custom(format!(
                "unknown vim_mode `{other}` (expected hint|enabled|disabled)"
            ))),
        },
        serde_json::Value::Null => Ok(VimModeSetting::default()),
        _ => Err(D::Error::custom("vim_mode must be a string or bool")),
    }
}

impl Default for ExtendedConfig {
    fn default() -> Self {
        Self {
            harnesses: HashMap::new(),
            agent_guidance_files: default_agent_guidance_files(),
            concurrency: Concurrency::default(),
            agent_dirs: Vec::new(),
            redact: RedactConfig::default(),
            tui: TuiConfig::default(),
            name: None,
            packages_directory: None,
            tools: HashMap::new(),
            allow_remote_config: false,
            utility_model: None,
            prompt_injection_guard: PromptInjectionGuardConfig::default(),
            system_prompt: SystemPromptConfig::default(),
            jobs: JobsConfig::default(),
            loop_guard: LoopGuardConfig::default(),
            dialog: DialogConfig::default(),
            skills: SkillsConfig::default(),
            plan_branch_root: default_plan_branch_root(),
            default_isolation_mode: IsolationModeSetting::default(),
            llm_mode: LlmMode::default(),
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            vim_mode: VimModeSetting::default(),
            thinking: ThinkingDisplay::default(),
            render_agent_markdown: true,
            render_user_markdown: false,
            show_cwd: true,
            show_branch: true,
            banner: BannerConfig::default(),
            diff_style: DiffStyle::default(),
            mouse_capture: true,
            rich_text_copy: true,
            exit_tail_lines: default_exit_tail_lines(),
            use_emojis: false,
            caffeinate_display_awake: false,
        }
    }
}

fn default_agent_guidance_files() -> Vec<String> {
    vec!["AGENTS.md".into()]
}

/// Load the effective [`ExtendedConfig`] for `cwd`: the first parseable
/// `extended-config.json` on the layered-config walk, or — when **none**
/// exists anywhere (a genuinely *fresh install*) — `Default` with the
/// skills scan-dir list seeded to [`SEEDED_SCAN_DIRS`]. Best-effort:
/// unparseable layers are skipped.
///
/// The fresh-install distinction is made here, at the *file-existence*
/// level: an absent file and an existing empty `{}` both parse to an
/// empty `scan_dirs`, so they can't be told apart after parse. The
/// seeding is materialization-only — it never happens for an existing
/// on-disk config whose `scan_dirs` is absent/empty (clean break: scan
/// nothing).
pub fn load_for_cwd(cwd: &Path) -> ExtendedConfig {
    use crate::config::dirs::discover_config_dirs;
    for dir in discover_config_dirs(cwd) {
        let path = dir.path.join("extended-config.json");
        if path.exists()
            && let Ok(doc) = ExtendedConfigDoc::load(&path)
        {
            return doc.config();
        }
    }
    // Fresh install: no extended-config on disk. Materialize the seeded
    // skills scan-dirs so new users discover (and see in `/settings`) the
    // default skill directories.
    let mut cfg = ExtendedConfig::default();
    cfg.skills.scan_dirs = SEEDED_SCAN_DIRS.iter().map(|s| s.to_string()).collect();
    cfg
}

/// Round-trip loader/saver for `extended-config.json` that preserves
/// unknown fields. Same pattern as [`crate::config::providers::ConfigDoc`]:
/// the raw `Value` is held alongside the typed view so writes don't
/// destroy fields a future cockpit version added.
pub struct ExtendedConfigDoc {
    pub path: PathBuf,
    raw: Value,
}

impl ExtendedConfigDoc {
    pub fn load(path: &Path) -> Result<Self> {
        let raw_str = if path.exists() {
            std::fs::read_to_string(path)
                .with_context(|| format!("reading extended-config.json at {}", path.display()))?
        } else {
            "{}".to_string()
        };
        let raw: Value = if raw_str.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_str(&raw_str)
                .with_context(|| format!("parsing extended-config.json at {}", path.display()))?
        };
        let raw = match raw {
            Value::Object(_) => raw,
            other => {
                anyhow::bail!("expected extended-config.json root to be an object, found {other:?}")
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            raw,
        })
    }

    /// Parse the raw object into the typed [`ExtendedConfig`]. Falls back
    /// to `Default` on malformed input (mirroring the tolerant loading
    /// done elsewhere in this module).
    pub fn config(&self) -> ExtendedConfig {
        serde_json::from_value(self.raw.clone()).unwrap_or_default()
    }

    /// Merge a typed [`ExtendedConfig`] back into the raw object and
    /// persist. Only fields we know how to serialize get overwritten;
    /// unknown keys at the root are preserved verbatim.
    pub fn write(&mut self, cfg: &ExtendedConfig) -> Result<()> {
        let obj = self
            .raw
            .as_object_mut()
            .expect("extended-config root is an object");
        let serialized = serde_json::to_value(cfg).context("serializing extended-config")?;
        if let Value::Object(map) = serialized {
            for (k, v) in map {
                obj.insert(k, v);
            }
        }
        // `utility_model` is `skip_serializing_if = none`, so clearing it
        // (Some → None) would otherwise leave the stale key on disk: the
        // merge above only overwrites keys present in the serialized map.
        // Mirror `ConfigDoc::write`'s explicit-remove pattern so the field
        // can be unset (the /settings picker's "clear" action).
        if cfg.utility_model.is_none() {
            obj.remove("utility_model");
        }
        let pretty =
            serde_json::to_string_pretty(&self.raw).context("serializing extended-config.json")?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, format!("{pretty}\n"))
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn vim_mode_round_trips_through_extended_doc() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.tui.vim_mode = VimModeSetting::Enabled;
        cfg.tui.thinking = ThinkingDisplay::Verbose;
        cfg.name = Some("Christopher".into());
        cfg.packages_directory = Some(PathBuf::from("/tmp/pkgs"));
        doc.write(&cfg).unwrap();

        let doc2 = ExtendedConfigDoc::load(&path).unwrap();
        let cfg2 = doc2.config();
        assert_eq!(cfg2.tui.vim_mode, VimModeSetting::Enabled);
        assert_eq!(cfg2.tui.thinking, ThinkingDisplay::Verbose);
        assert_eq!(cfg2.name.as_deref(), Some("Christopher"));
        assert_eq!(cfg2.packages_directory, Some(PathBuf::from("/tmp/pkgs")));
    }

    #[test]
    fn unknown_root_keys_survive_write() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, r#"{"future_feature":{"a":1}}"#).unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let cfg = doc.config();
        doc.write(&cfg).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("\"future_feature\""));
    }

    #[test]
    fn thinking_default_is_condensed() {
        assert_eq!(ThinkingDisplay::default(), ThinkingDisplay::Condensed);
    }

    #[test]
    fn new_top_level_keys_have_expected_defaults() {
        let cfg = ExtendedConfig::default();
        assert!(cfg.utility_model.is_none());
        assert!(!cfg.prompt_injection_guard.enabled);
        assert!(cfg.prompt_injection_guard.model.is_none());
        assert_eq!(cfg.system_prompt.time_injection_interval_minutes, 5);
        assert!(cfg.tui.banner.enabled);
    }

    #[test]
    fn new_keys_round_trip_through_extended_doc() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.utility_model = Some("anthropic:claude-haiku-4-5".into());
        cfg.prompt_injection_guard.enabled = true;
        cfg.prompt_injection_guard.model = Some("openai:gpt-4o-mini".into());
        cfg.system_prompt.time_injection_interval_minutes = 10;
        cfg.tui.banner.enabled = false;
        doc.write(&cfg).unwrap();

        let doc2 = ExtendedConfigDoc::load(&path).unwrap();
        let cfg2 = doc2.config();
        assert_eq!(
            cfg2.utility_model.as_deref(),
            Some("anthropic:claude-haiku-4-5")
        );
        assert!(cfg2.prompt_injection_guard.enabled);
        assert_eq!(
            cfg2.prompt_injection_guard.model.as_deref(),
            Some("openai:gpt-4o-mini")
        );
        assert_eq!(cfg2.system_prompt.time_injection_interval_minutes, 10);
        assert!(!cfg2.tui.banner.enabled);
    }

    #[test]
    fn clearing_utility_model_removes_the_key_from_disk() {
        // The /settings utility-model picker can clear the value back to
        // unset. Because `utility_model` is skip-if-none, the merge in
        // `write` won't overwrite a previously-stored value — the explicit
        // remove must drop it so the clear actually persists.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.utility_model = Some("anthropic:opus".into());
        doc.write(&cfg).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("utility_model")
        );

        // Reload, clear, write — the key must be gone on disk and on reload.
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.utility_model = None;
        doc.write(&cfg).unwrap();
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("utility_model"),
            "cleared utility_model must not linger on disk"
        );
        let cfg2 = ExtendedConfigDoc::load(&path).unwrap().config();
        assert_eq!(cfg2.utility_model, None);
    }

    #[test]
    fn loop_guard_threshold_defaults_to_two() {
        let cfg = ExtendedConfig::default();
        assert_eq!(cfg.loop_guard.repeat_threshold, 2);
        assert_eq!(cfg.loop_guard.effective_threshold(), 2);
    }

    #[test]
    fn loop_guard_threshold_clamps_below_two() {
        // A nonsensical threshold (< 2 would "fire on the first call
        // ever") is floored to 2 at read time.
        let cfg = LoopGuardConfig {
            repeat_threshold: 0,
        };
        assert_eq!(cfg.effective_threshold(), 2);
        let cfg = LoopGuardConfig {
            repeat_threshold: 1,
        };
        assert_eq!(cfg.effective_threshold(), 2);
        // A larger value is preserved.
        let cfg = LoopGuardConfig {
            repeat_threshold: 5,
        };
        assert_eq!(cfg.effective_threshold(), 5);
    }

    #[test]
    fn loop_guard_threshold_round_trips_through_extended_doc() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.loop_guard.repeat_threshold = 4;
        doc.write(&cfg).unwrap();
        let doc2 = ExtendedConfigDoc::load(&path).unwrap();
        assert_eq!(doc2.config().loop_guard.repeat_threshold, 4);
    }

    #[test]
    fn caffeinate_display_awake_defaults_off_and_maps_to_system_only_scope() {
        let cfg = ExtendedConfig::default();
        assert!(
            !cfg.tui.caffeinate_display_awake,
            "default must keep the display free to sleep"
        );
        assert_eq!(cfg.tui.sleep_scope(), SleepScope::SystemOnly);
    }

    #[test]
    fn caffeinate_display_awake_round_trips_and_maps_to_full_scope() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.tui.caffeinate_display_awake = true;
        doc.write(&cfg).unwrap();

        let doc2 = ExtendedConfigDoc::load(&path).unwrap();
        let cfg2 = doc2.config();
        assert!(cfg2.tui.caffeinate_display_awake);
        assert_eq!(cfg2.tui.sleep_scope(), SleepScope::SystemAndDisplay);
    }

    #[test]
    fn plan_branch_root_defaults_to_cockpit_plan() {
        let cfg = ExtendedConfig::default();
        assert_eq!(cfg.plan_branch_root, "cockpit-plan");
        // A config that omits the field still reads the default.
        let parsed: ExtendedConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.plan_branch_root, "cockpit-plan");
    }

    #[test]
    fn plan_branch_root_round_trips_through_extended_doc() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.plan_branch_root = "wip".to_string();
        doc.write(&cfg).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("\"planBranchRoot\""), "{on_disk}");
        let doc2 = ExtendedConfigDoc::load(&path).unwrap();
        assert_eq!(doc2.config().plan_branch_root, "wip");
    }

    #[test]
    fn default_isolation_mode_defaults_to_worktree() {
        // The resolved Q4c default: a config that omits the field reads
        // `worktree` (worktree + merge queue), not `shared_tree`.
        let cfg = ExtendedConfig::default();
        assert_eq!(cfg.default_isolation_mode, IsolationModeSetting::Worktree);
        let parsed: ExtendedConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(
            parsed.default_isolation_mode,
            IsolationModeSetting::Worktree
        );
        // Maps onto the DB-layer isolation mode.
        let db_mode: crate::db::plans::IsolationMode = parsed.default_isolation_mode.into();
        assert_eq!(db_mode, crate::db::plans::IsolationMode::Worktree);
    }

    #[test]
    fn default_isolation_mode_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.default_isolation_mode = IsolationModeSetting::SharedTree;
        doc.write(&cfg).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("\"defaultIsolationMode\""), "{on_disk}");
        assert!(on_disk.contains("shared_tree"), "{on_disk}");
        let doc2 = ExtendedConfigDoc::load(&path).unwrap();
        assert_eq!(
            doc2.config().default_isolation_mode,
            IsolationModeSetting::SharedTree
        );
    }

    #[test]
    fn llm_mode_defaults_to_defensive() {
        let cfg = ExtendedConfig::default();
        assert_eq!(cfg.llm_mode, LlmMode::Defensive);
        // A config that omits the field still reads the default.
        let parsed: ExtendedConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.llm_mode, LlmMode::Defensive);
    }

    #[test]
    fn llm_mode_parses_both_values() {
        let d: ExtendedConfig = serde_json::from_str(r#"{"llm_mode":"defensive"}"#).unwrap();
        assert_eq!(d.llm_mode, LlmMode::Defensive);
        let n: ExtendedConfig = serde_json::from_str(r#"{"llm_mode":"normal"}"#).unwrap();
        assert_eq!(n.llm_mode, LlmMode::Normal);
    }

    #[test]
    fn llm_mode_unknown_value_is_rejected_with_backtick_and_valid_set() {
        let err = serde_json::from_str::<ExtendedConfig>(r#"{"llm_mode":"yolo"}"#)
            .expect_err("unknown llm_mode must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("`yolo`"),
            "offending value must be backticked: {msg}"
        );
        assert!(msg.contains("defensive"), "valid set must be listed: {msg}");
        assert!(msg.contains("normal"), "valid set must be listed: {msg}");
    }

    #[test]
    fn llm_mode_toggled_flips() {
        assert_eq!(LlmMode::Defensive.toggled(), LlmMode::Normal);
        assert_eq!(LlmMode::Normal.toggled(), LlmMode::Defensive);
    }

    #[test]
    fn llm_mode_round_trips_through_extended_doc() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.llm_mode = LlmMode::Normal;
        doc.write(&cfg).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("\"llm_mode\""), "{on_disk}");
        let doc2 = ExtendedConfigDoc::load(&path).unwrap();
        assert_eq!(doc2.config().llm_mode, LlmMode::Normal);
    }

    #[test]
    fn skills_config_default_is_codex_mode_and_no_dirs() {
        let cfg = ExtendedConfig::default();
        assert!(
            cfg.skills.scan_dirs.is_empty(),
            "the struct default scans nothing; seeding is materialized only on a fresh install"
        );
        assert!(
            !cfg.skills.auto_bang_commands,
            "auto-`!` must default to disabled (Codex mode)"
        );
        assert!(
            !cfg.skills.ancestor_walk,
            "ancestor walk must default to off"
        );
    }

    #[test]
    fn skills_absent_scan_dirs_parses_empty_not_seeded() {
        // An existing config that omits `scan_dirs` parses to an empty
        // list (clean break — no implicit re-seed at parse time).
        let cfg: ExtendedConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.skills.scan_dirs.is_empty());
        assert!(!cfg.skills.ancestor_walk);
    }

    #[test]
    fn ancestor_walk_round_trips_through_extended_doc() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.skills.ancestor_walk = true;
        doc.write(&cfg).unwrap();
        let doc2 = ExtendedConfigDoc::load(&path).unwrap();
        assert!(doc2.config().skills.ancestor_walk);
    }

    #[test]
    fn skills_config_round_trips_through_extended_doc() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extended-config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut doc = ExtendedConfigDoc::load(&path).unwrap();
        let mut cfg = doc.config();
        cfg.skills.scan_dirs = vec!["~/.agents/skills".into(), "$PWD/.agents/skills".into()];
        cfg.skills.auto_bang_commands = true;
        doc.write(&cfg).unwrap();

        let doc2 = ExtendedConfigDoc::load(&path).unwrap();
        let cfg2 = doc2.config();
        assert_eq!(
            cfg2.skills.scan_dirs,
            vec![
                "~/.agents/skills".to_string(),
                "$PWD/.agents/skills".to_string()
            ]
        );
        assert!(cfg2.skills.auto_bang_commands);
    }
}
