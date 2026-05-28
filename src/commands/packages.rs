//! `cockpit packages {list,add}` — thin CLI over the package registry
//! (prompt `docs-agent.md` component A).

use anyhow::{Result, bail};

use crate::cli::{PackagesAddArgs, PackagesCommand};
use crate::db::Db;

pub async fn run(cmd: PackagesCommand) -> Result<()> {
    match cmd {
        PackagesCommand::List => list().await,
        PackagesCommand::Add(args) => add(args).await,
    }
}

async fn list() -> Result<()> {
    let db = Db::open_default()?;
    let packages = db.list_packages()?;
    if packages.is_empty() {
        println!(
            "No packages registered. Add one with `cockpit packages add` or `cockpit kcl import`."
        );
        return Ok(());
    }
    for p in &packages {
        let kind = p.source_type.as_str();
        // Show the display name only when it differs from the identifier
        // (kcl imports often carry a friendlier name).
        let label = if p.display_name == p.identifier {
            p.identifier.clone()
        } else {
            format!("{} ({})", p.identifier, p.display_name)
        };
        match &p.source_url {
            Some(url) => println!("{label}  [{kind}]  {url}  -> {}", p.path),
            None => println!("{label}  [{kind}]  -> {}", p.path),
        }
    }
    println!("\n{} package(s).", packages.len());
    Ok(())
}

async fn add(args: PackagesAddArgs) -> Result<()> {
    if args.git.is_some() && args.path.is_some() {
        bail!("pass either `--git` or `--path`, not both");
    }
    let cwd = std::env::current_dir()?;
    let db = Db::open_default()?;
    // Default is shallow; `--shallow` is the opt-OUT of shallow per the
    // kcl-parity flag name (`--shallow` toggles the full-clone behavior).
    let shallow = !args.shallow;

    if let Some(url) = args.git {
        let row = crate::packages::add_git(
            &db,
            &cwd,
            &args.identifier,
            &url,
            args.branch.as_deref(),
            shallow,
        )?;
        println!("Registered `{}` (git) at {}", row.identifier, row.path);
    } else if let Some(path) = args.path {
        let row = crate::packages::add_local(&db, &args.identifier, &path)?;
        println!("Registered `{}` (local) at {}", row.identifier, row.path);
    } else {
        bail!("`packages add` needs either `--git <url>` or `--path <dir>`");
    }
    Ok(())
}
