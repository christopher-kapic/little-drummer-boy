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
    Change,
    Yank,
}

pub struct Composer {
    buffer: String,
    cursor: usize,
    vim_mode: VimMode,
    vim_enabled: bool,
    /// True if the previous Normal-mode key was a `g` — the *next* `g`
    /// completes the `gg` motion (jump to buffer start). Cleared on any
    /// other key. Lives here so app.rs can stay stateless about chord
    /// sequencing.
    pending_g: bool,
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
            pending_g: false,
        }
    }

    pub fn set_vim_enabled(&mut self, enabled: bool) {
        self.vim_enabled = enabled;
        if !enabled {
            self.vim_mode = VimMode::Insert;
            self.pending_g = false;
        }
    }

    pub fn pending_g(&self) -> bool {
        self.pending_g
    }

    pub fn set_pending_g(&mut self, on: bool) {
        self.pending_g = on;
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

    /// Move to the start of the buffer (vim `gg`).
    pub fn move_buffer_start(&mut self) {
        self.cursor = 0;
    }

    /// Move to the start of the *last* line of the buffer (vim `G`).
    pub fn move_buffer_end(&mut self) {
        // Land at the start of the final line — matches vim's `G` when
        // no count is given (it goes to the last line, not the last
        // char).
        if let Some(last_nl) = self.buffer.rfind('\n') {
            self.cursor = last_nl + 1;
        } else {
            self.cursor = 0;
        }
    }

    /// Vim word-forward (`w`/`W`). `big_word=true` for `W` — uses
    /// whitespace boundaries only; `big_word=false` for `w` — also
    /// stops at punctuation transitions.
    pub fn move_word_forward(&mut self, big_word: bool) {
        let bytes = self.buffer.as_bytes();
        let n = bytes.len();
        if self.cursor >= n {
            return;
        }
        let classify = |ch: char| -> u8 {
            if ch.is_whitespace() {
                0
            } else if big_word || ch.is_alphanumeric() || ch == '_' {
                1
            } else {
                2 // punctuation (only meaningful for `w`)
            }
        };
        let mut it = self.buffer[self.cursor..].char_indices().peekable();
        let start_class = it.peek().map(|(_, c)| classify(*c)).unwrap_or(0);
        // Step 1: walk past the current class.
        while let Some((_, c)) = it.peek().copied() {
            if classify(c) == start_class && start_class != 0 {
                it.next();
            } else {
                break;
            }
        }
        // Step 2: walk past any whitespace.
        while let Some((_, c)) = it.peek().copied() {
            if c.is_whitespace() {
                it.next();
            } else {
                break;
            }
        }
        if let Some((rel, _)) = it.peek().copied() {
            self.cursor += rel;
        } else {
            self.cursor = n;
        }
    }

    /// Delete from cursor to the position vim-`w`/`W` would land at.
    pub fn delete_word_forward(&mut self, big_word: bool) {
        let start = self.cursor;
        self.move_word_forward(big_word);
        let end = self.cursor;
        if end > start {
            self.buffer.drain(start..end);
            self.cursor = start;
        }
    }

    /// Delete from cursor back to the position vim-`b`/`B` would land at.
    pub fn delete_word_backward(&mut self, big_word: bool) {
        let end = self.cursor;
        self.move_word_backward(big_word);
        let start = self.cursor;
        if end > start {
            self.buffer.drain(start..end);
        }
    }

    /// `d$` — delete from cursor to end of current line.
    pub fn delete_to_line_end(&mut self) {
        let start = self.cursor;
        self.move_line_end();
        let end = self.cursor;
        if end > start {
            self.buffer.drain(start..end);
            self.cursor = start;
        }
    }

    /// `d0` — delete from cursor back to start of current line.
    pub fn delete_to_line_start(&mut self) {
        let end = self.cursor;
        self.move_line_start();
        let start = self.cursor;
        if end > start {
            self.buffer.drain(start..end);
        }
    }

    /// `dd` — delete the line under the cursor (including its trailing
    /// `\n`, so a subsequent paste behaves linewise).
    pub fn delete_current_line(&mut self) {
        let line_start = self.buffer[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let line_end = self.buffer[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i + 1)
            .unwrap_or(self.buffer.len());
        self.buffer.drain(line_start..line_end);
        // After deleting the last line of a buffer with no trailing
        // newline, line_start can land past the end — clamp.
        self.cursor = line_start.min(self.buffer.len());
        // Snap to start of the (now-)current line for vim parity.
        let line_start = self.buffer[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = line_start;
    }

    /// `dG` — delete from cursor to end of buffer.
    pub fn delete_to_buffer_end(&mut self) {
        self.buffer.truncate(self.cursor);
    }

    /// `dgg` — delete from start of buffer to cursor.
    pub fn delete_to_buffer_start(&mut self) {
        self.buffer.drain(0..self.cursor);
        self.cursor = 0;
    }

    /// `o` — open a new empty line below the current one and land at
    /// its start. Caller is responsible for switching to Insert mode.
    pub fn open_below(&mut self) {
        self.move_line_end();
        self.insert_char('\n');
    }

    /// `O` — open a new empty line above the current one and land on
    /// it. Caller is responsible for switching to Insert mode.
    pub fn open_above(&mut self) {
        self.move_line_start();
        self.insert_char('\n');
        // insert_char advanced the cursor past the new `\n`; step one
        // byte back so we land at the start of the empty line we just
        // opened. The `\n` is single-byte so byte-decrement is safe.
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Vim word-backward (`b`/`B`).
    pub fn move_word_backward(&mut self, big_word: bool) {
        if self.cursor == 0 {
            return;
        }
        let classify = |ch: char| -> u8 {
            if ch.is_whitespace() {
                0
            } else if big_word || ch.is_alphanumeric() || ch == '_' {
                1
            } else {
                2
            }
        };
        let before = &self.buffer[..self.cursor];
        let chars: Vec<(usize, char)> = before.char_indices().collect();
        let mut i = chars.len();
        // Step 1: skip whitespace immediately before the cursor.
        while i > 0 && chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        if i == 0 {
            self.cursor = 0;
            return;
        }
        // Step 2: while previous char is same class as char i-1, keep going.
        let target_class = classify(chars[i - 1].1);
        while i > 0 && classify(chars[i - 1].1) == target_class && target_class != 0 {
            i -= 1;
        }
        self.cursor = chars.get(i).map(|(b, _)| *b).unwrap_or(0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, cursor: usize) -> Composer {
        let mut c = Composer::new(true);
        for ch in text.chars() {
            c.insert_char(ch);
        }
        c.cursor = cursor;
        c
    }

    #[test]
    fn dw_deletes_word_and_trailing_space() {
        let mut c = at("hello world", 0);
        c.delete_word_forward(false);
        assert_eq!(c.text(), "world");
        assert_eq!(c.cursor, 0);
    }

    #[test]
    fn db_deletes_back_to_prev_word() {
        let mut c = at("hello world", 11);
        c.delete_word_backward(false);
        assert_eq!(c.text(), "hello ");
        assert_eq!(c.cursor, 6);
    }

    #[test]
    fn dd_deletes_full_line_and_its_newline() {
        let mut c = at("a\nb\nc", 2); // cursor on 'b'
        c.delete_current_line();
        assert_eq!(c.text(), "a\nc");
        // Cursor should land at start of the (now-)current line.
        let (line, col) = c.cursor_line_col();
        assert_eq!((line, col), (1, 0));
    }

    #[test]
    fn dd_on_only_line_clears_buffer() {
        let mut c = at("just one", 4);
        c.delete_current_line();
        assert_eq!(c.text(), "");
        assert_eq!(c.cursor, 0);
    }

    #[test]
    fn d_dollar_deletes_to_eol() {
        let mut c = at("hello world", 5); // cursor after "hello"
        c.delete_to_line_end();
        assert_eq!(c.text(), "hello");
        assert_eq!(c.cursor, 5);
    }

    #[test]
    fn d_zero_deletes_to_line_start() {
        let mut c = at("hello", 5);
        c.delete_to_line_start();
        assert_eq!(c.text(), "");
        assert_eq!(c.cursor, 0);
    }

    #[test]
    fn open_below_inserts_newline_and_lands_on_it() {
        let mut c = at("hello\nworld", 2); // mid-"hello"
        c.open_below();
        assert_eq!(c.text(), "hello\n\nworld");
        // Cursor on the new empty line.
        let (line, col) = c.cursor_line_col();
        assert_eq!((line, col), (1, 0));
    }

    #[test]
    fn open_above_inserts_newline_above_and_lands_on_it() {
        let mut c = at("hello\nworld", 6); // start of "world"
        c.open_above();
        assert_eq!(c.text(), "hello\n\nworld");
        // Cursor on the new empty middle line.
        let (line, col) = c.cursor_line_col();
        assert_eq!((line, col), (1, 0));
    }

    #[test]
    fn word_forward_stops_on_punctuation() {
        let mut c = at("foo.bar baz", 0);
        c.move_word_forward(false);
        // small-w lands on the punctuation transition.
        assert_eq!(c.cursor, 3);
    }

    #[test]
    fn big_word_forward_skips_punctuation() {
        let mut c = at("foo.bar baz", 0);
        c.move_word_forward(true);
        // big-W treats `foo.bar` as one WORD; lands on `baz`.
        assert_eq!(c.cursor, 8);
    }

    #[test]
    fn gg_jumps_to_buffer_start() {
        let mut c = at("a\nb\nc", 4);
        c.move_buffer_start();
        assert_eq!(c.cursor, 0);
    }

    #[test]
    fn capital_g_lands_on_last_line_start() {
        let mut c = at("a\nb\nccc", 0);
        c.move_buffer_end();
        // Start of "ccc".
        assert_eq!(c.cursor, 4);
    }
}
