use anyhow::{Result, bail};

use crate::auth::codex;
use crate::cli::ProvidersCommand;

pub async fn run(cmd: ProvidersCommand) -> Result<()> {
    match cmd {
        ProvidersCommand::List => {
            // Static list for now — the only provider with an
            // interactive auth flow is codex. Everything else is
            // configured via /settings → Providers + `$VAR` refs in
            // header values.
            println!("Providers with interactive login:");
            print_codex_status();
            println!();
            println!("API-key providers (configure via the TUI's /settings):");
            for t in crate::providers::TEMPLATES {
                if matches!(t.auth, crate::config::providers::AuthKind::ApiKey) {
                    println!("  {} — {}", t.id, t.display);
                }
            }
            Ok(())
        }
        ProvidersCommand::Login { provider } => {
            let id = provider.unwrap_or_default();
            match id.as_str() {
                "" | "codex" => run_codex_login().await,
                other => bail!(
                    "unsupported provider for login: `{other}`. \
                     Today only `codex` has an interactive flow; \
                     static API keys are configured via /settings."
                ),
            }
        }
        ProvidersCommand::Logout { provider } => {
            let id = provider.unwrap_or_default();
            match id.as_str() {
                "" | "codex" => match codex::logout()? {
                    true => {
                        println!("codex: logged out");
                        Ok(())
                    }
                    false => {
                        println!("codex: nothing stored");
                        Ok(())
                    }
                },
                other => bail!("unsupported provider for logout: `{other}`"),
            }
        }
    }
}

async fn run_codex_login() -> Result<()> {
    let cfg = codex::LoginConfig::default();
    let tokens = codex::run_interactive_login(&cfg).await?;
    println!();
    println!("codex: logged in");
    println!("  saved tokens to ~/.local/state/cockpit/credentials.json (key: codex)");
    println!("  saved_at = {}", tokens.saved_at);
    Ok(())
}

fn print_codex_status() {
    match codex::load() {
        Ok(Some(t)) => println!(
            "  codex — Codex (ChatGPT Plus/Pro)   [logged in since {}]",
            t.saved_at
        ),
        Ok(None) => {
            println!("  codex — Codex (ChatGPT Plus/Pro)   [run `cockpit providers login codex`]")
        }
        Err(e) => println!("  codex — Codex (ChatGPT Plus/Pro)   [credential read failed: {e}]"),
    }
}
