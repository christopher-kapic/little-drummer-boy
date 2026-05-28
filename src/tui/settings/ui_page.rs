//! `/settings → UI` and the `Instructions File` sub-page reached from it.
//!
//! UI page: vim mode, thinking display, markdown rendering toggles,
//! mouse capture, rich-text copy, name, packages dir. The
//! "instructions file" row at the bottom drills into the
//! [`InstructionsPage`] grab/reorder editor for
//! `extended.agent_guidance_files`.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::config::extended::{ThinkingDisplay, VimModeSetting};
use crate::tui::textfield::TextField;
use crate::tui::theme::MUTED_COLOR_INDEX;

use super::{Nav, Page, SettingsDialog, save_status};

/// `/settings → UI` state.
pub(crate) struct UiPage {
    pub(super) cursor: usize,
    /// `Some(field)` when the user is inline-editing a text field.
    pub(super) editing: Option<UiField>,
    pub(super) buf: TextField,
    pub(super) status: Option<String>,
    /// Last value the user toggled the `mouse` setting to. The App
    /// reads this on dialog close to decide whether to push or pop
    /// crossterm's `EnableMouseCapture`. None = user didn't touch it.
    pub(crate) pending_mouse_capture: Option<bool>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) enum UiField {
    Name,
    PackagesDir,
}

/// `/settings → UI → Instructions File` state. Edits the
/// `extended.agent_guidance_files` list.
pub(super) struct InstructionsPage {
    pub(super) cursor: usize,
    /// When `Some(g)`, the user is holding the row currently at
    /// `cursor`. While grabbed they may rename it (typing goes to
    /// `g.buf`) and reorder it (↑/↓ swaps with the adjacent row —
    /// only arrows; j/k stay free so the user can type those letters
    /// into the filename). Enter commits and drops; Esc reverts the
    /// filename, swaps the row back to `g.origin`, and drops.
    pub(super) grabbed: Option<GrabState>,
    pub(super) status: Option<String>,
}

/// Per-row state while a row is grabbed.
pub(super) struct GrabState {
    /// Live text buffer for the grabbed row's filename.
    pub(super) buf: TextField,
    /// Index the row had when grabbed, restored on Esc.
    pub(super) origin: usize,
    /// Original filename. `Some` for rows that already existed
    /// (Esc restores the name); `None` for rows freshly created by
    /// `a` or Enter-on-`[+ add]` (Esc deletes them).
    pub(super) original_name: Option<String>,
}

/// Rows on the UI page (vim mode, thinking, render-agent-markdown,
/// render-user-markdown, mouse, rich-text-copy, emojis, name, packages
/// dir, instructions file).
pub(super) const UI_ROWS: usize = 10;

pub(super) fn bool_label(on: bool, on_label: &str, off_label: &str) -> String {
    if on {
        on_label.to_string()
    } else {
        off_label.to_string()
    }
}

fn cycle_vim(v: VimModeSetting) -> VimModeSetting {
    match v {
        VimModeSetting::Hint => VimModeSetting::Enabled,
        VimModeSetting::Enabled => VimModeSetting::Disabled,
        VimModeSetting::Disabled => VimModeSetting::Hint,
    }
}

pub(super) fn vim_label(v: VimModeSetting) -> &'static str {
    match v {
        VimModeSetting::Hint => "hint (default — vim on, hint chip on Normal entry)",
        VimModeSetting::Enabled => "enabled (vim on, no hint chip)",
        VimModeSetting::Disabled => "disabled (vim off)",
    }
}

fn cycle_thinking(t: ThinkingDisplay) -> ThinkingDisplay {
    match t {
        ThinkingDisplay::Condensed => ThinkingDisplay::Hidden,
        ThinkingDisplay::Hidden => ThinkingDisplay::Verbose,
        ThinkingDisplay::Verbose => ThinkingDisplay::Condensed,
    }
}

pub(super) fn thinking_label(t: ThinkingDisplay) -> &'static str {
    match t {
        ThinkingDisplay::Condensed => "condensed (default — chip, ctrl+j expands every block)",
        ThinkingDisplay::Hidden => "hidden (only `Thinking…` while in flight; nothing after)",
        ThinkingDisplay::Verbose => "verbose (always show reasoning inline)",
    }
}

impl SettingsDialog {
    pub(super) fn handle_ui_key(&mut self, key: KeyEvent) -> bool {
        // Detach + swap pattern (same rationale as handle_providers_key).
        // The inner handler must return navigation intent via `Nav`
        // instead of writing `self.page` directly — otherwise the
        // swap-back below would discard the write.
        let placeholder = Page::Ui(UiPage {
            cursor: 0,
            editing: None,
            buf: TextField::default(),
            status: None,
            pending_mouse_capture: None,
        });
        let mut page = std::mem::replace(&mut self.page, placeholder);
        let nav = if let Page::Ui(p) = &mut page {
            self.handle_ui_page_key(key, p)
        } else {
            Nav::Stay
        };
        match nav {
            Nav::Stay => {
                self.page = page;
                false
            }
            Nav::Replace(new) => {
                self.page = new;
                false
            }
            Nav::Close => true,
        }
    }

    fn handle_ui_page_key(&mut self, key: KeyEvent, p: &mut UiPage) -> Nav {
        if let Some(field) = p.editing {
            match key.code {
                KeyCode::Enter => {
                    let new = p.buf.text().trim().to_string();
                    match field {
                        UiField::Name => {
                            self.extended.name = if new.is_empty() { None } else { Some(new) };
                        }
                        UiField::PackagesDir => {
                            self.extended.packages_directory = if new.is_empty() {
                                None
                            } else {
                                Some(PathBuf::from(new))
                            };
                        }
                    }
                    p.editing = None;
                    p.status = match self.save_extended() {
                        Ok(()) => Some("saved".into()),
                        Err(e) => Some(format!("save failed: {e}")),
                    };
                }
                KeyCode::Esc => {
                    p.editing = None;
                    p.status = None;
                }
                _ => {
                    p.buf.handle_key(key);
                }
            }
            return Nav::Stay;
        }

        let rows = UI_ROWS;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Nav::Close,
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                return Nav::Replace(Page::Root {
                    cursor: self.last_root_cursor,
                });
            }
            KeyCode::Up | KeyCode::Char('k') => {
                p.cursor = p.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                p.cursor = (p.cursor + 1).min(rows - 1);
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match p.cursor {
                0 => {
                    self.extended.tui.vim_mode = cycle_vim(self.extended.tui.vim_mode);
                    p.status = save_status(self.save_extended());
                }
                1 => {
                    self.extended.tui.thinking = cycle_thinking(self.extended.tui.thinking);
                    p.status = save_status(self.save_extended());
                }
                2 => {
                    self.extended.tui.render_agent_markdown =
                        !self.extended.tui.render_agent_markdown;
                    p.status = save_status(self.save_extended());
                }
                3 => {
                    self.extended.tui.render_user_markdown =
                        !self.extended.tui.render_user_markdown;
                    p.status = save_status(self.save_extended());
                }
                4 => {
                    self.extended.tui.mouse_capture = !self.extended.tui.mouse_capture;
                    p.pending_mouse_capture = Some(self.extended.tui.mouse_capture);
                    p.status = save_status(self.save_extended());
                }
                5 => {
                    self.extended.tui.rich_text_copy = !self.extended.tui.rich_text_copy;
                    p.status = save_status(self.save_extended());
                }
                6 => {
                    self.extended.tui.use_emojis = !self.extended.tui.use_emojis;
                    p.status = save_status(self.save_extended());
                }
                7 => {
                    p.buf = TextField::new(self.extended.name.clone().unwrap_or_default());
                    p.editing = Some(UiField::Name);
                }
                8 => {
                    let cur = self
                        .extended
                        .packages_directory
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    p.buf = TextField::new(cur);
                    p.editing = Some(UiField::PackagesDir);
                }
                9 => {
                    return Nav::Replace(Page::Instructions(InstructionsPage {
                        cursor: 0,
                        grabbed: None,
                        status: None,
                    }));
                }
                _ => {}
            },
            _ => {}
        }
        Nav::Stay
    }

    pub(super) fn render_ui_page(&self, frame: &mut Frame, area: Rect, p: &UiPage) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let mut lines: Vec<Line<'static>> = Vec::new();

        lines.push(Line::from(Span::styled(
            "User-interface preferences".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        let rows: [(&str, String); 10] = [
            (
                "vim mode",
                vim_label(self.extended.tui.vim_mode).to_string(),
            ),
            (
                "thinking",
                thinking_label(self.extended.tui.thinking).to_string(),
            ),
            (
                "render agent markdown",
                bool_label(
                    self.extended.tui.render_agent_markdown,
                    "on (default)",
                    "off",
                ),
            ),
            (
                "render user markdown",
                bool_label(
                    self.extended.tui.render_user_markdown,
                    "on",
                    "off (default)",
                ),
            ),
            (
                "mouse",
                bool_label(
                    self.extended.tui.mouse_capture,
                    "on (default — click + drag-select; hold Shift/Option for native select)",
                    "off (native terminal select + copy)",
                ),
            ),
            (
                "rich-text copy",
                bool_label(
                    self.extended.tui.rich_text_copy,
                    "on (default — Ctrl+Shift+Y copies last agent message as rich text)",
                    "off (Ctrl+Shift+Y disabled)",
                ),
            ),
            (
                "emojis",
                bool_label(
                    self.extended.tui.use_emojis,
                    "enabled (emoji glyphs in tool calls + splash)",
                    "disabled (default — text-only; safe for terminals without emoji)",
                ),
            ),
            (
                "name",
                self.extended
                    .name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "(unset)".to_string()),
            ),
            (
                "packages dir",
                self.extended
                    .packages_directory
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(unset)".to_string()),
            ),
            (
                "instructions file",
                if self.extended.agent_guidance_files.is_empty() {
                    "(none)".to_string()
                } else {
                    self.extended.agent_guidance_files.join(", ")
                },
            ),
        ];

        let label_w = rows
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0);

        for (i, (label, value)) in rows.iter().enumerate() {
            let marker = if i == p.cursor { "▸ " } else { "  " };
            let label_style = if i == p.cursor {
                yellow.add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled(format!("{:<width$}", label, width = label_w), label_style),
                Span::raw("  "),
                Span::styled(value.clone(), muted),
            ]));
        }

        if let Some(field) = p.editing {
            let prompt = match field {
                UiField::Name => "name: ",
                UiField::PackagesDir => "packages dir: ",
            };
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled(prompt.to_string(), muted),
                Span::styled(p.buf.text().to_string(), Style::default().fg(Color::White)),
                Span::styled("▎".to_string(), Style::default().fg(Color::Yellow)),
            ]));
        }

        if let Some(status) = &p.status {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(status.clone(), yellow)));
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    // ── Instructions sub-page ────────────────────────────────────────────

    pub(super) fn handle_instructions_key(&mut self, key: KeyEvent) -> bool {
        let placeholder = Page::Instructions(InstructionsPage {
            cursor: 0,
            grabbed: None,
            status: None,
        });
        let mut page = std::mem::replace(&mut self.page, placeholder);
        let nav = if let Page::Instructions(p) = &mut page {
            self.handle_instructions_page_key(key, p)
        } else {
            Nav::Stay
        };
        match nav {
            Nav::Stay => {
                self.page = page;
                false
            }
            Nav::Replace(new) => {
                self.page = new;
                false
            }
            Nav::Close => true,
        }
    }

    fn handle_instructions_page_key(&mut self, key: KeyEvent, p: &mut InstructionsPage) -> Nav {
        // ── Grab mode ───────────────────────────────────────────────
        // The user is holding a row: typing edits its filename, arrow
        // keys (only arrows — j/k stay free for typing into the
        // filename) swap it with the neighbor, Enter commits, Esc
        // reverts (both name and position).
        if p.grabbed.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.commit_instructions_grab(p);
                }
                KeyCode::Esc => {
                    self.cancel_instructions_grab(p);
                }
                KeyCode::Up if p.cursor > 0 => {
                    self.extended
                        .agent_guidance_files
                        .swap(p.cursor, p.cursor - 1);
                    p.cursor -= 1;
                }
                KeyCode::Down if p.cursor + 1 < self.extended.agent_guidance_files.len() => {
                    self.extended
                        .agent_guidance_files
                        .swap(p.cursor, p.cursor + 1);
                    p.cursor += 1;
                }
                _ => {
                    if let Some(g) = p.grabbed.as_mut() {
                        g.buf.handle_key(key);
                    }
                }
            }
            return Nav::Stay;
        }

        let rows = self.extended.agent_guidance_files.len();
        // Max cursor = rows (the `[+ add]` synthetic row at the bottom).
        let max_cursor = rows;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Nav::Close,
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                return Nav::Replace(Page::Ui(UiPage {
                    cursor: 9,
                    editing: None,
                    buf: TextField::default(),
                    status: None,
                    pending_mouse_capture: None,
                }));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                p.cursor = p.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                p.cursor = (p.cursor + 1).min(max_cursor);
            }
            KeyCode::Char('a') => {
                self.start_instructions_grab_on_new(p);
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if p.cursor < self.extended.agent_guidance_files.len() {
                    self.extended.agent_guidance_files.remove(p.cursor);
                    let total = self.extended.agent_guidance_files.len();
                    p.cursor = p.cursor.min(total.saturating_sub(1).max(0));
                    p.status = save_status(self.save_extended());
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if p.cursor < self.extended.agent_guidance_files.len() {
                    let cur = self.extended.agent_guidance_files[p.cursor].clone();
                    p.grabbed = Some(GrabState {
                        buf: TextField::new(cur.clone()),
                        origin: p.cursor,
                        original_name: Some(cur),
                    });
                    p.status = None;
                } else if p.cursor == rows {
                    self.start_instructions_grab_on_new(p);
                }
            }
            _ => {}
        }
        Nav::Stay
    }

    /// Append an empty row, move the cursor to it, and grab it for
    /// rename + reorder. Used by `a` and by Enter on `[+ add]`.
    fn start_instructions_grab_on_new(&mut self, p: &mut InstructionsPage) {
        self.extended.agent_guidance_files.push(String::new());
        let idx = self.extended.agent_guidance_files.len() - 1;
        p.cursor = idx;
        p.grabbed = Some(GrabState {
            buf: TextField::default(),
            origin: idx,
            original_name: None,
        });
        p.status = None;
    }

    /// Drop the grabbed row, writing its buffer back to the list.
    /// An empty trimmed filename deletes the row instead.
    fn commit_instructions_grab(&mut self, p: &mut InstructionsPage) {
        let Some(g) = p.grabbed.take() else { return };
        let trimmed = g.buf.text().trim().to_string();
        if trimmed.is_empty() {
            if p.cursor < self.extended.agent_guidance_files.len() {
                self.extended.agent_guidance_files.remove(p.cursor);
            }
        } else if let Some(slot) = self.extended.agent_guidance_files.get_mut(p.cursor) {
            *slot = trimmed;
        }
        let total = self.extended.agent_guidance_files.len();
        if total == 0 {
            p.cursor = 0;
        } else {
            p.cursor = p.cursor.min(total - 1);
        }
        p.status = save_status(self.save_extended());
    }

    /// Drop the grabbed row without saving: restore its original
    /// position and (for previously-existing rows) its original name.
    /// A row created in this grab is removed.
    fn cancel_instructions_grab(&mut self, p: &mut InstructionsPage) {
        let Some(g) = p.grabbed.take() else { return };
        match g.original_name {
            Some(name) => {
                if let Some(slot) = self.extended.agent_guidance_files.get_mut(p.cursor) {
                    *slot = name;
                }
                let target = g
                    .origin
                    .min(self.extended.agent_guidance_files.len().saturating_sub(1));
                while p.cursor > target {
                    self.extended
                        .agent_guidance_files
                        .swap(p.cursor, p.cursor - 1);
                    p.cursor -= 1;
                }
                while p.cursor < target {
                    self.extended
                        .agent_guidance_files
                        .swap(p.cursor, p.cursor + 1);
                    p.cursor += 1;
                }
            }
            None => {
                if p.cursor < self.extended.agent_guidance_files.len() {
                    self.extended.agent_guidance_files.remove(p.cursor);
                }
                let total = self.extended.agent_guidance_files.len();
                if total == 0 {
                    p.cursor = 0;
                } else {
                    p.cursor = p.cursor.min(total - 1);
                }
            }
        }
        p.status = None;
    }

    pub(super) fn render_instructions_page(
        &self,
        frame: &mut Frame,
        area: Rect,
        p: &InstructionsPage,
    ) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let cyan = Style::default().fg(Color::Cyan);
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                "Instructions File".to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            Line::from(Span::styled(
                "Only the first matching file (in this order) is injected \
                 into prompts. Walks up from cwd to the git root."
                    .to_string(),
                muted,
            )),
            Line::default(),
        ];

        for (i, name) in self.extended.agent_guidance_files.iter().enumerate() {
            let is_grabbed = p.grabbed.is_some() && i == p.cursor;
            let on_cursor = i == p.cursor;
            // Marker shows grab state: ✥ when held, ▸ on the cursor,
            // blank otherwise.
            let marker = if is_grabbed {
                "✥ "
            } else if on_cursor {
                "▸ "
            } else {
                "  "
            };
            let display = if is_grabbed {
                p.grabbed.as_ref().unwrap().buf.text().to_string()
            } else {
                name.clone()
            };
            let style = if is_grabbed {
                cyan.add_modifier(Modifier::BOLD)
            } else if on_cursor {
                yellow.add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let mut spans = vec![Span::raw(marker), Span::styled(display, style)];
            if is_grabbed {
                // Inline cursor caret + an inline hint if the buffer
                // is still empty (freshly-added row).
                spans.push(Span::styled("▎".to_string(), cyan));
                if p.grabbed.as_ref().unwrap().buf.text().is_empty() {
                    spans.push(Span::styled("  (type filename)".to_string(), muted));
                }
            }
            lines.push(Line::from(spans));
        }

        // The `[+ add filename]` row is hidden while a row is held —
        // the user is already on the grabbed row's text input.
        if p.grabbed.is_none() {
            let add_idx = self.extended.agent_guidance_files.len();
            let add_selected = p.cursor == add_idx;
            let marker = if add_selected { "▸ " } else { "  " };
            let style = if add_selected {
                yellow.add_modifier(Modifier::BOLD)
            } else {
                muted
            };
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled("[+ add filename]".to_string(), style),
            ]));
        }

        if let Some(status) = &p.status {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(status.clone(), yellow)));
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}
