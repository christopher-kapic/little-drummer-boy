//! Secret redaction.
//!
//! Every prompt that crosses the network goes through `scrub()`. This is
//! a non-bypassable chokepoint by design — see `GOALS.md` §7 and
//! `CLAUDE.md` "Design rules".
//!
//! Sources of secrets:
//!   - `std::env::vars()` (less an allowlist of obviously-not-secret
//!     vars: PATH, HOME, SHELL, TERM, LANG, …).
//!   - Project `.env`, `.env.local`, plus any `extended.redact.extra_dotenv_paths`.
//!     Walks up to the git root.
//!
//! Replacement uses `aho-corasick` for O(n+m) multi-pattern matching across
//! a single linear scan. Matches are case-sensitive and substring-aware
//! (so a token embedded in a longer URL is still redacted).

use std::path::Path;

use anyhow::Result;

use crate::config::extended::RedactConfig;

/// A built lookup table of `value -> origin-name` pairs that the next
/// outbound request must be scrubbed against.
pub struct RedactionTable {
    // Will hold an aho_corasick::AhoCorasick + a parallel Vec<String> of
    // origin names for the `cockpit debug redact` command. Implementation
    // deferred to avoid pulling the dep in before the rest of the
    // skeleton compiles.
    _placeholder: String,
}

impl RedactionTable {
    /// Build the table from the OS environment + every `.env` file in
    /// scope. Honors the `enabled`, `scan_environment`, `scan_dotenv`,
    /// `extra_dotenv_paths`, and `min_secret_length` knobs.
    pub fn build(_cfg: &RedactConfig, _cwd: &Path) -> Result<Self> {
        todo!()
    }

    /// Replace every secret in `body` with the configured placeholder.
    /// Returns the cleaned string. Never mutates `body` in place — the
    /// pre-redaction body must be available for local logs (which are
    /// also redacted, but separately).
    pub fn scrub(&self, _body: &str) -> String {
        todo!()
    }
}
