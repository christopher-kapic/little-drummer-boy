//! Theme palette + JSON theme loader.
//!
//! Themes live in `~/.config/cockpit/themes/*.json` plus any
//! `.cockpit/themes/` on the discovered config path. Built-ins to ship
//! initially: `system`, `tokyonight`, `gruvbox`.

/// Foreground color index used for muted/secondary text across the TUI
/// (status line, popup descriptions, help text).
pub const MUTED_COLOR_INDEX: u8 = 250;
