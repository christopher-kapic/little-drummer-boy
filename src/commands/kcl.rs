//! `cockpit kcl import` — one-way registry import from a local kcl
//! install (prompt `docs-agent.md` component A). Reads kcl's DB
//! read-only, never writes back.

use anyhow::Result;

use crate::cli::KclCommand;
use crate::db::Db;
use crate::packages::{KclImport, import_from_kcl};

pub async fn run(cmd: KclCommand) -> Result<()> {
    match cmd {
        KclCommand::Import => import().await,
    }
}

async fn import() -> Result<()> {
    let db = Db::open_default()?;
    match import_from_kcl(&db)? {
        KclImport::Imported(n) => {
            println!("Imported {n} package(s) from kcl.");
        }
        KclImport::NoKclDb(path) => {
            println!(
                "No kcl registry found at {} — nothing to import.",
                path.display()
            );
        }
    }
    Ok(())
}
