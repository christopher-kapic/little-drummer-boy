//! Secret redaction.
//!
//! Every string the daemon hands to a model provider goes through
//! [`RedactionTable::scrub`]. This is a non-bypassable chokepoint by
//! design — see `GOALS.md` §7 and `CLAUDE.md` "Design rules".
//!
//! Sources of secrets scanned at table-build time:
//!   - `std::env::vars()` minus a small "obviously not a secret"
//!     allowlist (`PATH`, `HOME`, `SHELL`, `TERM`, `LANG`, …).
//!   - Project `.env`, `.env.local`, walked up to the git root.
//!   - Any paths configured in `extended.redact.extra_dotenv_paths`.
//!
//! Replacement is single-linear-scan multi-pattern via `aho-corasick`.
//! Matches are case-sensitive and substring-aware (so a token embedded
//! in a longer URL is still redacted).

use std::path::{Path, PathBuf};

use aho_corasick::{AhoCorasick, MatchKind};
use anyhow::Result;

use crate::config::extended::RedactConfig;

/// Env vars that are *never* treated as secrets even when they would
/// otherwise meet the length threshold. Substrings of these values
/// would be redacted out of every shell pipeline if we let them in,
/// for no security benefit.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "USERNAME",
    "SHELL",
    "TERM",
    "TERM_PROGRAM",
    "PWD",
    "OLDPWD",
    "DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "HOSTNAME",
    "LOGNAME",
    "EDITOR",
    "VISUAL",
    "PAGER",
    "TZ",
    "TMPDIR",
    "TEMP",
    "TMP",
    "COLORTERM",
    "OS",
    "OSTYPE",
];

/// Prefix-matched allowlist entries — any env var whose name starts
/// with one of these is skipped. Covers the `LC_*`, `LANG*`, and `XDG_*`
/// families called out in the spec.
const ENV_ALLOWLIST_PREFIXES: &[&str] = &["LC_", "LANG", "XDG_"];

/// `true` when `name` is in the built-in allowlist (exact match or any
/// prefix family) or in the user's per-config `allowlist`.
fn is_allowlisted(name: &str, user_allowlist: &[String]) -> bool {
    if ENV_ALLOWLIST.contains(&name) {
        return true;
    }
    if ENV_ALLOWLIST_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    user_allowlist.iter().any(|a| a == name)
}

/// A built lookup of `value → origin-name` pairs the next outbound
/// request must be scrubbed against. Hold one per session (cheap to
/// rebuild; small in-memory footprint).
pub struct RedactionTable {
    /// Aho-Corasick search structure; `None` when there's nothing to
    /// scrub or redaction is disabled. Keeping it `Option` lets
    /// [`scrub`] short-circuit without allocating.
    matcher: Option<AhoCorasick>,
    /// Parallel to `matcher`'s pattern list. Used by
    /// `cockpit debug redact` to render `value (from $VAR)` rows.
    origins: Vec<String>,
    /// What every match is replaced with. Distinctive on purpose so
    /// leaks into provider logs are easy to grep for.
    placeholder: String,
    /// `true` when the user disabled redaction at config level. The
    /// scrub call still returns the input unchanged; we keep the flag
    /// so `cockpit debug redact` can say so.
    disabled: bool,
}

impl RedactionTable {
    /// Build a table from the OS env + `.env` files under `cwd`.
    /// Honors `enabled`, `scan_environment`, `scan_dotenv`,
    /// `extra_dotenv_paths`, and `min_secret_length`.
    pub fn build(cfg: &RedactConfig, cwd: &Path) -> Result<Self> {
        if !cfg.enabled {
            return Ok(Self {
                matcher: None,
                origins: Vec::new(),
                placeholder: cfg.placeholder.clone(),
                disabled: true,
            });
        }

        let mut entries: Vec<(String, String)> = Vec::new();

        if cfg.scan_environment {
            for (name, value) in std::env::vars() {
                if is_allowlisted(&name, &cfg.allowlist) {
                    continue;
                }
                if value.len() < cfg.min_secret_length {
                    continue;
                }
                entries.push((value, format!("${name}")));
            }
        }

        if cfg.scan_dotenv {
            for path in collect_dotenv_paths(cwd, &cfg.extra_dotenv_paths) {
                if let Ok(file_entries) =
                    read_dotenv_file(&path, cfg.min_secret_length, &cfg.allowlist)
                {
                    entries.extend(file_entries);
                }
            }
        }

        // Denylist: forced inclusion even for short / allowlisted values.
        for v in &cfg.denylist {
            if v.is_empty() {
                continue;
            }
            entries.push((v.clone(), "$denylist".to_string()));
        }

        // Sort longest-first so that overlapping patterns prefer the
        // longer match (`aho-corasick` with LeftmostLongest does this
        // implicitly, but sorting also gives the debug-dump a stable
        // canonical order).
        entries.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

        // De-duplicate identical values; we don't want to redact a
        // single value twice (the placeholder would still be right but
        // the origins list would carry stale entries).
        entries.dedup_by(|a, b| a.0 == b.0);

        if entries.is_empty() {
            return Ok(Self {
                matcher: None,
                origins: Vec::new(),
                placeholder: cfg.placeholder.clone(),
                disabled: false,
            });
        }

        let patterns: Vec<&str> = entries.iter().map(|(v, _)| v.as_str()).collect();
        let matcher = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .ascii_case_insensitive(false)
            .build(&patterns)
            .map_err(|e| anyhow::anyhow!("building aho-corasick: {e}"))?;
        let origins = entries.iter().map(|(_, o)| o.clone()).collect();

        Ok(Self {
            matcher: Some(matcher),
            origins,
            placeholder: cfg.placeholder.clone(),
            disabled: false,
        })
    }

    /// Scrub every secret in `body`. Returns the cleaned string. The
    /// no-table-or-disabled path returns the input unchanged without
    /// allocating.
    pub fn scrub(&self, body: &str) -> String {
        let Some(matcher) = self.matcher.as_ref() else {
            return body.to_string();
        };
        matcher.replace_all(body, &vec![self.placeholder.as_str(); self.origins.len()])
    }

    /// `true` when there's nothing to redact and `scrub` will pass
    /// through. Useful for the debug command.
    pub fn is_empty(&self) -> bool {
        self.matcher.is_none()
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }

    /// `(value, origin)` pairs for the debug command. Values themselves
    /// are sensitive — only call this from local `cockpit debug
    /// redact` after the user has explicitly asked.
    pub fn entries_for_debug(&self) -> Vec<&str> {
        self.origins.iter().map(|s| s.as_str()).collect()
    }
}

/// Returns every `.env`-style path that applies to `cwd` plus the
/// user's `extra_dotenv_paths`. Walks ancestors (stopping at the git
/// root if found, falling back to a small fixed depth otherwise).
fn collect_dotenv_paths(cwd: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    let mut cursor: Option<&Path> = Some(cwd);
    let mut depth = 0usize;
    let max_depth = 12;
    while let Some(dir) = cursor {
        for name in [".env", ".env.local"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                out.push(candidate);
            }
        }
        // Stop at git root.
        if dir.join(".git").exists() {
            break;
        }
        depth += 1;
        if depth >= max_depth {
            break;
        }
        cursor = dir.parent();
    }

    for p in extra {
        if p.is_file() {
            out.push(p.clone());
        }
    }

    out
}

/// Parse a `.env` file and yield `(value, "$VAR (file.env)")` pairs
/// for every entry whose value is at least `min_len` chars and whose
/// name is not in the user's per-config allowlist. Tolerates missing
/// files by virtue of being called from a guarded site.
///
/// We parse by hand rather than going through `dotenvy::from_path_iter`
/// — that one calls `std::env::set_var` as a side effect on some
/// versions, which is not what a secret-scan path should do.
fn read_dotenv_file(
    path: &Path,
    min_len: usize,
    user_allowlist: &[String],
) -> Result<Vec<(String, String)>> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some(eq) = line.find('=') else { continue };
        let (name, value) = line.split_at(eq);
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        if is_allowlisted(&name, user_allowlist) {
            continue;
        }
        let value = value[1..].trim();
        // Strip surrounding quotes if present.
        let value = strip_quotes(value);
        if value.len() < min_len {
            continue;
        }
        let origin = format!("${name} ({})", path.display());
        out.push((value.to_string(), origin));
    }
    Ok(out)
}

fn strip_quotes(s: &str) -> &str {
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn enabled_cfg() -> RedactConfig {
        RedactConfig {
            enabled: true,
            scan_environment: false,
            scan_dotenv: false,
            extra_dotenv_paths: vec![],
            min_secret_length: 8,
            placeholder: "***REDACT***".into(),
            denylist: vec![],
            allowlist: vec![],
        }
    }

    #[test]
    fn disabled_passes_through() {
        let mut cfg = enabled_cfg();
        cfg.enabled = false;
        let dir = TempDir::new().unwrap();
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        assert!(t.disabled());
        assert_eq!(t.scrub("sk-secret-token"), "sk-secret-token");
    }

    #[test]
    fn empty_passes_through() {
        let cfg = enabled_cfg();
        let dir = TempDir::new().unwrap();
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        assert!(t.is_empty());
        assert_eq!(t.scrub("anything goes"), "anything goes");
    }

    #[test]
    fn dotenv_values_redacted() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env");
        std::fs::write(
            &env_path,
            "API_KEY=sk-super-secret-token-1234\nUSER_VAR=ignored-short\n# comment\nQUOTED=\"another-long-secret-here\"\n",
        )
        .unwrap();
        let mut cfg = enabled_cfg();
        cfg.scan_dotenv = true;
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        let body = "got sk-super-secret-token-1234 and another-long-secret-here";
        let scrubbed = t.scrub(body);
        assert!(!scrubbed.contains("sk-super-secret-token-1234"));
        assert!(!scrubbed.contains("another-long-secret-here"));
        assert!(scrubbed.contains("***REDACT***"));
    }

    #[test]
    fn short_values_skipped() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env");
        std::fs::write(&env_path, "SHORT=abc\nLONG=long-enough-value-here\n").unwrap();
        let mut cfg = enabled_cfg();
        cfg.scan_dotenv = true;
        cfg.min_secret_length = 8;
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        // The 3-char value would have created a useless pattern; check
        // that benign substrings aren't replaced.
        assert_eq!(t.scrub("abc def"), "abc def");
        assert_eq!(t.scrub("long-enough-value-here"), "***REDACT***");
    }

    #[test]
    fn substring_matches() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env");
        std::fs::write(&env_path, "TOKEN=embedded-secret-abc\n").unwrap();
        let mut cfg = enabled_cfg();
        cfg.scan_dotenv = true;
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        let scrubbed = t.scrub("the URL is https://api.example.com?t=embedded-secret-abc&u=x");
        assert!(scrubbed.contains("***REDACT***"));
        assert!(!scrubbed.contains("embedded-secret-abc"));
    }

    #[test]
    fn default_placeholder_is_the_explicit_string() {
        // The user-visible placeholder is part of the spec; if anyone
        // edits the default, this test fails on purpose.
        let cfg = RedactConfig::default();
        assert_eq!(
            cfg.placeholder,
            "**REDACTED BY COCKPIT - DO NOT TRY TO OBTAIN BY WORKAROUND**"
        );
    }

    #[test]
    fn env_var_value_redacted_with_default_placeholder() {
        // Set a dedicated env var and confirm it lands in the table and
        // gets scrubbed to the default placeholder. Use a value name
        // unique enough that prior env state can't fight us.
        let key = "COCKPIT_TEST_SECRET_TOKEN_XYZ";
        let val = "supersecret-token-value-1234";
        // SAFETY: tests run single-threaded enough that env mutation
        // here is acceptable; the same pattern is used elsewhere in the
        // test suite.
        unsafe {
            std::env::set_var(key, val);
        }
        let cfg = RedactConfig {
            enabled: true,
            scan_environment: true,
            scan_dotenv: false,
            extra_dotenv_paths: vec![],
            min_secret_length: 8,
            placeholder: RedactConfig::default().placeholder,
            denylist: vec![],
            allowlist: vec![],
        };
        let dir = TempDir::new().unwrap();
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        let scrubbed = t.scrub(&format!("the token is {val} ok"));
        assert!(scrubbed.contains("**REDACTED BY COCKPIT - DO NOT TRY TO OBTAIN BY WORKAROUND**"));
        assert!(!scrubbed.contains(val));
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn short_env_values_not_redacted() {
        let key = "COCKPIT_TEST_SHORT_VALUE";
        let val = "abc";
        unsafe {
            std::env::set_var(key, val);
        }
        let mut cfg = enabled_cfg();
        cfg.scan_environment = true;
        cfg.min_secret_length = 8;
        let dir = TempDir::new().unwrap();
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        // The 3-char value must not contribute a pattern.
        assert_eq!(t.scrub("the value is abc here"), "the value is abc here");
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn allowlisted_path_not_redacted_even_when_long() {
        // PATH is almost always long enough to clear min_secret_length;
        // confirm $PATH (and the LC_/LANG/XDG_ families) are never in
        // the table even with min_secret_length lowered all the way.
        // (Other env vars' values may still be substrings of PATH —
        // that's an inherent property of substring redaction and is
        // covered by `allowlisted_env_var_names_not_in_table`.)
        let mut cfg = enabled_cfg();
        cfg.scan_environment = true;
        cfg.min_secret_length = 1;
        let dir = TempDir::new().unwrap();
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        let origins = t.entries_for_debug();
        for skipped in ["$PATH", "$HOME", "$LANG", "$LC_ALL", "$XDG_RUNTIME_DIR"] {
            assert!(
                !origins.contains(&skipped),
                "expected allowlisted origin `{skipped}` to be absent"
            );
        }
        for name in ["LC_ALL", "LANG", "XDG_RUNTIME_DIR"] {
            assert!(
                is_allowlisted(name, &[]),
                "expected `{name}` to be allowlisted by prefix"
            );
        }
    }

    #[test]
    fn denylisted_value_always_redacted_including_short() {
        let mut cfg = enabled_cfg();
        cfg.scan_environment = false;
        cfg.scan_dotenv = false;
        cfg.min_secret_length = 16; // huge threshold so length can't help
        cfg.denylist = vec!["sek".into()]; // 3 chars — would normally fail
        let dir = TempDir::new().unwrap();
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        let scrubbed = t.scrub("the keyword sek appears here");
        assert!(scrubbed.contains("***REDACT***"));
        assert!(!scrubbed.contains(" sek "));
    }

    #[test]
    fn denylist_overrides_allowlisted_env_var() {
        // Even if the user added FOO to the allowlist, putting its
        // literal value on the denylist forces redaction.
        let mut cfg = enabled_cfg();
        cfg.scan_environment = false;
        cfg.scan_dotenv = false;
        cfg.denylist = vec!["my-allowlisted-value".into()];
        cfg.allowlist = vec!["FOO".into()];
        let dir = TempDir::new().unwrap();
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        let scrubbed = t.scrub("got my-allowlisted-value back");
        assert!(scrubbed.contains("***REDACT***"));
        assert!(!scrubbed.contains("my-allowlisted-value"));
    }

    #[test]
    fn user_allowlist_skips_dotenv_entry() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env");
        std::fs::write(&env_path, "USER_TOKEN=very-long-allowed-value\n").unwrap();
        let mut cfg = enabled_cfg();
        cfg.scan_dotenv = true;
        cfg.allowlist = vec!["USER_TOKEN".into()];
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        assert_eq!(
            t.scrub("got very-long-allowed-value"),
            "got very-long-allowed-value"
        );
    }

    #[test]
    fn allowlisted_env_var_names_not_in_table() {
        // The allowlist works by *name*: even with scan_environment
        // on, `$PATH`/`$HOME`/`$SHELL` etc. must not contribute
        // patterns to the matcher. (Substring overlap with other env
        // vars is a separate concern and an inherent property of
        // substring redaction; that's fine — we just don't want PATH
        // itself catalogued.)
        let cfg = RedactConfig {
            enabled: true,
            scan_environment: true,
            scan_dotenv: false,
            extra_dotenv_paths: vec![],
            min_secret_length: 1,
            placeholder: "***".into(),
            denylist: vec![],
            allowlist: vec![],
        };
        let dir = TempDir::new().unwrap();
        let t = RedactionTable::build(&cfg, dir.path()).unwrap();
        let origins = t.entries_for_debug();
        for name in ENV_ALLOWLIST {
            let key = format!("${name}");
            assert!(
                !origins.contains(&key.as_str()),
                "allowlisted env var {name} leaked into the redaction table"
            );
        }
    }
}
