#![allow(dead_code)]
//! Credential storage at `$XDG_STATE_HOME/cockpit/credentials.json`
//! (defaulting to `~/.local/state/cockpit/credentials.json`).
//!
//! Why `state` rather than `share`: an auth token is mutable runtime
//! data the program can regenerate (re-login, refresh). `~/.local/share`
//! is for application data files the program does not regenerate.
//!
//! On Unix the file is created with mode `0600`. The file is opaque
//! JSON: `{ "<provider-id>": { ... }, ... }`. The shape of each entry
//! is per-provider — `api_key` for static keys, an OAuth bundle for
//! device-flow providers — so we store them as untyped `serde_json::Value`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

/// Default credentials path: `~/.local/state/cockpit/credentials.json`.
/// Honors `XDG_STATE_HOME` per the XDG spec.
pub fn default_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.trim().is_empty() {
            return Some(PathBuf::from(xdg).join("cockpit/credentials.json"));
        }
    }
    let home = dirs::home_dir()?;
    Some(home.join(".local/state/cockpit/credentials.json"))
}

pub struct CredentialStore {
    path: PathBuf,
    records: BTreeMap<String, Value>,
}

impl CredentialStore {
    pub fn open(path: PathBuf) -> Result<Self> {
        let records = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            if raw.trim().is_empty() {
                BTreeMap::new()
            } else {
                serde_json::from_str::<BTreeMap<String, Value>>(&raw)
                    .with_context(|| format!("parsing {}", path.display()))?
            }
        } else {
            BTreeMap::new()
        };
        Ok(Self { path, records })
    }

    pub fn open_default() -> Result<Self> {
        let path = default_path().context("could not locate $HOME for credentials path")?;
        Self::open(path)
    }

    pub fn get(&self, provider_id: &str) -> Option<&Value> {
        self.records.get(provider_id)
    }

    /// Convenience for the common API-key case.
    pub fn api_key(&self, provider_id: &str) -> Option<String> {
        self.records
            .get(provider_id)?
            .get("api_key")?
            .as_str()
            .map(str::to_string)
    }

    pub fn set(&mut self, provider_id: impl Into<String>, value: Value) {
        self.records.insert(provider_id.into(), value);
    }

    pub fn set_api_key(&mut self, provider_id: impl Into<String>, key: impl Into<String>) {
        self.set(provider_id, serde_json::json!({ "api_key": key.into() }));
    }

    pub fn remove(&mut self, provider_id: &str) {
        self.records.remove(provider_id);
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let pretty = serde_json::to_string_pretty(&self.records)?;
        write_with_0600(&self.path, pretty.as_bytes())?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
fn write_with_0600(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    let mut file = opts
        .open(path)
        .with_context(|| format!("opening {} for write", path.display()))?;
    std::io::Write::write_all(&mut file, bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_with_0600(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trips_an_api_key() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("credentials.json");
        let mut store = CredentialStore::open(path.clone()).unwrap();
        store.set_api_key("opencode-zen", "secret");
        store.save().unwrap();

        let store2 = CredentialStore::open(path).unwrap();
        assert_eq!(store2.api_key("opencode-zen").as_deref(), Some("secret"));
    }

    #[test]
    fn remove_drops_record() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("credentials.json");
        let mut store = CredentialStore::open(path).unwrap();
        store.set_api_key("x", "k");
        store.remove("x");
        assert!(store.get("x").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn file_has_0600_perms_after_save() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("credentials.json");
        let mut store = CredentialStore::open(path.clone()).unwrap();
        store.set_api_key("p", "k");
        store.save().unwrap();
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    #[test]
    fn xdg_state_home_overrides_default_path() {
        // Sanity check: setting XDG_STATE_HOME points the default at it.
        let tmp = TempDir::new().unwrap();
        // Each test process is independent w/ respect to env vars in
        // single-threaded mode; cargo test multithreads so we just
        // observe the result rather than relying on a stable value.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        let path = default_path().unwrap();
        assert!(path.starts_with(tmp.path()));
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }
}
