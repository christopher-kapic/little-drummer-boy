//! Slash-command menu.
//!
//! The set is the union of:
//!   - Built-ins (see `opencode-features-review.md` §5 "Built-in slash
//!     commands").
//!   - User-defined commands from `~/.config/opencode/commands/*.md`
//!     and `<project>/.opencode/commands/*.md`.
//!
//! Built-ins that change in cockpit:
//!   - `/vim` — toggle composer vim mode (default ON in cockpit).
//!   - `/skills` — also includes `~/.claude/skills/`.
//!   - `/redact` — cockpit-only; shows what would be substituted next request.
//!   - `/mcp` — cockpit-only; prints the mcp2cli pointer and exits.
//!   - `/share`, `/statusline`, `/terminaltitle` — omitted (see review).
