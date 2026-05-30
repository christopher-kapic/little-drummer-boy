//! Theme palette + JSON theme loader.
//!
//! Themes live in `~/.config/cockpit/themes/*.json` plus any
//! `.cockpit/themes/` on the discovered config path. Built-ins to ship
//! initially: `system`, `tokyonight`, `gruvbox`.

/// Foreground color index used for muted/secondary text across the TUI
/// (status line, popup descriptions, help text).
pub const MUTED_COLOR_INDEX: u8 = 250;

/// Accent blue used for the rounded outlines (user-message bubble,
/// launch-banner box). The brighter blue that reads as the app accent
/// against the surrounding chrome.
pub const ACCENT_BLUE_INDEX: u8 = 33;

/// Orange used for a subagent's (child) name in the delegation
/// running-line and the `… worked for …` / `… failed after …` header.
/// Only the child name carries it; the parent name uses the default
/// style.
pub const SUBAGENT_ORANGE_INDEX: u8 = 208;
