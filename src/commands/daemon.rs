//! `cockpit daemon` subcommands.

use anyhow::Result;

use crate::cli::DaemonCommand;
use crate::daemon::{self, DaemonPaths, DaemonStatus};

pub async fn run(cmd: DaemonCommand) -> Result<()> {
    let paths = DaemonPaths::resolve()?;
    match cmd {
        DaemonCommand::Start {
            foreground,
            detach,
            no_sandbox,
        } => {
            if detach && !foreground {
                let pid = daemon::spawn_detached(no_sandbox)?;
                println!(
                    "daemon: spawned (pid {pid})\n  socket: {}",
                    paths.socket.display()
                );
                return Ok(());
            }
            // Foreground mode: blocks until SIGINT/SIGTERM. A daemon
            // launched `--no-sandbox` disables filesystem sandboxing for
            // ALL its sessions (sandboxing part 2): export the marker env
            // var the session workers read at spawn (Layer B style).
            if no_sandbox {
                // SAFETY: set before the runtime spins up worker tasks; a
                // process-global read-only marker thereafter.
                unsafe {
                    std::env::set_var(crate::daemon::session_worker::DAEMON_NO_SANDBOX_ENV, "1");
                }
            }
            println!(
                "daemon: starting in foreground (pid {})\n  socket: {}\n  pid file: {}",
                std::process::id(),
                paths.socket.display(),
                paths.pid_file.display()
            );
            daemon::run_foreground(paths).await
        }
        DaemonCommand::Stop => {
            let stopped = daemon::stop(&paths)?;
            if stopped {
                println!("daemon: stopped");
            } else {
                println!("daemon: not running (no pid file)");
            }
            Ok(())
        }
        DaemonCommand::Status => {
            match daemon::probe(&paths).await {
                DaemonStatus::Running => {
                    println!("daemon: running\n  socket: {}", paths.socket.display());
                }
                DaemonStatus::Stale => {
                    println!(
                        "daemon: not responding (stale pid file or socket)\n  pid: {}\n  socket: {}",
                        paths.pid_file.display(),
                        paths.socket.display()
                    );
                }
                DaemonStatus::NotRunning => {
                    println!("daemon: not running");
                }
            }
            Ok(())
        }
    }
}
