//! Layered-config directory discovery.
//!
//! Walk order (matches the [[config_layering]] plan):
//!
//!   1. Home-scoped: `~/.config/cockpit/`, then `~/.cockpit/`.
//!   2. Machine-local-but-project-scoped: a hashed-cwd dir under the
//!      cockpit data dir. Lets a user override per-cwd without
//!      committing anything to the repo. Hashing the cwd dodges
//!      filename-invalid characters and path-length limits.
//!   3. Every ancestor of `cwd` containing `.cockpit/`, from `cwd` upward,
//!      stopping at the `{$HOME, /srv, /opt}` stop set.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Where a cockpit config directory was discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDirKind {
    /// `~/.config/cockpit/`
    HomeXdg,
    /// `~/.cockpit/`
    HomeDot,
    /// `<cockpit_data_dir>/local-configs/<hash(cwd)>/` — machine-local
    /// per-cwd config. Never checked into a repo.
    MachineLocal,
    /// An ancestor of cwd containing `.cockpit/` (project-scoped layer).
    Project,
}

#[derive(Debug, Clone)]
pub struct ConfigDir {
    pub kind: ConfigDirKind,
    pub path: PathBuf,
}

/// All cockpit config directories that exist on disk and apply to `cwd`.
pub fn discover_config_dirs(cwd: &Path) -> Vec<ConfigDir> {
    let mut out = Vec::new();

    if let Some(home) = dirs::home_dir() {
        let xdg = home.join(".config/cockpit");
        if xdg.is_dir() {
            out.push(ConfigDir {
                kind: ConfigDirKind::HomeXdg,
                path: xdg,
            });
        }
        let dot = home.join(".cockpit");
        if dot.is_dir() {
            out.push(ConfigDir {
                kind: ConfigDirKind::HomeDot,
                path: dot,
            });
        }
    }

    if let Ok(local) = local_config_dir_for(cwd)
        && local.is_dir()
    {
        out.push(ConfigDir {
            kind: ConfigDirKind::MachineLocal,
            path: local,
        });
    }

    for dir in walk_up_to_stops(cwd) {
        let candidate = dir.join(".cockpit");
        if candidate.is_dir() {
            out.push(ConfigDir {
                kind: ConfigDirKind::Project,
                path: candidate,
            });
        }
    }

    out
}

/// Default places `/settings` will offer when no config exists yet.
pub fn creatable_config_dirs() -> Vec<ConfigDir> {
    let mut out = Vec::new();
    if let Some(home) = dirs::home_dir() {
        out.push(ConfigDir {
            kind: ConfigDirKind::HomeXdg,
            path: home.join(".config/cockpit"),
        });
        out.push(ConfigDir {
            kind: ConfigDirKind::HomeDot,
            path: home.join(".cockpit"),
        });
    }
    out
}

/// Candidate locations for "add a new config scoped to this directory":
/// the project-local `.cockpit/` and the machine-local hashed-cwd dir.
/// Returned even when they don't exist yet — the caller scaffolds them.
pub fn cwd_scoped_creatable_dirs(cwd: &Path) -> Vec<ConfigDir> {
    let mut out = vec![ConfigDir {
        kind: ConfigDirKind::Project,
        path: cwd.join(".cockpit"),
    }];
    if let Ok(local) = local_config_dir_for(cwd) {
        out.push(ConfigDir {
            kind: ConfigDirKind::MachineLocal,
            path: local,
        });
    }
    out
}

/// Stable per-cwd directory under the cockpit data dir. The cwd is
/// canonicalized when possible (so `./foo` and `/abs/foo` map to the
/// same layer), then SHA-256-hashed and truncated to 16 hex chars so
/// it's filename-safe everywhere. Returns an error if the data dir
/// can't be located (no `$HOME` and no XDG data var).
pub fn local_config_dir_for(cwd: &Path) -> anyhow::Result<PathBuf> {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    let base = crate::config::resolve::cockpit_data_dir()?;
    Ok(base.join("local-configs").join(hex))
}

/// Create `dir` (and parents) and write a minimal `config.json` if one
/// isn't already present. Returns the path of the config file.
pub fn scaffold_config_dir(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let config_path = dir.join("config.json");
    if !config_path.exists() {
        let default = "{\n  \"providers\": {},\n  \"agents\": {},\n  \"tools\": {}\n}\n";
        std::fs::write(&config_path, default)?;
    }
    Ok(config_path)
}

/// Walk `cwd` and its ancestors, stopping at the `{$HOME, /srv, /opt}`
/// stop set. Returns the directories in walk order (cwd first).
pub fn walk_up_to_stops(cwd: &Path) -> Vec<PathBuf> {
    let stops: Vec<PathBuf> = [
        dirs::home_dir(),
        Some(PathBuf::from("/srv")),
        Some(PathBuf::from("/opt")),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut out = Vec::new();
    let mut cursor = Some(cwd);
    while let Some(dir) = cursor {
        if stops.iter().any(|s| dir == s) {
            break;
        }
        out.push(dir.to_path_buf());
        cursor = dir.parent();
    }
    out
}
