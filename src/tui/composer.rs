//! Prompt composer.
//!
//! Vim mode is **default on** (`GOALS.md` §1b). This deviates from codex
//! (vim is opt-in there) — Vim users shouldn't have to discover a slash
//! command before they can `dd` a line.
//!
//! Modes:
//!   - `Insert`  — standard editor; `Esc` -> Normal.
//!   - `Normal`  — `h j k l w b e 0 $ gg G x D Y p P i a I A o O d{motion} y{motion}`.
//!   - `Operator` — pending after `d`/`y`, awaiting a motion.
//!
//! Reference implementation: codex's `bottom_pane/textarea.rs`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Insert,
    Normal,
    Operator(Operator),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Delete,
    Yank,
}

pub struct Composer {
    pub buffer: String,
    pub cursor: usize,
    pub vim_mode: VimMode,
    pub vim_enabled: bool,
}

impl Composer {
    pub fn new(vim_enabled: bool) -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            vim_mode: if vim_enabled {
                VimMode::Normal
            } else {
                VimMode::Insert
            },
            vim_enabled,
        }
    }
}
