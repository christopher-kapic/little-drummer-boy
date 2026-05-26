#![allow(dead_code)]
//! Single-line text input for dialog fields.
//!
//! Not vim-mode aware — dialogs aren't where you live-edit prose. Handles
//! the bread-and-butter cases: char insert, backspace, delete-forward,
//! arrow keys, home/end. Wider character sets (CJK, emoji) are stored
//! by byte position; the cursor moves by char boundary.

use crossterm::event::{KeyCode, KeyEvent};

#[derive(Debug, Clone, Default)]
pub struct TextField {
    buffer: String,
    cursor: usize,
}

impl TextField {
    pub fn new(initial: impl Into<String>) -> Self {
        let buffer = initial.into();
        let cursor = buffer.len();
        Self { buffer, cursor }
    }

    pub fn text(&self) -> &str {
        &self.buffer
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set(&mut self, value: impl Into<String>) {
        self.buffer = value.into();
        self.cursor = self.buffer.len();
    }

    /// Apply a key event; returns true if the event was consumed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(ch) => {
                self.buffer.insert(self.cursor, ch);
                self.cursor += ch.len_utf8();
                true
            }
            KeyCode::Backspace => {
                if self.cursor == 0 {
                    return true;
                }
                let prev = self.buffer[..self.cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.buffer.drain(prev..self.cursor);
                self.cursor = prev;
                true
            }
            KeyCode::Delete => {
                if self.cursor >= self.buffer.len() {
                    return true;
                }
                let next_len = self.buffer[self.cursor..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(0);
                self.buffer.drain(self.cursor..self.cursor + next_len);
                true
            }
            KeyCode::Left => {
                if let Some((i, _)) = self.buffer[..self.cursor].char_indices().last() {
                    self.cursor = i;
                }
                true
            }
            KeyCode::Right => {
                if let Some(ch) = self.buffer[self.cursor..].chars().next() {
                    self.cursor += ch.len_utf8();
                }
                true
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.buffer.len();
                true
            }
            _ => false,
        }
    }

    /// Char column (not byte). For cursor placement only.
    pub fn cursor_col(&self) -> usize {
        self.buffer[..self.cursor].chars().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn insert_chars_and_backspace() {
        let mut tf = TextField::default();
        tf.handle_key(key(KeyCode::Char('a')));
        tf.handle_key(key(KeyCode::Char('b')));
        tf.handle_key(key(KeyCode::Char('c')));
        assert_eq!(tf.text(), "abc");
        assert_eq!(tf.cursor_col(), 3);
        tf.handle_key(key(KeyCode::Backspace));
        assert_eq!(tf.text(), "ab");
    }

    #[test]
    fn arrows_move_by_char_boundary() {
        let mut tf = TextField::new("héllo");
        assert_eq!(tf.cursor_col(), 5);
        tf.handle_key(key(KeyCode::Home));
        assert_eq!(tf.cursor_col(), 0);
        tf.handle_key(key(KeyCode::Right));
        tf.handle_key(key(KeyCode::Right));
        assert_eq!(tf.cursor_col(), 2);
    }

    #[test]
    fn delete_removes_char_forward() {
        let mut tf = TextField::new("abc");
        tf.handle_key(key(KeyCode::Home));
        tf.handle_key(key(KeyCode::Delete));
        assert_eq!(tf.text(), "bc");
    }
}
