//! `cockpit daemon` subcommands.

use anyhow::Result;

use crate::cli::DaemonCommand;
use crate::daemon::{self, DaemonPaths, DaemonStatus};

pub async fn run(cmd: DaemonCommand) -> Result<()> {
    let paths = DaemonPaths::resolve()?;
    match cmd {
        DaemonCommand::Start { foreground, detach } => {
            if detach && !foreground {
                let pid = daemon::spawn_detached()?;
                println!(
                    "daemon: spawned (pid {pid})\n  socket: {}",
                    paths.socket.display()
                );
                return Ok(());
            }
            // Foreground mode: blocks until SIGINT/SIGTERM.
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
