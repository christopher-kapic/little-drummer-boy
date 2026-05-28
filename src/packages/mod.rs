//! Package registry operations for the `docs` agent (GOALS §3a, prompt
//! `docs-agent.md` components A + decision 4).
//!
//! This module owns the side-effecting half of the registry: resolving
//! the cockpit clone directory, deriving collision-safe identifiers and
//! directory names, looking a dependency's source repo up from official
//! package-registry metadata (never a guessed URL — decision 4), shallow
//! Git clones, and the one-way `cockpit kcl import`. The pure DB CRUD
//! lives in [`crate::db::packages`].

pub mod resolve;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::db::Db;
use crate::db::packages::{NewPackage, PackageRow, SourceType};

/// Default cockpit clone directory when `packages_directory` is unset.
/// Distinct from kcl's `~/src/kcl-packages` so the two registries never
/// fight over a clone tree.
pub const DEFAULT_CLONE_SUBDIR: &str = "src/cockpit-packages";

/// Resolve the directory cockpit clones Git packages into. Honors the
/// `packages_directory` config key (tilde-expanded), else
/// `~/src/cockpit-packages/`.
pub fn clone_dir(cwd: &Path) -> Result<PathBuf> {
    if let Some(dir) = configured_clone_dir(cwd) {
        return Ok(dir);
    }
    let home = dirs::home_dir().context("could not locate home dir")?;
    Ok(home.join(DEFAULT_CLONE_SUBDIR))
}

/// Read `packages_directory` from the first layered `extended-config.json`
/// that sets it, tilde-expanded. `None` when unset.
fn configured_clone_dir(cwd: &Path) -> Option<PathBuf> {
    use crate::config::dirs::discover_config_dirs;
    use crate::config::extended::ExtendedConfigDoc;
    discover_config_dirs(cwd)
        .into_iter()
        .find_map(|d| ExtendedConfigDoc::load(&d.path.join("extended-config.json")).ok())
        .and_then(|d| d.config().packages_directory.clone())
        .map(|p| {
            let expanded = shellexpand::tilde(&p.to_string_lossy()).into_owned();
            PathBuf::from(expanded)
        })
}

/// Percent-encode an identifier for use as a directory name. Encodes
/// every byte that isn't an unreserved URL char (`A-Za-z0-9._-`), so
/// `npm:@tanstack/query` becomes a single flat, filesystem-safe segment
/// — matching kcl's clone-dir scheme.
pub fn percent_encode_identifier(identifier: &str) -> String {
    let mut out = String::with_capacity(identifier.len());
    for &b in identifier.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0xf));
        }
    }
    out
}

fn hex_upper(nibble: u8) -> char {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    HEX[nibble as usize] as char
}

/// Supported ecosystems for autonomous repo resolution + identifier
/// slugging (prompt component A + decision 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ecosystem {
    Cargo,
    Npm,
    Pip,
}

impl Ecosystem {
    /// The identifier prefix (`cargo`, `npm`, `pip`).
    pub fn prefix(self) -> &'static str {
        match self {
            Ecosystem::Cargo => "cargo",
            Ecosystem::Npm => "npm",
            Ecosystem::Pip => "pip",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cargo" | "crates" | "rust" => Some(Ecosystem::Cargo),
            "npm" | "node" => Some(Ecosystem::Npm),
            "pip" | "pypi" | "python" => Some(Ecosystem::Pip),
            _ => None,
        }
    }
}

/// Derive the ecosystem-prefixed identifier slug for an autonomous add
/// (`cargo:tokio`, `npm:@tanstack/query`, `pip:requests`). Avoids
/// cross-ecosystem collisions (decision: "preserve kcl's identifiers
/// verbatim on import" — that path doesn't go through here).
pub fn ecosystem_slug(eco: Ecosystem, name: &str) -> String {
    format!("{}:{name}", eco.prefix())
}

/// Register a Local package: an absolute on-disk `path`, no clone. The
/// identifier defaults to the path's final component when not given.
pub fn add_local(db: &Db, identifier: &str, path: &Path) -> Result<PackageRow> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("resolving local package path `{}`", path.display()))?;
    if !canonical.is_dir() {
        bail!("local package path `{}` is not a directory", path.display());
    }
    db.upsert_package(&NewPackage {
        identifier: identifier.to_string(),
        display_name: identifier.to_string(),
        source_type: SourceType::Local,
        source_url: None,
        source_branch: None,
        path: canonical.to_string_lossy().into_owned(),
        shallow: false,
    })
}

/// Register a Git package: shallow-clone `url` (unless `shallow` is
/// false) into the clone dir under a percent-encoded identifier, then
/// upsert. Deduped by `source_url`: if a package with the same repo is
/// already registered, its clone is reused (no second clone) and the
/// new identifier points at the same on-disk `path`.
///
/// `branch` is recorded so a future `cockpit packages update` can pull
/// the right ref; when `Some`, the clone is restricted to that branch.
pub fn add_git(
    db: &Db,
    cwd: &Path,
    identifier: &str,
    url: &str,
    branch: Option<&str>,
    shallow: bool,
) -> Result<PackageRow> {
    // Repo dedupe: reuse an existing clone for the same URL.
    if let Some(existing) = db.package_by_source_url(url)? {
        return db.upsert_package(&NewPackage {
            identifier: identifier.to_string(),
            display_name: identifier.to_string(),
            source_type: SourceType::Git,
            source_url: Some(url.to_string()),
            source_branch: branch
                .map(str::to_string)
                .or(existing.source_branch.clone()),
            path: existing.path.clone(),
            shallow: existing.shallow,
        });
    }

    let dir = clone_dir(cwd)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating clone dir `{}`", dir.display()))?;
    let dest = dir.join(percent_encode_identifier(identifier));

    // Concurrency: if the destination already holds a clone (a racing
    // caller got there first), reuse it rather than re-cloning.
    if dest.join(".git").is_dir() {
        return db.upsert_package(&NewPackage {
            identifier: identifier.to_string(),
            display_name: identifier.to_string(),
            source_type: SourceType::Git,
            source_url: Some(url.to_string()),
            source_branch: branch.map(str::to_string),
            path: dest.to_string_lossy().into_owned(),
            shallow,
        });
    }

    git_clone(url, &dest, branch, shallow)
        .with_context(|| format!("cloning `{url}` into `{}`", dest.display()))?;

    db.upsert_package(&NewPackage {
        identifier: identifier.to_string(),
        display_name: identifier.to_string(),
        source_type: SourceType::Git,
        source_url: Some(url.to_string()),
        source_branch: branch.map(str::to_string),
        path: dest.to_string_lossy().into_owned(),
        shallow,
    })
}

/// Run `git clone`. Shallow (`--depth 1`) by default to bound disk/time
/// for large dependencies (prompt decision 4). A non-zero exit surfaces
/// the captured stderr as the error (clean failure, no panic).
fn git_clone(url: &str, dest: &Path, branch: Option<&str>, shallow: bool) -> Result<()> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("clone");
    if shallow {
        cmd.arg("--depth").arg("1");
    }
    if let Some(b) = branch {
        cmd.arg("--branch").arg(b);
    }
    cmd.arg("--").arg(url).arg(dest);
    let output = cmd
        .output()
        .context("spawning `git clone` (is git installed and on PATH?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git clone failed: {}", stderr.trim());
    }
    Ok(())
}

/// Import packages from kcl's registry that cockpit doesn't already have.
/// Reads kcl's `~/.local/share/kcl/kcl.db` (honoring `$XDG_DATA_HOME`),
/// referencing kcl's on-disk `path` as-is (no re-clone). One-way:
/// never writes to kcl's DB. Returns the number of packages added.
///
/// Dedupe matches the registry's own: by `identifier`, and additionally
/// by `source_url` for Git packages (so a repo cockpit already tracks
/// under a different identifier isn't re-imported).
pub fn import_from_kcl(db: &Db) -> Result<KclImport> {
    let kcl_db_path = kcl_db_path()?;
    if !kcl_db_path.exists() {
        return Ok(KclImport::NoKclDb(kcl_db_path));
    }

    let conn = rusqlite::Connection::open_with_flags(
        &kcl_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("opening kcl db at {}", kcl_db_path.display()))?;

    let mut stmt = conn
        .prepare(
            "SELECT identifier, display_name, source_type, source_url, source_branch, path, shallow \
             FROM packages",
        )
        .context("preparing kcl packages query")?;
    let rows = stmt
        .query_map([], |row| {
            let shallow: i64 = row.get(6)?;
            Ok(NewPackage {
                identifier: row.get(0)?,
                display_name: row.get(1)?,
                source_type: SourceType::from_str(&row.get::<_, String>(2)?),
                source_url: row.get(3)?,
                source_branch: row.get(4)?,
                path: row.get(5)?,
                shallow: shallow != 0,
            })
        })
        .context("querying kcl packages")?;

    let mut added = 0u32;
    for row in rows {
        let pkg = row.context("decoding kcl package row")?;
        // Skip if we already have this identifier, or (for Git) this repo.
        if db.package_by_identifier(&pkg.identifier)?.is_some() {
            continue;
        }
        if pkg.source_type == SourceType::Git
            && let Some(url) = &pkg.source_url
            && db.package_by_source_url(url)?.is_some()
        {
            continue;
        }
        let (_, inserted) = db.insert_package_if_absent(&pkg)?;
        if inserted {
            added += 1;
        }
    }
    Ok(KclImport::Imported(added))
}

/// Outcome of [`import_from_kcl`].
pub enum KclImport {
    /// kcl's DB was found; `n` packages were added.
    Imported(u32),
    /// No kcl DB at the resolved path — clean no-op, not an error.
    NoKclDb(PathBuf),
}

/// Resolve kcl's DB path: `$XDG_DATA_HOME/kcl/kcl.db` if set, else
/// `~/.local/share/kcl/kcl.db`.
fn kcl_db_path() -> Result<PathBuf> {
    if let Ok(s) = std::env::var("XDG_DATA_HOME")
        && !s.trim().is_empty()
    {
        return Ok(PathBuf::from(s).join("kcl").join("kcl.db"));
    }
    let home = dirs::home_dir().context("could not locate home dir")?;
    Ok(home.join(".local/share/kcl/kcl.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_keeps_unreserved_escapes_rest() {
        assert_eq!(percent_encode_identifier("tokio"), "tokio");
        assert_eq!(percent_encode_identifier("cargo:tokio"), "cargo%3Atokio");
        assert_eq!(
            percent_encode_identifier("npm:@tanstack/query"),
            "npm%3A%40tanstack%2Fquery"
        );
        // The result is a single flat path segment.
        assert!(!percent_encode_identifier("npm:@tanstack/query").contains('/'));
    }

    #[test]
    fn ecosystem_slug_prefixes() {
        assert_eq!(ecosystem_slug(Ecosystem::Cargo, "tokio"), "cargo:tokio");
        assert_eq!(
            ecosystem_slug(Ecosystem::Npm, "@tanstack/query"),
            "npm:@tanstack/query"
        );
        assert_eq!(ecosystem_slug(Ecosystem::Pip, "requests"), "pip:requests");
    }

    #[test]
    fn add_git_dedupes_by_source_url() {
        let db = Db::open_in_memory().unwrap();
        // Pre-register a repo with a known on-disk path (no real clone).
        db.upsert_package(&NewPackage {
            identifier: "first".into(),
            display_name: "first".into(),
            source_type: SourceType::Git,
            source_url: Some("https://example.invalid/repo".into()),
            source_branch: Some("main".into()),
            path: "/existing/clone".into(),
            shallow: true,
        })
        .unwrap();
        // Adding a second identifier for the same URL must reuse the path
        // and NOT attempt a clone (the URL is unreachable; a clone would
        // error). This exercises the dedupe branch.
        let tmp = tempfile::tempdir().unwrap();
        let row = add_git(
            &db,
            tmp.path(),
            "second",
            "https://example.invalid/repo",
            None,
            true,
        )
        .unwrap();
        assert_eq!(row.path, "/existing/clone");
        assert_eq!(row.identifier, "second");
    }

    #[test]
    fn import_missing_kcl_db_is_clean() {
        // Point XDG_DATA_HOME at an empty dir so kcl.db is absent.
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        unsafe { std::env::set_var("XDG_DATA_HOME", tmp.path()) };
        let db = Db::open_in_memory().unwrap();
        let result = import_from_kcl(&db).unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
        assert!(matches!(result, KclImport::NoKclDb(_)));
    }
}
