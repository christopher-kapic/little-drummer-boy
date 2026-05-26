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

/// Prompt glyph shown at the start of the composer input line and in the
/// submitted-history echo.
pub const INPUT_PREFIX: &str = "❯ ";

/// Display width of [`INPUT_PREFIX`] in terminal columns. Computed via
/// `unicode-width` so wider glyphs (CJK, emoji) would size correctly if
/// the prefix is ever changed.
pub fn input_prefix_width() -> usize {
    use unicode_width::UnicodeWidthStr;
    INPUT_PREFIX.width()
}

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
    buffer: String,
    cursor: usize,
    vim_mode: VimMode,
    vim_enabled: bool,
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

    pub fn text(&self) -> &str {
        &self.buffer
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn vim_mode(&self) -> VimMode {
        self.vim_mode
    }

    pub fn set_vim_mode(&mut self, mode: VimMode) {
        self.vim_mode = mode;
    }

    pub fn vim_enabled(&self) -> bool {
        self.vim_enabled
    }

    /// Reset to empty + cursor at start. Used after submit and on `Esc`
    /// while a slash command is being composed.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
    }

    pub fn insert_char(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn delete_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.buffer[..self.cursor]
            .char_indices()
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.buffer.drain(previous..self.cursor);
        self.cursor = previous;
    }

    pub fn delete_right(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let next_len = self.buffer[self.cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
        self.buffer.drain(self.cursor..self.cursor + next_len);
    }

    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.buffer[..self.cursor]
            .char_indices()
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
    }

    pub fn move_right(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        if let Some(next) = self.buffer[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    pub fn move_up(&mut self) {
        let before = &self.buffer[..self.cursor];
        let Some(prev_nl) = before.rfind('\n') else {
            return;
        };
        let curr_line_start = prev_nl + 1;
        let col = before[curr_line_start..].chars().count();
        let prev_line_end = prev_nl;
        let prev_line_start = self.buffer[..prev_line_end]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prev_line = &self.buffer[prev_line_start..prev_line_end];
        let target_chars = col.min(prev_line.chars().count());
        let target_byte = prev_line
            .char_indices()
            .nth(target_chars)
            .map(|(i, _)| i)
            .unwrap_or(prev_line.len());
        self.cursor = prev_line_start + target_byte;
    }

    pub fn move_down(&mut self) {
        let buf = &self.buffer;
        let cursor = self.cursor;
        let line_start = buf[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = buf[line_start..cursor].chars().count();
        let Some(rel_nl) = buf[cursor..].find('\n') else {
            return;
        };
        let next_line_start = cursor + rel_nl + 1;
        let next_line_end = buf[next_line_start..]
            .find('\n')
            .map(|i| next_line_start + i)
            .unwrap_or(buf.len());
        let next_line = &buf[next_line_start..next_line_end];
        let target_chars = col.min(next_line.chars().count());
        let target_byte = next_line
            .char_indices()
            .nth(target_chars)
            .map(|(i, _)| i)
            .unwrap_or(next_line.len());
        self.cursor = next_line_start + target_byte;
    }

    pub fn move_line_start(&mut self) {
        let line_start = self.buffer[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = line_start;
    }

    pub fn move_line_end(&mut self) {
        let buf = &self.buffer;
        let line_end = buf[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(buf.len());
        self.cursor = line_end;
    }

    /// Newline count + 1 (or 1 when empty). Useful for sizing the input box.
    pub fn line_count(&self) -> usize {
        if self.buffer.is_empty() {
            1
        } else {
            self.buffer.split('\n').count()
        }
    }

    /// Cursor's (line, column) measured in characters. The column is a
    /// char count, not a display width — callers that care about wide
    /// glyphs must convert.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let before = &self.buffer[..self.cursor];
        let line = before.matches('\n').count();
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = before[line_start..].chars().count();
        (line, col)
    }
}
