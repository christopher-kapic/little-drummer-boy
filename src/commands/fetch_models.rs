//! `cockpit fetch-models` — refresh every configured provider's model
//! list by hitting its OpenAI-compatible `/models` endpoint.
//!
//! Drift policy: if the upstream listing omits a model the user already
//! has configured, the command prompts with three options and a
//! "don't ask again" toggle. The non-interactive `--on-unlisted` flag
//! bypasses the prompt (CI use). The chosen default is persisted as
//! `on_unlisted_models_fetch` under `config.json` so future runs skip
//! the prompt.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::cli::FetchModelsArgs;
use crate::config::dirs::discover_config_dirs;
use crate::config::providers::{ConfigDoc, OnUnlistedModelsFetch, ProviderEntry, ProvidersConfig};
use crate::providers::models_fetch::{self, FetchOutcome};

pub async fn run(args: FetchModelsArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("getting cwd")?;
    let dirs = discover_config_dirs(&cwd);
    let config_path: PathBuf = dirs
        .first()
        .map(|d| d.path.join("config.json"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no cockpit config found — run `/settings` inside the TUI to create one"
            )
        })?;

    let mut doc = ConfigDoc::load(&config_path)?;
    let mut cfg = doc.providers();

    let policy_override = match args.on_unlisted.as_deref() {
        Some("keep") => Some(OnUnlistedModelsFetch::Keep),
        Some("remove") => Some(OnUnlistedModelsFetch::Remove),
        Some("ask") => Some(OnUnlistedModelsFetch::Ask),
        Some(other) => anyhow::bail!("--on-unlisted must be keep|remove|ask, got `{other}`"),
        None => None,
    };

    let targets: Vec<String> = if let Some(p) = args.provider.as_ref() {
        if !cfg.providers.contains_key(p) {
            anyhow::bail!("no provider with id `{p}` in {}", config_path.display());
        }
        vec![p.clone()]
    } else {
        cfg.providers.keys().cloned().collect()
    };

    if targets.is_empty() {
        println!("no providers configured");
        return Ok(());
    }

    let mut summaries: Vec<(String, Result<FetchOutcome, anyhow::Error>)> = Vec::new();
    for id in &targets {
        let entry = cfg.providers.get(id).expect("filtered above").clone();
        println!("→ {id} ({})", entry.url);

        let (_, missing) = models_fetch::resolve_headers(&entry.headers);
        if !missing.is_empty() {
            println!("  ⚠ skipped: missing env var(s): {}", missing.join(", "));
            summaries.push((
                id.clone(),
                Err(anyhow::anyhow!("missing env vars: {}", missing.join(", "))),
            ));
            continue;
        }

        let outcome =
            models_fetch::fetch_models(&entry.url, &entry.headers, Some(Duration::from_secs(15)))
                .await;

        match &outcome {
            Ok(FetchOutcome::Models(models)) => {
                println!("  ✓ {} model(s) fetched", models.len());
            }
            Ok(FetchOutcome::Unsupported) => {
                println!("  · no /models endpoint (404) — skipped");
            }
            Err(e) => {
                println!("  ✗ {e}");
            }
        }
        summaries.push((id.clone(), outcome));
    }

    // Detect drift (config models not in remote) before mutating cfg.
    let drift: Vec<(String, Vec<String>)> = summaries
        .iter()
        .filter_map(|(id, outcome)| match outcome {
            Ok(FetchOutcome::Models(remote)) => {
                let entry = cfg.providers.get(id)?;
                let missing: Vec<String> = entry
                    .models
                    .iter()
                    .filter(|m| !remote.iter().any(|r| r.id == m.id))
                    .map(|m| m.id.clone())
                    .collect();
                if missing.is_empty() {
                    None
                } else {
                    Some((id.clone(), missing))
                }
            }
            _ => None,
        })
        .collect();

    let decision = pick_policy(&mut cfg, policy_override, &drift)?;

    // Apply decisions.
    for (id, outcome) in summaries {
        if let Ok(FetchOutcome::Models(models)) = outcome {
            let entry = cfg.providers.get_mut(&id).expect("populated");
            apply_models(entry, models, decision);
        }
    }

    doc.write(&cfg).context("writing config.json")?;
    println!("config.json updated.");
    Ok(())
}

fn apply_models(
    entry: &mut ProviderEntry,
    remote: Vec<crate::config::providers::ModelEntry>,
    decision: OnUnlistedModelsFetch,
) {
    match decision {
        OnUnlistedModelsFetch::Remove | OnUnlistedModelsFetch::Ask => {
            // For Ask, the prompt has already converted to a concrete choice
            // by the time this is called.
            entry.models = remote;
        }
        OnUnlistedModelsFetch::Keep => {
            // Merge: keep config-only entries verbatim, but overlay any
            // remote-side updates onto the matching ids.
            let mut new = remote;
            for old in &entry.models {
                if !new.iter().any(|n| n.id == old.id) {
                    new.push(old.clone());
                }
            }
            entry.models = new;
        }
    }
    entry.models_fetched_at = Some(chrono::Utc::now());
}

fn pick_policy(
    cfg: &mut ProvidersConfig,
    explicit: Option<OnUnlistedModelsFetch>,
    drift: &[(String, Vec<String>)],
) -> Result<OnUnlistedModelsFetch> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    if drift.is_empty() {
        return Ok(cfg
            .on_unlisted_models_fetch
            .unwrap_or(OnUnlistedModelsFetch::Keep));
    }
    let stored = cfg.on_unlisted_models_fetch;
    if matches!(stored, Some(OnUnlistedModelsFetch::Keep))
        || matches!(stored, Some(OnUnlistedModelsFetch::Remove))
    {
        return Ok(stored.unwrap());
    }

    // Interactive prompt.
    println!();
    println!("Some configured models are not in the upstream /models list:");
    for (pid, mids) in drift {
        for mid in mids {
            println!("  {pid} › {mid}");
        }
    }
    println!();
    println!("  [1] Don't remove unlisted models (default)");
    println!("  [2] Remove unlisted models");
    println!("  [3] Don't ask again (apply default, persist)");
    print!("Choose 1/2/3: ");
    std::io::stdout().flush().ok();

    let stdin = std::io::stdin();
    let mut buf = String::new();
    stdin.lock().read_line(&mut buf).ok();
    let pick = match buf.trim() {
        "2" => OnUnlistedModelsFetch::Remove,
        "3" => {
            cfg.on_unlisted_models_fetch = Some(OnUnlistedModelsFetch::Keep);
            OnUnlistedModelsFetch::Keep
        }
        _ => OnUnlistedModelsFetch::Keep,
    };
    Ok(pick)
}
