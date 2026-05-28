//! Background shell jobs (GOALS §22).
//!
//! A background job spawns a shell subprocess that runs to completion
//! without blocking the human. Its stdout+stderr stream line-by-line into
//! a bounded ring buffer so `background.tail` can show recent progress;
//! on exit, a budget-capped result injects into main context at the next
//! turn boundary via [`JobEvent::Completed`].
//!
//! Output crossing to the model is budget-capped via
//! [`crate::intel::budget::BudgetedWriter`] (§10) — a `cargo build` can
//! dump megabytes; the model only ever sees the §22 token cap.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::engine::agent::TurnEvent;
use crate::engine::jobs::authority::JobEvent;
use crate::engine::jobs::spec::JobKind;
use crate::intel::budget::BudgetedWriter;
use crate::redact::RedactionTable;

use super::{ASYNC_RESULT_TOKEN_CAP, BACKGROUND_RING_LINES, TAIL_TOKEN_CAP};

/// Handle the authority keeps for a live background job. Lets it read the
/// tail ring and kill the process. `kill` is best-effort and idempotent.
pub struct BackgroundHandle {
    label: String,
    ring: Arc<Mutex<VecDeque<String>>>,
    /// Set when the job is asked to die; the spawned task observes it.
    kill_tx: tokio::sync::watch::Sender<bool>,
}

impl BackgroundHandle {
    /// Budget-capped tail of the last `lines` output lines, scrubbed for
    /// secrets. Returns an empty string when no output has been produced.
    pub fn tail(&self, lines: usize, redact: &RedactionTable) -> String {
        let snapshot: Vec<String> = {
            let ring = self.ring.lock().unwrap();
            let n = lines.min(ring.len());
            ring.iter().skip(ring.len() - n).cloned().collect()
        };
        let mut writer = BudgetedWriter::new(TAIL_TOKEN_CAP);
        // Tail: keep the most recent lines, so write from the end forward
        // and reverse — but BudgetedWriter is forward-only, so we just
        // write oldest→newest of the requested window and accept that an
        // over-cap window drops its *oldest* lines (the head of the
        // window), keeping the freshest output.
        let start = snapshot
            .len()
            .saturating_sub(window_that_fits(&snapshot, TAIL_TOKEN_CAP));
        for line in &snapshot[start..] {
            if !writer.writeln(line) {
                break;
            }
        }
        let body = writer.into_string();
        let scrubbed = redact.scrub(&body);
        if scrubbed.is_empty() {
            format!("`{}` has produced no output yet", self.label)
        } else {
            scrubbed
        }
    }

    /// Signal the spawned task to kill the child. Idempotent.
    pub fn kill(&self) {
        let _ = self.kill_tx.send(true);
    }
}

/// Compute how many trailing lines of `lines` fit under `cap` tokens, so
/// `tail` keeps the freshest output rather than the oldest.
fn window_that_fits(lines: &[String], cap: usize) -> usize {
    let mut probe = BudgetedWriter::new(cap);
    let mut count = 0;
    for line in lines.iter().rev() {
        if probe.writeln(line) {
            count += 1;
        } else {
            break;
        }
    }
    count
}

/// Spawn a background shell job. Returns the handle the authority keeps +
/// the task's [`tokio::task::JoinHandle`] (the authority takes its
/// `abort_handle` for cancellation).
pub fn spawn(
    job_id: String,
    label: String,
    command: String,
    cwd: std::path::PathBuf,
    redact: Arc<RedactionTable>,
    turn_tx: mpsc::Sender<TurnEvent>,
    event_tx: mpsc::Sender<JobEvent>,
) -> (BackgroundHandle, tokio::task::JoinHandle<()>) {
    let ring: Arc<Mutex<VecDeque<String>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(BACKGROUND_RING_LINES)));
    let (kill_tx, kill_rx) = tokio::sync::watch::channel(false);

    let handle = BackgroundHandle {
        label: label.clone(),
        ring: ring.clone(),
        kill_tx,
    };

    let task = tokio::spawn(run_background(
        job_id, label, command, cwd, ring, redact, turn_tx, event_tx, kill_rx,
    ));
    (handle, task)
}

#[allow(clippy::too_many_arguments)]
async fn run_background(
    job_id: String,
    label: String,
    command: String,
    cwd: std::path::PathBuf,
    ring: Arc<Mutex<VecDeque<String>>>,
    redact: Arc<RedactionTable>,
    turn_tx: mpsc::Sender<TurnEvent>,
    event_tx: mpsc::Sender<JobEvent>,
    mut kill_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&command)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If the authority aborts this task, kill the child too — a leaked
        // subprocess would outlive its job (anti-runaway).
        .kill_on_drop(true);
    scrub_env(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = event_tx
                .send(JobEvent::Completed {
                    job_id,
                    label,
                    kind: JobKind::Background,
                    result: format!("failed to spawn: {e}"),
                    failed: true,
                    requests: Vec::new(),
                })
                .await;
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut out_lines = stdout
        .map(|s| BufReader::new(s).lines())
        .expect("stdout piped");
    let mut err_lines = stderr
        .map(|s| BufReader::new(s).lines())
        .expect("stderr piped");

    let push = |ring: &Arc<Mutex<VecDeque<String>>>, line: String| {
        let mut r = ring.lock().unwrap();
        if r.len() == BACKGROUND_RING_LINES {
            r.pop_front();
        }
        r.push_back(line);
    };

    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut killed = false;

    loop {
        tokio::select! {
            // Kill request from the authority / `background.cancel`.
            changed = kill_rx.changed() => {
                if changed.is_ok() && *kill_rx.borrow() {
                    killed = true;
                    let _ = child.start_kill();
                    break;
                }
            }
            line = out_lines.next_line(), if !stdout_done => {
                match line {
                    Ok(Some(l)) => {
                        push(&ring, redact.scrub(&l));
                        let _ = turn_tx.try_send(TurnEvent::JobProgress {
                            job_id: job_id.clone(),
                        });
                    }
                    _ => stdout_done = true,
                }
            }
            line = err_lines.next_line(), if !stderr_done => {
                match line {
                    Ok(Some(l)) => {
                        push(&ring, redact.scrub(&l));
                        let _ = turn_tx.try_send(TurnEvent::JobProgress {
                            job_id: job_id.clone(),
                        });
                    }
                    _ => stderr_done = true,
                }
            }
            else => break,
        }
        if stdout_done && stderr_done {
            break;
        }
    }

    let status = child.wait().await;
    let exit = status.as_ref().ok().and_then(|s| s.code());
    let success = status.as_ref().map(|s| s.success()).unwrap_or(false);

    // Build the budget-capped result from the ring's freshest output.
    let snapshot: Vec<String> = {
        let r = ring.lock().unwrap();
        r.iter().cloned().collect()
    };
    let mut writer = BudgetedWriter::new(ASYNC_RESULT_TOKEN_CAP);
    let fit = window_that_fits(&snapshot, ASYNC_RESULT_TOKEN_CAP);
    let start = snapshot.len().saturating_sub(fit);
    if fit < snapshot.len() {
        let _ = writer.writeln(&format!(
            "[earlier output elided — {} of {} line(s) shown]",
            fit,
            snapshot.len()
        ));
    }
    for line in &snapshot[start..] {
        if !writer.writeln(line) {
            break;
        }
    }
    let body = writer.into_string();

    let (result, failed) = if killed {
        (format!("background `{label}` was cancelled"), false)
    } else {
        let header = match exit {
            Some(0) => format!("background `{label}` finished (exit 0)\n"),
            Some(code) => format!("background `{label}` finished (exit {code})\n"),
            None => format!("background `{label}` terminated by signal\n"),
        };
        (format!("{header}{body}"), !success)
    };

    let _ = event_tx
        .send(JobEvent::Completed {
            job_id,
            label,
            kind: JobKind::Background,
            result,
            failed,
            requests: Vec::new(),
        })
        .await;
}

/// Same env-injection scrub as the `bash` tool: strip injection-vector
/// vars + `*_KEY`/`*_SECRET`/`*_TOKEN`.
fn scrub_env(cmd: &mut Command) {
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
    for var in FIXED_REMOVE {
        cmd.env_remove(var);
    }
    for (k, _v) in std::env::vars() {
        let upper = k.to_uppercase();
        if upper.ends_with("_KEY") || upper.ends_with("_SECRET") || upper.ends_with("_TOKEN") {
            cmd.env_remove(&k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_that_fits_keeps_freshest() {
        let lines: Vec<String> = (0..50).map(|i| format!("line number {i}")).collect();
        // A tiny cap fits only a couple of trailing lines.
        let fit = window_that_fits(&lines, 6);
        assert!(fit >= 1 && fit < lines.len());
    }

    /// A background job that emits progress then sleeps: `tail` shows the
    /// emitted lines while it's still running, and `cancel` (via the kill
    /// handle) kills it and yields a cancelled completion.
    #[tokio::test]
    async fn tail_shows_progress_then_cancel_kills() {
        let cfg = crate::config::extended::RedactConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let redact = Arc::new(RedactionTable::build(&cfg, tmp.path()).unwrap());
        let (turn_tx, _turn_rx) = mpsc::channel(64);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let (handle, _task) = spawn(
            "job-1".into(),
            "slow".into(),
            // Emit two lines, then sleep long enough that we can tail + kill.
            "printf 'progress one\\nprogress two\\n'; sleep 30".into(),
            tmp.path().to_path_buf(),
            redact.clone(),
            turn_tx,
            event_tx,
        );

        // Wait until both lines land in the ring (poll the tail).
        let mut waited = 0;
        loop {
            let t = handle.tail(40, &redact);
            if t.contains("progress two") {
                assert!(t.contains("progress one"));
                break;
            }
            assert!(waited < 100, "lines never appeared in tail: {t}");
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += 1;
        }

        // Cancel kills the still-sleeping child.
        handle.kill();
        let completed = tokio::time::timeout(Duration::from_secs(10), event_rx.recv())
            .await
            .expect("cancel should complete the job")
            .unwrap();
        match completed {
            JobEvent::Completed { result, failed, .. } => {
                assert!(!failed, "a cancelled job isn't a failure");
                assert!(result.contains("cancelled"), "got {result}");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    use std::time::Duration;
}
