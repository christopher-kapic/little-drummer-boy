//! `bash` — execute a shell command.
//!
//! Auto-allow for v0 (GOALS bootstrap policy). The `exec_approval` flow
//! and Shift+Tab approval-mode cycling will land alongside the rest of
//! plan §3e.
//!
//! Per the tool-availability-policy memory: at startup we probe
//! `$PATH` for `rg`/`fd` and (on macOS) `gsed`. The tool description
//! advertises which of these are available so the model picks the
//! right binary, and on macOS-with-gsed we prepend a small `sed()`
//! shell function so `sed` invocations use the GNU implementation —
//! BSD `sed` differs enough that scripts written for Linux fail
//! silently on macOS.
//!
//! Safety:
//!   - Output is capped at [`crate::tools::common::OUTPUT_BYTE_CAP`].
//!   - The env scrub list from plan §3c removes the well-known
//!     injection-vector vars (`BASH_ENV`, `PROMPT_COMMAND`, …) and
//!     anything matching the `*_KEY` / `*_SECRET` / `*_TOKEN` patterns.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::engine::tool::{Tool, ToolCtx, ToolOutput};
use crate::tools::common::{OUTPUT_BYTE_CAP, truncate_head_tail};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

/// One-shot guard so the Windows "shell sandboxing unavailable" notice
/// prints at most once per process (≈ per session — the daemon runs one
/// process). Token economy §10: a single terse line, never repeated.
#[cfg(windows)]
static WINDOWS_NOTICE_SHOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Configured at construction time from a `$PATH` probe. `description`
/// is the cached string returned by [`Tool::description`]; `prelude`
/// is prepended to every shell command (currently used only for the
/// macOS `sed → gsed` alias).
pub struct BashTool {
    description: String,
    prelude: String,
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BashTool {
    pub fn new() -> Self {
        let has_rg = which::which("rg").is_ok();
        let has_fd = which::which("fd").is_ok();
        let has_gsed = which::which("gsed").is_ok();
        let alias_sed = cfg!(target_os = "macos") && has_gsed;

        // Build the description. GOALS §10 says: one sentence,
        // terse. We append a short suffix listing the search binaries
        // that are actually on PATH — saves the model a probe step.
        let mut hints: Vec<&str> = Vec::new();
        if has_rg {
            hints.push("rg");
        }
        if has_fd {
            hints.push("fd");
        }
        let search_hint = if hints.is_empty() {
            String::new()
        } else {
            format!("; prefer {} over grep/find for searches", hints.join("/"))
        };
        let sed_hint = if alias_sed {
            "; `sed` is wired to gsed (GNU)".to_string()
        } else {
            String::new()
        };
        let description = format!(
            "Execute a shell command; returns stdout/stderr/exit (8 KB cap, 120s default timeout){search_hint}{sed_hint}"
        );

        // Prepend a `sed` shell function on macOS so the model can use
        // its standard Linux-style flags without having to remember to
        // type `gsed` itself. `command gsed` bypasses the function on
        // recursion (no infinite-loop hazard).
        let prelude = if alias_sed {
            "sed() { command gsed \"$@\"; }; ".to_string()
        } else {
            String::new()
        };

        Self {
            description,
            prelude,
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command":    { "type": "string", "description": "Shell command" },
                "cwd":        { "type": "string", "description": "Working directory; defaults to session cwd" },
                "timeout_ms": { "type": "integer", "description": "Hard timeout in ms (max 600000)" }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| crate::engine::tool::invalid_input("`command` is required"))?;
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(|s| crate::tools::common::resolve(s, &ctx.cwd))
            .unwrap_or_else(|| ctx.cwd.clone());
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        tracing::debug!(command, timeout_ms, "bash: spawning");

        let prefixed = if self.prelude.is_empty() {
            command.to_string()
        } else {
            format!("{}{command}", self.prelude)
        };

        // Resolve whether to confine this run (sandboxing part 2):
        //
        //   - Windows: never (no zerobox backend) — run unconfined and
        //     show the one-time per-session notice.
        //   - Sandboxing disabled for this session (`/sandbox off` /
        //     `--no-sandbox`): run unconfined.
        //   - Otherwise consult part 1: if every constituent simple
        //     command is already granted broad access (Session/Project/
        //     Global), skip the box and run with broadened access.
        //   - Else run sandboxed (cwd + session tmp rw, PATH exec, deny
        //     outside).
        let sandbox_on =
            ctx.session.sandbox_enabled() && crate::tools::shell_sandbox::shell_sandbox_supported();

        // Windows has no zerobox backend: show the one-time per-session
        // notice that the shell runs unconfined. The flag is only ever
        // `Some` on Windows; elsewhere it stays `None`.
        let windows_notice: Option<&'static str> = windows_shell_notice(ctx);

        let granted_broad = if sandbox_on {
            command_granted_broad(ctx, command).await
        } else {
            false
        };
        let confine = sandbox_on && !granted_broad;

        let tmp_dir = ctx.session.tmp_dir();
        let scrub = scrub_overrides();

        // First attempt: sandboxed (confined) or broadened/unconfined.
        let attempt = run_shell(
            &prefixed,
            &cwd,
            confine,
            tmp_dir.as_deref(),
            &scrub,
            ctx,
            timeout_ms,
        )
        .await;
        let outcome = match attempt {
            RunOutcome::Cancelled => {
                return Ok(ToolOutput::truncated_text(
                    "Error: command cancelled by user (ctrl+c)".to_string(),
                ));
            }
            RunOutcome::TimedOut => {
                return Ok(ToolOutput::truncated_text(format!(
                    "Error: timeout after {timeout_ms} ms"
                )));
            }
            RunOutcome::SpawnError(e) => {
                return Ok(ToolOutput::text(format!("Error spawning shell: {e}")));
            }
            RunOutcome::WaitError(e) => {
                return Ok(ToolOutput::text(format!("Error running command: {e}")));
            }
            RunOutcome::Done(o) => o,
        };

        // Run-fail-escalate (sandboxing part 2): a non-zero exit from a
        // *confined* run may have been caused by a denied read outside
        // cwd — but zerobox is silent, so we can't be sure, nor name the
        // path. Offer an honest broadened re-run; on a non-deny choice
        // part 1 persists the grant, and we re-run with broadened access
        // (which repeats side-effects — noted in the prompt).
        let mut final_outcome = outcome;
        if confine
            && !final_outcome.success
            && let Some(approver) = ctx.approver.as_ref()
        {
            let decision = approver.approve_command(command).await?;
            // Deny → fall through with the original sandboxed result.
            if decision.is_allowed() {
                let rerun = run_shell(
                    &prefixed,
                    &cwd,
                    false, // broadened — no confinement
                    tmp_dir.as_deref(),
                    &scrub,
                    ctx,
                    timeout_ms,
                )
                .await;
                match rerun {
                    RunOutcome::Cancelled => {
                        return Ok(ToolOutput::truncated_text(
                            "Error: command cancelled by user (ctrl+c)".to_string(),
                        ));
                    }
                    RunOutcome::TimedOut => {
                        return Ok(ToolOutput::truncated_text(format!(
                            "Error: timeout after {timeout_ms} ms"
                        )));
                    }
                    RunOutcome::SpawnError(e) => {
                        return Ok(ToolOutput::text(format!("Error spawning shell: {e}")));
                    }
                    RunOutcome::WaitError(e) => {
                        return Ok(ToolOutput::text(format!("Error running command: {e}")));
                    }
                    RunOutcome::Done(o) => final_outcome = o,
                }
            }
        }

        let body = render_output(&final_outcome, windows_notice);
        if body.len() > OUTPUT_BYTE_CAP {
            // Head+tail so the `exit:` line and any stderr at the tail
            // survive — the failure signal usually lives there.
            Ok(ToolOutput::truncated_text(truncate_head_tail(
                &body,
                OUTPUT_BYTE_CAP,
            )))
        } else {
            Ok(ToolOutput::text(body))
        }
    }
}

/// Whether *every* simple command in `command` is already granted broad
/// (Session/Project/Global) access through part 1's store — in which
/// case the sandboxed run is skipped and the command runs broadened with
/// no prompt. A wrapper, an ungranted command, or no approver all return
/// `false` (run sandboxed). Pure store reads — never prompts here.
async fn command_granted_broad(ctx: &ToolCtx, command: &str) -> bool {
    let Some(approver) = ctx.approver.as_ref() else {
        return false;
    };
    let classification = crate::approval::classify::classify(command);
    let simple = classification.simple_commands();
    if simple.is_empty() || classification.has_wrapper() {
        // Empty / unparseable / no simple commands, or any wrapper → run
        // sandboxed (a wrapper is never persistable, so never "granted
        // broad").
        return false;
    }
    simple
        .iter()
        .all(|info| approver.store().is_command_granted(&info.key))
}

/// The combined outcome of one shell run.
struct ShellOutcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: i32,
    signaled: bool,
    success: bool,
}

/// Internal run result, distinguishing the abort paths from a completed
/// run so the caller can early-return the right marker.
enum RunOutcome {
    Done(ShellOutcome),
    Cancelled,
    TimedOut,
    SpawnError(std::io::Error),
    WaitError(std::io::Error),
}

/// Render the model-facing body from a finished run, prepending a
/// one-time platform notice when present.
fn render_output(o: &ShellOutcome, notice: Option<&str>) -> String {
    let stdout = String::from_utf8_lossy(&o.stdout);
    let stderr = String::from_utf8_lossy(&o.stderr);
    let body = format_combined(&stdout, &stderr, o.exit, o.signaled);
    match notice {
        Some(n) => format!("{n}\n{body}"),
        None => body,
    }
}

/// Spawn `sh -c <command>` — confined via zerobox when `confine`, else
/// plain — apply the process-group + kill-on-drop + cancel/timeout/
/// pgid-kill logic (identical for both paths), and return the outcome.
///
/// Building the confined child via `Sandbox::...prepare().into_command()`
/// (not `.run()`/`.spawn()`) is what lets us keep pgid control through
/// the sandbox: we own the `tokio::process::Command` and apply the same
/// `process_group(0)` + `kill_on_drop` + `tokio::select!`(wait vs cancel
/// vs timeout) + negative-pgid kill the unsandboxed path uses.
async fn run_shell(
    command: &str,
    cwd: &std::path::Path,
    confine: bool,
    tmp_dir: Option<&std::path::Path>,
    scrub: &[(String, String)],
    ctx: &ToolCtx,
    timeout_ms: u64,
) -> RunOutcome {
    let mut cmd = if confine {
        match crate::tools::shell_sandbox::build_sandboxed_command(command, cwd, tmp_dir, scrub)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                // A policy-validation failure (e.g. unusable cwd) is a
                // spawn error to the model — never a silent downgrade to
                // unconfined.
                return RunOutcome::SpawnError(std::io::Error::other(format!(
                    "sandbox setup failed: {e}"
                )));
            }
        }
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command).current_dir(cwd);
        for (k, _v) in scrub {
            c.env_remove(k);
        }
        c
    };

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // If this future is dropped (e.g. the worker task is torn down)
        // the immediate child dies too — a leaked subprocess would
        // outlive its turn. The process-group kill below handles the
        // descendant tree on an explicit ctrl+c cancel.
        .kill_on_drop(true);
    // Unix: put the child in its own process group so a cancel can kill
    // the whole tree (the `sh -c` plus anything it spawned — a test
    // runner, a `make`, …), not just the immediate shell. We signal the
    // negative pgid below. `tokio::process::Command::process_group` is
    // the inherent wrapper over the `CommandExt` setting. Windows has no
    // process groups; we fall back to `Child::kill` on cancel. This is
    // applied identically whether or not the command was confined —
    // zerobox handed us a plain `tokio::process::Command`.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return RunOutcome::SpawnError(e),
    };
    #[cfg(unix)]
    let child_pid = child.id();

    // Drain stdout/stderr on background tasks so `wait()` can't deadlock
    // on a full pipe buffer, while keeping `child` borrowable (rather
    // than consumed by `wait_with_output`) so the cancel branch can kill
    // it. The reader tasks end naturally when the pipes close.
    use tokio::io::AsyncReadExt;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });

    let timeout = std::time::Duration::from_millis(timeout_ms);
    // Race the command against (a) its timeout and (b) a turn-cancel
    // (user ctrl+c). On cancel we terminate the process group promptly
    // so a long-running test run dies instead of holding the turn open.
    let status = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => {
            kill_child(&mut child, #[cfg(unix)] child_pid).await;
            stdout_task.abort();
            stderr_task.abort();
            return RunOutcome::Cancelled;
        }
        res = tokio::time::timeout(timeout, child.wait()) => match res {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return RunOutcome::WaitError(e),
            Err(_) => {
                // Timed out: kill the group so a hung command can't
                // linger past its deadline, then report.
                kill_child(&mut child, #[cfg(unix)] child_pid).await;
                stdout_task.abort();
                stderr_task.abort();
                return RunOutcome::TimedOut;
            }
        },
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    let exit = status.code().unwrap_or(-1);
    let signaled = !status.success() && status.code().is_none();

    RunOutcome::Done(ShellOutcome {
        stdout,
        stderr,
        exit,
        signaled,
        success: status.success(),
    })
}

/// Terminate a cancelled `bash` child. On Unix the child was spawned in
/// its own process group (`process_group(0)`), so we signal the **negated
/// pgid** to take down the `sh -c` and every descendant it spawned (a test
/// runner, a `make`, …) — killing only the immediate child would orphan
/// the real work. We send `SIGTERM` for a graceful stop, give it a brief
/// grace window, then `SIGKILL` anything still alive. On Windows there is
/// no process group; `Child::kill` (the immediate child) is the fallback.
async fn kill_child(child: &mut tokio::process::Child, #[cfg(unix)] pid: Option<u32>) {
    #[cfg(unix)]
    {
        if let Some(pid) = pid {
            // SAFETY: `libc::kill` with a negative pid signals the process
            // group; passing a valid pgid (== the leader pid, since we set
            // `process_group(0)`) is sound. Failure (ESRCH — already gone)
            // is ignored.
            let pgid = pid as i32;
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
            // Brief grace period for a clean shutdown, then SIGKILL the
            // group. We don't block the turn long — 200ms is plenty for a
            // shell to forward the signal and reap.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        } else {
            // No pid (already reaped) — fall back to the immediate child.
            let _ = child.kill().await;
        }
        // Reap the leader so it doesn't linger as a zombie.
        let _ = child.wait().await;
    }
    #[cfg(not(unix))]
    {
        // Windows: no process groups. Kill the immediate child and reap.
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

fn format_combined(stdout: &str, stderr: &str, exit: i32, signaled: bool) -> String {
    let mut out = String::new();
    if !stdout.is_empty() {
        out.push_str("stdout:\n");
        out.push_str(stdout);
        if !stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    if !stderr.is_empty() {
        out.push_str("stderr:\n");
        out.push_str(stderr);
        if !stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    if signaled {
        out.push_str("exit: signaled\n");
    } else {
        out.push_str(&format!("exit: {exit}\n"));
    }
    out
}

/// The one-time per-process "shell sandboxing unavailable on Windows"
/// notice (sandboxing part 2). Returns `Some(...)` at most once, and only
/// when the session wanted sandboxing on. A no-op (`None`) on every other
/// platform.
#[cfg(windows)]
fn windows_shell_notice(ctx: &ToolCtx) -> Option<&'static str> {
    if ctx.session.sandbox_enabled()
        && !WINDOWS_NOTICE_SHOWN.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        Some("Note: shell sandboxing is unavailable on Windows; commands run unconfined.")
    } else {
        None
    }
}

#[cfg(not(windows))]
fn windows_shell_notice(_ctx: &ToolCtx) -> Option<&'static str> {
    None
}

/// The env-scrub list from plan §3c (injection-vector vars + the
/// `*_KEY` / `*_SECRET` / `*_TOKEN` patterns), as `(key, "")` pairs.
///
/// Returned as a list so both run paths apply it identically: the
/// unconfined path `env_remove`s each key, and the sandboxed path passes
/// the same keys to zerobox as empty-value `env` overrides (which clears
/// them in the confined child's environment, since zerobox builds the
/// child env from a filtered inherit + our overrides). The value is the
/// empty string for the override form; the key alone is what the
/// unconfined path removes.
fn scrub_overrides() -> Vec<(String, String)> {
    const FIXED_REMOVE: &[&str] = &[
        "BASH_ENV",
        "ENV",
        "PROMPT_COMMAND",
        "NODE_OPTIONS",
        "SHELLOPTS",
        "BASHOPTS",
        "GREP_OPTIONS",
        "GREP_COLORS",
    ];
    let mut out: Vec<(String, String)> = FIXED_REMOVE
        .iter()
        .map(|k| ((*k).to_string(), String::new()))
        .collect();
    // Pattern strip: anything ending in _KEY / _SECRET / _TOKEN, case
    // insensitive.
    for (k, _v) in std::env::vars() {
        let upper = k.to_uppercase();
        if upper.ends_with("_KEY") || upper.ends_with("_SECRET") || upper.ends_with("_TOKEN") {
            out.push((k, String::new()));
        }
    }
    out
}

/// Windows-only: the shell-sandbox notice fires at most once per process
/// and only when the session wanted sandboxing on (sandboxing part 2).
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_notice_fires_once_then_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::tools::common::test_ctx(tmp.path());
        // test_ctx defaults sandbox OFF → no notice.
        assert!(windows_shell_notice(&ctx).is_none());
        // With sandbox requested ON, the notice fires once, then the
        // one-shot guard silences it (process-global).
        ctx.session.set_sandbox_enabled(true);
        let first = windows_shell_notice(&ctx);
        let second = windows_shell_notice(&ctx);
        // Exactly one of the two is `Some` (whichever observed the guard
        // first); the other is `None`. (Other tests in this binary may
        // have tripped the guard already, so we assert "at most one.")
        assert!(first.is_none() || second.is_none());
        // And shell sandboxing is reported unsupported on Windows.
        assert!(!crate::tools::shell_sandbox::shell_sandbox_supported());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A turn-cancel (ctrl+c) terminates a long-running `bash` command
    /// promptly — the tool returns the cancelled marker in well under the
    /// command's natural runtime — and the killed command's *descendant*
    /// (spawned in the same process group) dies too, so a runaway test
    /// runner can't outlive its turn.
    #[tokio::test]
    async fn cancel_kills_process_group_promptly() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::tools::common::test_ctx(tmp.path());
        let tool = BashTool::new();

        // A descendant subshell touches a heartbeat file every 100ms. If the
        // process group is killed, the heartbeat stops; if only the immediate
        // `sh -c` died, the descendant would keep updating it.
        let heartbeat = tmp.path().join("heartbeat");
        let hb = heartbeat.to_string_lossy().to_string();
        let command = format!("( while true; do touch '{hb}'; sleep 0.1; done ) & sleep 30",);

        let cancel = ctx.cancel.clone();
        // Fire the cancel shortly after the command starts.
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.cancel();
        });

        let start = Instant::now();
        let out = tool
            .call(serde_json::json!({ "command": command }), &ctx)
            .await
            .expect("bash call returns");
        let elapsed = start.elapsed();
        canceller.await.unwrap();

        // Returned promptly (well under the 30s sleep) with the cancel marker.
        assert!(
            elapsed < Duration::from_secs(5),
            "cancel should return promptly, took {elapsed:?}"
        );
        assert!(
            out.content.contains("cancelled by user"),
            "expected cancel marker, got: {}",
            out.content
        );

        // Give the SIGTERM→SIGKILL window time to land, then confirm the
        // descendant heartbeat has stopped (process group was killed).
        tokio::time::sleep(Duration::from_millis(600)).await;
        let mtime_after_kill = std::fs::metadata(&heartbeat)
            .ok()
            .and_then(|m| m.modified().ok());
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mtime_later = std::fs::metadata(&heartbeat)
            .ok()
            .and_then(|m| m.modified().ok());
        assert_eq!(
            mtime_after_kill, mtime_later,
            "descendant heartbeat kept updating — process group was not killed"
        );
    }

    /// A normal (uncancelled) command still runs to completion and returns
    /// its output + exit line.
    #[tokio::test]
    async fn normal_command_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::tools::common::test_ctx(tmp.path());
        let tool = BashTool::new();
        let out = tool
            .call(serde_json::json!({ "command": "printf hello" }), &ctx)
            .await
            .expect("bash call returns");
        assert!(out.content.contains("hello"), "got: {}", out.content);
        assert!(out.content.contains("exit: 0"), "got: {}", out.content);
    }

    // ---- run-fail-escalate decision logic (sandboxing part 2) -------------

    use std::sync::Arc;

    use crate::approval::Approver;
    use crate::approval::classify::SimpleCommandInfo;
    use crate::approval::store::{GrantStore, Scope};

    /// Build a sandbox-enabled ctx with an approver + grant store.
    fn ctx_with_store(cwd: &std::path::Path) -> ToolCtx {
        let db = crate::db::Db::open_in_memory().unwrap();
        let session =
            crate::session::Session::create(db.clone(), cwd.to_path_buf(), "coder").unwrap();
        session.set_sandbox_enabled(true);
        let sid = session.id;
        let locks = Arc::new(crate::locks::LockManager::from_db(db.clone()).unwrap());
        let cfg = crate::config::extended::RedactConfig::default();
        let redact = Arc::new(crate::redact::RedactionTable::build(&cfg, cwd).unwrap());
        let hub = Arc::new(crate::engine::interrupt::InterruptHub::detached());
        let store = GrantStore::new(db.clone(), sid, cwd.to_path_buf());
        let approver = Arc::new(Approver::new(store, db, sid, "coder", hub.clone()));
        ToolCtx {
            agent_id: "coder".to_string(),
            locks,
            session: Arc::new(session),
            cwd: cwd.to_path_buf(),
            redact,
            interrupts: hub,
            cancel: tokio_util::sync::CancellationToken::new(),
            approver: Some(approver),
        }
    }

    #[tokio::test]
    async fn granted_broad_skips_the_box() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_store(tmp.path());
        let approver = ctx.approver.as_ref().unwrap();
        // Not yet granted → must run sandboxed.
        assert!(!command_granted_broad(&ctx, "cargo build --release").await);
        // Grant `cargo build` at Session scope.
        let info = SimpleCommandInfo {
            program: "cargo".into(),
            subcommand: Some("build".into()),
            key: crate::approval::classify::ApprovalKey {
                program: "cargo".into(),
                subcommand: Some("build".into()),
            },
            wrapper: false,
        };
        approver
            .store()
            .record_command(&info, Scope::Session)
            .unwrap();
        // Now the same command is granted broad → skip the box.
        assert!(command_granted_broad(&ctx, "cargo build --release").await);
        // A different subcommand is still ungranted → run sandboxed.
        assert!(!command_granted_broad(&ctx, "cargo test").await);
    }

    #[tokio::test]
    async fn wrapper_never_skips_the_box() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_store(tmp.path());
        // A wrapper can't be persisted, so it can never be "granted
        // broad" → always runs sandboxed (and re-prompts on failure).
        assert!(!command_granted_broad(&ctx, "bash -c 'echo hi'").await);
        assert!(!command_granted_broad(&ctx, "sudo rm x").await);
    }

    #[tokio::test]
    async fn no_approver_never_skips_the_box() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::tools::common::test_ctx(tmp.path());
        // No approver → can't know any grant → run sandboxed.
        assert!(!command_granted_broad(&ctx, "ls").await);
    }

    // NOTE: an end-to-end "runs confined and EPERMs an outside read" test
    // is deliberately omitted. On Linux the zerobox path re-execs THIS
    // test binary as the `zerobox-linux-sandbox` helper, which only works
    // from a binary whose `main` ran `arg0::dispatch_linux_sandbox_helper`
    // first — the test harness's `main` does not, so a confined spawn
    // hangs/errors on helper re-entry. Per the build spec we therefore
    // cover the Sandbox CONFIGURATION/command-building (see
    // `shell_sandbox::tests::builds_confined_command`) and the
    // run-fail-escalate DECISION logic (above) instead of live EPERM
    // enforcement. The unconfined cancel/timeout/pgid path stays fully
    // exercised by `cancel_kills_process_group_promptly` /
    // `normal_command_completes` (test_ctx defaults sandbox OFF).
}
