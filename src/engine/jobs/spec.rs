//! `jobs` meta-tool action schemas + per-action arg parsing.
//!
//! The meta-tool's *outer* schema is fixed and minimal (`action` +
//! `args`) so the tools array stays byte-stable across a conversation
//! (no prompt-cache bust on capability growth). Per-action `args` are
//! validated here, leaning on the §12 repair layer for the loose outer
//! shape: the dispatcher repairs the outer object, then this module does
//! the real per-action validation.

use serde_json::Value;

use crate::engine::tool::invalid_input;

/// The enabled-mid-conversation branches of the `jobs` meta-tool. Parsed
/// from the `action` string; unknown actions are a model-fault invalid
/// input (priority #1 — fail loud, not silent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobAction {
    LoopStart,
    LoopCancel,
    BackgroundStart,
    BackgroundTail,
    BackgroundCancel,
    /// List active jobs (always available in main).
    List,
}

impl JobAction {
    pub fn as_str(self) -> &'static str {
        match self {
            JobAction::LoopStart => "loop.start",
            JobAction::LoopCancel => "loop.cancel",
            JobAction::BackgroundStart => "background.start",
            JobAction::BackgroundTail => "background.tail",
            JobAction::BackgroundCancel => "background.cancel",
            JobAction::List => "list",
        }
    }
}

/// Parse an `action` string into a [`JobAction`]. Returns an
/// invalid-input error (model fault) for an unknown action.
pub fn parse_action(action: &str) -> anyhow::Result<JobAction> {
    match action {
        "loop.start" => Ok(JobAction::LoopStart),
        "loop.cancel" => Ok(JobAction::LoopCancel),
        "background.start" => Ok(JobAction::BackgroundStart),
        "background.tail" => Ok(JobAction::BackgroundTail),
        "background.cancel" => Ok(JobAction::BackgroundCancel),
        "list" => Ok(JobAction::List),
        other => Err(invalid_input(format!(
            "unknown jobs action `{other}` (expected loop.start, loop.cancel, background.start, background.tail, background.cancel, or list)"
        ))),
    }
}

/// What a running job is. `Timer` is a `Loop` with `limit == 1`; the UI
/// renders it distinctly but the scheduler treats both the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Loop,
    Timer,
    Background,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Loop => "loop",
            JobKind::Timer => "timer",
            JobKind::Background => "background",
        }
    }
}

/// Validated `loop.start` arguments. `timer` is this with `limit = 1`.
#[derive(Debug, Clone)]
pub struct LoopStartArgs {
    /// Seconds between iterations.
    pub interval_secs: u64,
    /// The self-prompt delivered each iteration.
    pub prompt: String,
    /// Exponential backoff (double the delay each iteration up to a
    /// ceiling). Default false.
    pub backoff: bool,
    /// Max iterations. `None` = unlimited. Default 10. `Some(1)` = timer.
    pub limit: Option<u64>,
    /// Each iteration accumulates in the main context (default true) vs.
    /// an ephemeral fork (false).
    pub keep_in_context: bool,
    /// Only meaningful when `keep_in_context == false`: fresh fork per
    /// iteration (true) vs. accumulate-in-fork (false, default).
    pub independent: bool,
}

impl LoopStartArgs {
    /// `true` when this loop is a one-shot timer (`limit == 1`).
    pub fn is_timer(&self) -> bool {
        self.limit == Some(1)
    }

    pub fn kind(&self) -> JobKind {
        if self.is_timer() {
            JobKind::Timer
        } else {
            JobKind::Loop
        }
    }
}

/// Ceiling on the backoff delay so an exponential loop can't drift to
/// effectively-never. Five minutes is plenty for a poll loop.
pub const BACKOFF_CEILING_SECS: u64 = 300;

/// Minimum loop interval. A weak model emitting `interval: 0` would
/// otherwise busy-loop the provider; clamp to a sane floor.
pub const MIN_INTERVAL_SECS: u64 = 1;

/// Default loop iteration cap (GOALS §22).
pub const DEFAULT_LOOP_LIMIT: u64 = 10;

/// Parse + validate `loop.start` args. Accepts `interval` as either a
/// number of seconds or a string like `"30s"` / `"2m"` / `"1h"` —
/// defensive against weak models (priority #1). `limit: 0` means
/// unlimited.
pub fn parse_loop_start(args: &Value) -> anyhow::Result<LoopStartArgs> {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid_input("`prompt` is required and must be a non-empty string"))?
        .to_string();

    let interval_secs = match args.get("interval") {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Some(Value::String(s)) => parse_duration_secs(s),
        _ => None,
    }
    .ok_or_else(|| {
        invalid_input("`interval` is required (seconds, or a string like \"30s\"/\"2m\"/\"1h\")")
    })?
    .max(MIN_INTERVAL_SECS);

    let backoff = args
        .get("backoff")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // limit: absent → default 10; 0 → unlimited; >0 → that cap.
    let limit = match args.get("limit") {
        None | Some(Value::Null) => Some(DEFAULT_LOOP_LIMIT),
        Some(v) => match v.as_u64() {
            Some(0) => None,
            Some(n) => Some(n),
            None => {
                return Err(invalid_input("`limit` must be a non-negative integer"));
            }
        },
    };

    let keep_in_context = args
        .get("keep_in_context")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let independent = args
        .get("independent")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(LoopStartArgs {
        interval_secs,
        prompt,
        backoff,
        limit,
        keep_in_context,
        independent,
    })
}

/// `loop.cancel` args — a job id.
#[derive(Debug, Clone)]
pub struct LoopCancelArgs {
    pub job_id: String,
}

pub fn parse_loop_cancel(args: &Value) -> anyhow::Result<LoopCancelArgs> {
    let job_id = args
        .get("job_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid_input("`job_id` is required"))?
        .to_string();
    Ok(LoopCancelArgs { job_id })
}

/// `background.start` args — a shell command.
#[derive(Debug, Clone)]
pub struct BackgroundStartArgs {
    pub command: String,
    /// Optional working directory; defaults to the session cwd.
    pub cwd: Option<String>,
}

pub fn parse_background_start(args: &Value) -> anyhow::Result<BackgroundStartArgs> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid_input("`command` is required"))?
        .to_string();
    let cwd = args.get("cwd").and_then(Value::as_str).map(str::to_string);
    Ok(BackgroundStartArgs { command, cwd })
}

/// `background.tail` args — a job id and an optional line count.
#[derive(Debug, Clone)]
pub struct BackgroundTailArgs {
    pub job_id: String,
    pub lines: usize,
}

/// Default number of trailing lines `background.tail` returns.
pub const DEFAULT_TAIL_LINES: usize = 40;

pub fn parse_background_tail(args: &Value) -> anyhow::Result<BackgroundTailArgs> {
    let job_id = args
        .get("job_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid_input("`job_id` is required"))?
        .to_string();
    let lines = args
        .get("lines")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_TAIL_LINES)
        .clamp(1, super::BACKGROUND_RING_LINES);
    Ok(BackgroundTailArgs { job_id, lines })
}

/// `background.cancel` args — a job id.
#[derive(Debug, Clone)]
pub struct BackgroundCancelArgs {
    pub job_id: String,
}

pub fn parse_background_cancel(args: &Value) -> anyhow::Result<BackgroundCancelArgs> {
    let job_id = args
        .get("job_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid_input("`job_id` is required"))?
        .to_string();
    Ok(BackgroundCancelArgs { job_id })
}

/// A create-action a fork emitted that main must decide whether to honour
/// (anti-runaway: forks request, they do not spawn). Rides back to main
/// bundled with the fork's terminal return.
#[derive(Debug, Clone)]
pub enum SpawnRequest {
    Loop(LoopStartArgs),
    Background(BackgroundStartArgs),
}

impl SpawnRequest {
    /// One-line human description for the request chip surfaced to main.
    pub fn summary(&self) -> String {
        match self {
            SpawnRequest::Loop(a) => {
                let kind = if a.is_timer() { "timer" } else { "loop" };
                format!(
                    "{kind}(interval={}s, prompt={:?})",
                    a.interval_secs,
                    snippet(&a.prompt)
                )
            }
            SpawnRequest::Background(a) => {
                format!("background({:?})", snippet(&a.command))
            }
        }
    }
}

fn snippet(s: &str) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.chars().count() > 60 {
        let t: String = first.chars().take(60).collect();
        format!("{t}…")
    } else {
        first.to_string()
    }
}

/// Parse a duration string like `"30s"`, `"2m"`, `"1h"`, or a bare
/// number (seconds). Returns `None` on garbage.
pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num.trim().parse().ok()?;
    match unit {
        "s" | "S" => Some(n),
        "m" | "M" => Some(n.saturating_mul(60)),
        "h" | "H" => Some(n.saturating_mul(3600)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_action_known_and_unknown() {
        assert_eq!(parse_action("loop.start").unwrap(), JobAction::LoopStart);
        assert_eq!(parse_action("list").unwrap(), JobAction::List);
        assert!(parse_action("loop.frobnicate").is_err());
    }

    #[test]
    fn loop_start_defaults() {
        let a = parse_loop_start(&json!({ "interval": 30, "prompt": "check it" })).unwrap();
        assert_eq!(a.interval_secs, 30);
        assert_eq!(a.limit, Some(DEFAULT_LOOP_LIMIT));
        assert!(a.keep_in_context);
        assert!(!a.independent);
        assert!(!a.backoff);
        assert!(!a.is_timer());
        assert_eq!(a.kind(), JobKind::Loop);
    }

    #[test]
    fn loop_start_limit_one_is_timer() {
        let a =
            parse_loop_start(&json!({ "interval": "5m", "prompt": "fire", "limit": 1 })).unwrap();
        assert!(a.is_timer());
        assert_eq!(a.kind(), JobKind::Timer);
        assert_eq!(a.interval_secs, 300);
    }

    #[test]
    fn loop_start_limit_zero_is_unlimited() {
        let a = parse_loop_start(&json!({ "interval": 10, "prompt": "p", "limit": 0 })).unwrap();
        assert_eq!(a.limit, None);
    }

    #[test]
    fn loop_start_missing_prompt_errors() {
        assert!(parse_loop_start(&json!({ "interval": 10 })).is_err());
        assert!(parse_loop_start(&json!({ "interval": 10, "prompt": "  " })).is_err());
    }

    #[test]
    fn loop_start_missing_interval_errors() {
        assert!(parse_loop_start(&json!({ "prompt": "p" })).is_err());
    }

    #[test]
    fn interval_floor_clamps_zero() {
        let a = parse_loop_start(&json!({ "interval": 0, "prompt": "p" })).unwrap();
        assert_eq!(a.interval_secs, MIN_INTERVAL_SECS);
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration_secs("30"), Some(30));
        assert_eq!(parse_duration_secs("30s"), Some(30));
        assert_eq!(parse_duration_secs("2m"), Some(120));
        assert_eq!(parse_duration_secs("1h"), Some(3600));
        assert_eq!(parse_duration_secs("nonsense"), None);
        assert_eq!(parse_duration_secs(""), None);
    }

    #[test]
    fn background_start_requires_command() {
        assert!(parse_background_start(&json!({})).is_err());
        let a = parse_background_start(&json!({ "command": "cargo test" })).unwrap();
        assert_eq!(a.command, "cargo test");
        assert!(a.cwd.is_none());
    }

    #[test]
    fn background_tail_clamps_lines() {
        let a = parse_background_tail(&json!({ "job_id": "x", "lines": 99999 })).unwrap();
        assert_eq!(a.lines, super::super::BACKGROUND_RING_LINES);
        let b = parse_background_tail(&json!({ "job_id": "x" })).unwrap();
        assert_eq!(b.lines, DEFAULT_TAIL_LINES);
    }
}
