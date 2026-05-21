//! Configuration loaders for `cockpit`.
//!
//! Two layered formats:
//!
//! - `opencode.json` — read verbatim using opencode's existing locations and
//!   precedence (see `opencode-features-review.md` §3). Unknown keys are
//!   silently ignored to honor the drop-in-replacement goal.
//! - `extended-config.json` — cockpit-only superset; read from the same
//!   locations as `opencode.json`, merged last so it can override
//!   opencode-level keys when needed. See `GOALS.md` §4 for the schema.

pub mod extended;
pub mod opencode;
pub mod resolve;
