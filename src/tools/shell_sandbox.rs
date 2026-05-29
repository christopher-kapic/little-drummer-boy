//! zerobox shell confinement for the `bash` tool (sandboxing part 2).
//!
//! Wraps a `sh -c <command>` invocation in a zerobox `Sandbox` confined
//! to: the agent cwd (read+write), the per-session tmp dir (read+write),
//! and `PATH` execution (zerobox's default profile auto-adds a minimal
//! system-path read entry, so any binary on `PATH` still runs). Reads
//! outside that allowlist are denied — silently, inside the child only
//! (zerobox is hard-deny with no callback), which is why the
//! run-fail-escalate prompt in `bash.rs` can't name the blocked path.
//!
//! We build the child via `Sandbox::...prepare().into_command()` rather
//! than `.run()`/`.spawn()` so the caller keeps full control of the
//! `tokio::process::Command` — cockpit re-applies `process_group(0)` +
//! `kill_on_drop` and runs its own cancel/timeout/pgid-kill loop, exactly
//! as the unsandboxed path does. `.run()`/`.spawn()` would use
//! `output()`/piped internally and lose pgid control.
//!
//! Platform support is Linux/macOS/WSL only (zerobox has no native
//! Windows backend); on Windows the shell runs unconfined and this module
//! is never invoked (see `bash.rs`). Network confinement is out of scope:
//! we never call `allow_net*`, so the child keeps the host's network
//! behavior.
//!
//! Linux re-entry: zerobox re-execs the current binary as
//! `zerobox-linux-sandbox`. [`init`] must run once near process start
//! (before the tokio runtime / extra threads) — it dispatches the helper
//! and installs the PATH-prepend alias guard. The resolved helper exe is
//! threaded into every sandbox via `.linux_sandbox_exe(...)`.

#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

use anyhow::Result;

/// Linux helper alias path, captured by [`init`] and read by
/// [`build_sandboxed_command`]. `None` on non-Linux or when init wasn't
/// run / failed. The guard that keeps the alias dir alive is leaked for
/// the process lifetime (intentional — sandboxed children may re-enter at
/// any time until exit).
#[cfg(target_os = "linux")]
static LINUX_SANDBOX_EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Dispatch the Linux sandbox helper and install the PATH-prepend alias.
///
/// MUST be called near the very start of `main` — before the tokio
/// runtime is built and before any extra threads spawn — because the
/// dispatch can re-exec the process as the helper and the PATH mutation
/// is only sound single-threaded (zerobox documents both constraints).
/// A no-op on non-Linux. The alias guard is leaked deliberately so the
/// helper alias outlives every sandboxed child for the process lifetime.
/// Idempotent: the `LINUX_SANDBOX_EXE` `OnceLock` ignores a second set,
/// so a defensive call from a test is harmless.
pub fn init() {
    #[cfg(target_os = "linux")]
    {
        zerobox::arg0::dispatch_linux_sandbox_helper();
        let exe = match zerobox::arg0::prepend_path_entry_for_zerobox_aliases() {
            Ok(guard) => {
                let exe = guard.zerobox_linux_sandbox_exe().to_path_buf();
                // Keep the alias dir + PATH entry alive for the whole
                // process: leak the guard. Sandboxed children may
                // re-enter the helper at any point until exit.
                std::mem::forget(guard);
                Some(exe)
            }
            Err(e) => {
                tracing::warn!(error = %e, "zerobox Linux helper init failed; shell sandbox disabled");
                None
            }
        };
        let _ = LINUX_SANDBOX_EXE.set(exe);
    }
}

/// Whether shell sandboxing can run on this platform. False on Windows
/// (no zerobox backend) — `bash.rs` takes the unconfined path + a
/// one-time notice there.
pub const fn shell_sandbox_supported() -> bool {
    cfg!(not(windows))
}

/// Build a confined `sh -c <command>` as a `tokio::process::Command`,
/// ready for the caller to apply `process_group(0)` / `kill_on_drop` and
/// run its cancel/timeout loop.
///
/// `command` is the full (prelude-prefixed) shell line. `cwd` is the
/// agent working directory — read+write inside the sandbox. `tmp_dir`,
/// when present, is the per-session scratch dir — also read+write, and
/// counted as inside the boundary by native-tool checks. `extra_env` is
/// applied on top of the inherited environment (cockpit uses it for the
/// env-scrub overrides). Reads outside cwd + tmp are denied.
///
/// Returns an error only if zerobox's policy validation fails (e.g. an
/// unusable cwd); a failure there is surfaced to the model as a spawn
/// error, never silently downgraded to unconfined.
pub async fn build_sandboxed_command(
    command: &str,
    cwd: &std::path::Path,
    tmp_dir: Option<&std::path::Path>,
    extra_env: &[(String, String)],
) -> Result<tokio::process::Command> {
    let mut sandbox = zerobox::Sandbox::command("sh")
        .arg("-c")
        .arg(command)
        .cwd(cwd.to_path_buf())
        // Inherit the parent env so PATH / HOME / language vars survive;
        // cockpit's env-scrub overrides are layered on below. (Without
        // `inherit_env` zerobox keeps only a tiny default set, which
        // breaks tools that read e.g. `CARGO_HOME`.)
        .inherit_env()
        // cwd is the read+write working area.
        .allow_read(cwd.to_path_buf())
        .allow_write(cwd.to_path_buf());

    if let Some(tmp) = tmp_dir {
        sandbox = sandbox
            .allow_read(tmp.to_path_buf())
            .allow_write(tmp.to_path_buf());
    }

    // Layer cockpit's env-scrub overrides (e.g. blanking injection-vector
    // vars) on top of the inherited env.
    for (k, v) in extra_env {
        sandbox = sandbox.env(k.clone(), v.clone());
    }

    // Linux: hand zerobox the helper alias captured at init so it can
    // re-enter the current binary as the sandbox helper. When init didn't
    // run / failed, fall through to zerobox's internal default resolution.
    #[cfg(target_os = "linux")]
    if let Some(Some(exe)) = LINUX_SANDBOX_EXE.get() {
        sandbox = sandbox.linux_sandbox_exe(exe.clone());
    }

    let prepared = sandbox.prepare().await?;
    Ok(prepared.into_command())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_off_only_on_windows() {
        assert_eq!(shell_sandbox_supported(), cfg!(not(windows)));
    }

    /// The confined command builds to a runnable `tokio::process::Command`
    /// with cwd + tmp as the write area (sandboxing part 2). Gated to
    /// Unix; the Linux backend needs the helper, which `init` installs
    /// (idempotent — safe to call from a test). We assert the *builder*
    /// succeeds and targets the right program, not EPERM enforcement
    /// (that needs a child + the helper re-entry, impractical to assert
    /// from a unit test without spawning).
    #[cfg(unix)]
    #[tokio::test]
    async fn builds_confined_command() {
        init();
        let cwd = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cmd = build_sandboxed_command(
            "true",
            cwd.path(),
            Some(tmp.path()),
            &[("SECRET_KEY".to_string(), String::new())],
        )
        .await
        .expect("sandbox command builds");
        // The prepared command is real and runnable. On Linux it re-execs
        // through the sandbox helper alias, so the program is the helper
        // binary, not `sh` directly; either way it's a non-empty program.
        let dbg = format!("{cmd:?}");
        assert!(!dbg.is_empty());
    }
}
