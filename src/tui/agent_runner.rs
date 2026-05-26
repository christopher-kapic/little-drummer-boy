//! Wires the [`crate::engine::Driver`] into a background tokio task and
//! surfaces its events to the TUI via the same `Arc<Mutex<Vec<...>>>`
//! pattern `app.rs` uses for `/fetch-models`.
//!
//! Why not stream directly into `App.history`: the app's event loop
//! holds `&mut self` during draws, so the only safe place to push from
//! a tokio task is a `Mutex` the loop drains per tick. One drain pass
//! per `EVENT_TICK` is plenty for a chat surface.

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::config::dirs::discover_config_dirs;
use crate::config::providers::{ConfigDoc, ProvidersConfig};
use crate::engine::Driver;
use crate::engine::TurnEvent;
use crate::engine::builtin::{self, SpawnArgs};
use crate::engine::model::{Model, ModelParams};
use crate::locks::LockManager;
use crate::session::Session;

/// Handle the TUI keeps to talk to the engine.
pub struct AgentRunner {
    /// Send user-typed messages here.
    pub input_tx: mpsc::Sender<String>,
    /// Drained per tick into [`crate::tui::app::App::history`].
    pub events: Arc<Mutex<Vec<TurnEvent>>>,
    /// Mirrors the name of whoever's currently on top of the driver's
    /// agent stack. The chrome reads this to update the active-agent
    /// indicator (GOALS §1a).
    pub active_agent: Arc<Mutex<String>>,
}

/// Build the driver + spawn the task + return the handle. Errors out
/// (so the TUI can fall back to its "input captured" stub message) when
/// no provider is configured or its auth env var is missing.
pub fn try_spawn(cwd: &Path) -> Result<AgentRunner, String> {
    let providers_cfg = load_providers(cwd)?;
    let model = Model::from_config(&providers_cfg).map_err(|e| format!("model: {e}"))?;
    let model = Arc::new(model);

    let session = Arc::new(Session::new(cwd.to_path_buf()));
    if let Some(active) = &providers_cfg.active_model {
        session.set_active_model(&active.provider, &active.model);
    }
    let locks = Arc::new(LockManager::new());

    let spawn_args = SpawnArgs {
        model: model.clone(),
        params: ModelParams::default(),
    };
    let root = Arc::new(builtin::orchestrator_build(&spawn_args));

    let (input_tx, input_rx) = mpsc::channel::<String>(32);
    let (event_tx, mut event_rx) = mpsc::channel::<TurnEvent>(64);

    let events = Arc::new(Mutex::new(Vec::new()));
    let active_agent = Arc::new(Mutex::new(root.name.clone()));

    let events_for_drain = events.clone();
    let active_for_drain = active_agent.clone();

    let mut driver = Driver::new(session, locks, cwd.to_path_buf(), root);

    // Driver task: runs the engine's main loop, which itself drains
    // the input channel between inference rounds for queued-message
    // folding (GOALS §1c).
    tokio::spawn(async move {
        if let Err(e) = driver.run_main_loop(input_rx, &event_tx).await {
            tracing::error!(error = ?e, "driver error");
            let _ = event_tx
                .send(TurnEvent::ToolError {
                    agent: "engine".into(),
                    call_id: String::new(),
                    tool: "engine".into(),
                    error: format!("{e:#}"),
                })
                .await;
        }
        // Final active-agent snapshot after the loop ends (unlikely;
        // it ends only when the channel closes).
        let name = driver.active_agent().to_string();
        *active_for_drain.lock().unwrap() = name;
    });

    // Event-drain task: pushes events into the shared buffer the TUI
    // reads per tick. Kept separate from the driver task so a slow TUI
    // can't backpressure the model loop.
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            events_for_drain.lock().unwrap().push(event);
        }
    });

    Ok(AgentRunner {
        input_tx,
        events,
        active_agent,
    })
}

fn load_providers(cwd: &Path) -> Result<ProvidersConfig, String> {
    let dirs = discover_config_dirs(cwd);
    let Some(dir) = dirs.first() else {
        return Err("no cockpit config — run /settings to create one".into());
    };
    let path = dir.path.join("config.json");
    let doc = ConfigDoc::load(&path).map_err(|e| format!("config load: {e}"))?;
    Ok(doc.providers())
}

/// One-line summary of a tool call's args for the `→ tool(...)`
/// affordance the TUI renders. Public so [`crate::tui::app`] can
/// reuse it when projecting [`TurnEvent::ToolStart`] into history.
pub fn short_args(v: &serde_json::Value) -> String {
    if let Some(map) = v.as_object() {
        let mut out = String::new();
        for (k, val) in map {
            if !out.is_empty() {
                out.push_str(", ");
            }
            let rendered = match val {
                serde_json::Value::String(s) if s.len() <= 40 => format!("{k}=\"{s}\""),
                serde_json::Value::String(s) => format!("{k}=<{}c>", s.len()),
                serde_json::Value::Bool(b) => format!("{k}={b}"),
                serde_json::Value::Number(n) => format!("{k}={n}"),
                other => format!(
                    "{k}={}",
                    other.to_string().chars().take(40).collect::<String>()
                ),
            };
            out.push_str(&rendered);
            if out.chars().count() > 80 {
                out.push('…');
                break;
            }
        }
        out
    } else {
        v.to_string()
    }
}

/// First non-empty trimmed line of `s`, capped at `max_chars`. Used
/// for tool-output snippets and subagent prompt previews.
pub fn first_line(s: &str, max_chars: usize) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.chars().count() > max_chars {
        let truncated: String = first.chars().take(max_chars).collect();
        format!("{truncated}…")
    } else {
        first.to_string()
    }
}
