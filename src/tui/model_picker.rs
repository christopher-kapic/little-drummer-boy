#![allow(dead_code)]
//! `/model` picker dialog.
//!
//! Opens over the chat surface. Lists every model across every
//! configured provider as `provider/model-id`, with favorites pinned
//! at the top. The user can filter by typing; arrow keys move; Enter
//! selects.
//!
//! If the chosen model carries `thinking_modes`, a follow-up "level"
//! picker appears so the user can pick `off` / `low` / `medium` /
//! `high`. The result is written to `active_model` in config.json.
//!
//! The dialog is independent of `tui/settings.rs` to keep that file's
//! state machine focused on settings editing.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::config::dirs::discover_config_dirs;
use crate::config::providers::{ActiveModelRef, ConfigDoc, ProvidersConfig, ThinkingMode};
use crate::tui::textfield::TextField;
use crate::tui::theme::MUTED_COLOR_INDEX;

pub const DIALOG_HEIGHT: u16 = 18;

/// Visible model rows in the pick step. The dialog reserves the rest of
/// its height for the border, filter line, section headers, and help
/// line. Drives the scroll window (same scrolloff=1 behavior as the
/// composer `@`-popup).
const MODEL_WINDOW: usize = 11;

pub struct ModelPickerDialog {
    config_path: PathBuf,
    cfg: ProvidersConfig,
    entries: Vec<Entry>,
    filter: TextField,
    cursor: usize,
    /// Top visible index of the scroll window over the filtered list.
    scroll: usize,
    step: Step,
    error: Option<String>,
    done: bool,
}

#[derive(Clone)]
struct Entry {
    provider_id: String,
    model_id: String,
    display_name: Option<String>,
    is_favorite: bool,
    thinking_modes: Vec<ThinkingMode>,
}

impl Entry {
    fn label(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }

    fn matches(&self, q: &str) -> bool {
        let q = q.trim().to_ascii_lowercase();
        if q.is_empty() {
            return true;
        }
        let label = self.label().to_ascii_lowercase();
        if label.contains(&q) {
            return true;
        }
        self.display_name
            .as_deref()
            .map(|n| n.to_ascii_lowercase().contains(&q))
            .unwrap_or(false)
    }
}

enum Step {
    /// Picking the model.
    Pick,
    /// Model picked; choose a thinking mode.
    ChooseThinking {
        provider_id: String,
        model_id: String,
        modes: Vec<ThinkingMode>,
        cursor: usize,
    },
}

impl ModelPickerDialog {
    /// Try to open the picker for the given cwd. Returns `Err` if no
    /// config is reachable; callers should show the message inline.
    pub fn open(cwd: &Path) -> Result<Self, String> {
        let dirs = discover_config_dirs(cwd);
        let dir = dirs
            .first()
            .ok_or_else(|| "no cockpit config found — run /settings to create one".to_string())?;
        let config_path = dir.path.join("config.json");
        let doc = ConfigDoc::load(&config_path).map_err(|e| e.to_string())?;
        let cfg = doc.providers();

        let mut entries: Vec<Entry> = Vec::new();
        for (pid, entry) in &cfg.providers {
            for model in &entry.models {
                entries.push(Entry {
                    provider_id: pid.clone(),
                    model_id: model.id.clone(),
                    display_name: model.name.clone(),
                    is_favorite: model.favorite,
                    thinking_modes: model.thinking_modes.clone(),
                });
            }
        }
        // Stable order: favorites first (by label), then non-favorites
        // (by label).
        entries.sort_by(|a, b| {
            b.is_favorite
                .cmp(&a.is_favorite)
                .then_with(|| a.label().cmp(&b.label()))
        });

        Ok(Self {
            config_path,
            cfg,
            entries,
            filter: TextField::default(),
            cursor: 0,
            scroll: 0,
            step: Step::Pick,
            error: None,
            done: false,
        })
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    fn filtered_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.matches(self.filter.text()))
            .map(|(i, _)| i)
            .collect()
    }

    /// Returns true if the dialog should close.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if matches!(key.code, KeyCode::Esc) {
            return true;
        }
        match &mut self.step {
            Step::Pick => self.handle_pick_key(key),
            Step::ChooseThinking { .. } => self.handle_thinking_key(key),
        }
    }

    fn handle_pick_key(&mut self, key: KeyEvent) -> bool {
        let visible = self.filtered_indices();
        let max = visible.len().saturating_sub(1);
        match key.code {
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                self.scroll = crate::tui::app::windowed_scroll(
                    self.cursor,
                    self.scroll,
                    visible.len(),
                    MODEL_WINDOW,
                );
            }
            KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(max);
                self.scroll = crate::tui::app::windowed_scroll(
                    self.cursor,
                    self.scroll,
                    visible.len(),
                    MODEL_WINDOW,
                );
            }
            KeyCode::Enter => {
                if let Some(&i) = visible.get(self.cursor) {
                    let entry = self.entries[i].clone();
                    if entry.thinking_modes.is_empty() {
                        self.commit_active_model(entry.provider_id, entry.model_id, None);
                        return true;
                    } else {
                        let modes = entry.thinking_modes.clone();
                        self.step = Step::ChooseThinking {
                            provider_id: entry.provider_id,
                            model_id: entry.model_id,
                            modes,
                            cursor: 0,
                        };
                    }
                }
            }
            _ => {
                // Typing filters the list. Reset the cursor when the
                // visible set changes to avoid pointing past the end.
                let before = self.filter.text().to_string();
                self.filter.handle_key(key);
                if before != self.filter.text() {
                    self.cursor = 0;
                    self.scroll = 0;
                }
            }
        }
        false
    }

    fn handle_thinking_key(&mut self, key: KeyEvent) -> bool {
        let (provider_id, model_id, modes, cursor) = match &mut self.step {
            Step::ChooseThinking {
                provider_id,
                model_id,
                modes,
                cursor,
            } => (provider_id, model_id, modes, cursor),
            _ => return false,
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *cursor = cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *cursor = (*cursor + 1).min(modes.len().saturating_sub(1));
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                self.step = Step::Pick;
            }
            KeyCode::Enter => {
                let mode = modes.get(*cursor).copied();
                let p = provider_id.clone();
                let m = model_id.clone();
                self.commit_active_model(p, m, mode);
                return true;
            }
            _ => {}
        }
        false
    }

    fn commit_active_model(
        &mut self,
        provider_id: String,
        model_id: String,
        thinking_mode: Option<ThinkingMode>,
    ) {
        self.cfg.active_model = Some(ActiveModelRef {
            provider: provider_id,
            model: model_id,
            thinking_mode,
        });
        if let Err(e) = self.save() {
            self.error = Some(format!("save failed: {e}"));
        }
        self.done = true;
    }

    fn save(&mut self) -> Result<(), String> {
        let mut doc = ConfigDoc::load(&self.config_path).map_err(|e| e.to_string())?;
        doc.write(&self.cfg).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" /model — pick the active model ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
        match &self.step {
            Step::Pick => self.render_pick(frame, layout[0]),
            Step::ChooseThinking { .. } => self.render_thinking(frame, layout[0]),
        }
        let help = match &self.step {
            Step::Pick => "type to filter  ↑/↓  enter: pick  esc: cancel",
            Step::ChooseThinking { .. } => "↑/↓  enter: confirm  ←: back  esc: cancel",
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                help.to_string(),
                Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX)),
            ))),
            layout[1],
        );
    }

    fn render_pick(&self, frame: &mut Frame, area: Rect) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Indexed(178));
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("filter: ".to_string(), muted),
            Span::styled(
                self.filter.text().to_string(),
                Style::default().fg(Color::White),
            ),
            Span::styled("▎".to_string(), Style::default().fg(Color::Yellow)),
        ]));
        lines.push(Line::default());

        let visible = self.filtered_indices();
        if visible.is_empty() {
            let body = if self.entries.is_empty() {
                "(no models — run /fetch-models or add a provider via /settings)"
            } else {
                "(no matches — try a different filter)"
            };
            lines.push(Line::from(Span::styled(body.to_string(), muted)));
        } else {
            let mut seen_fav = false;
            let mut seen_other = false;
            // Scroll window: same scrolloff=1 behavior as the @-popup.
            let offset = crate::tui::app::windowed_scroll(
                self.cursor,
                self.scroll,
                visible.len(),
                MODEL_WINDOW,
            );
            for (i, &idx) in visible.iter().enumerate().skip(offset).take(MODEL_WINDOW) {
                let e = &self.entries[idx];
                if e.is_favorite && !seen_fav {
                    lines.push(Line::from(Span::styled(
                        "favorites".to_string(),
                        muted.add_modifier(Modifier::ITALIC),
                    )));
                    seen_fav = true;
                }
                if !e.is_favorite && !seen_other {
                    lines.push(Line::from(Span::styled(
                        "all models".to_string(),
                        muted.add_modifier(Modifier::ITALIC),
                    )));
                    seen_other = true;
                }
                let active = i == self.cursor;
                let marker = if active { "▸ " } else { "  " };
                let label_style = if active {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if e.is_favorite {
                    yellow
                } else {
                    Style::default().fg(Color::White)
                };
                let mut spans = vec![
                    Span::raw(marker.to_string()),
                    Span::styled(e.label(), label_style),
                ];
                if let Some(name) = &e.display_name {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(name.clone(), muted));
                }
                if !e.thinking_modes.is_empty() {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(
                        format!("[thinking: {}]", thinking_summary(&e.thinking_modes)),
                        muted,
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
        if let Some(err) = &self.error {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(Color::Red),
            )));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn render_thinking(&self, frame: &mut Frame, area: Rect) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let (provider_id, model_id, modes, cursor) = match &self.step {
            Step::ChooseThinking {
                provider_id,
                model_id,
                modes,
                cursor,
            } => (provider_id, model_id, modes, cursor),
            _ => return,
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("model: ".to_string(), muted),
            Span::styled(
                format!("{provider_id}/{model_id}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Thinking mode:".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for (i, m) in modes.iter().enumerate() {
            let marker = if i == *cursor { "▸ " } else { "  " };
            let style = if i == *cursor {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(vec![
                Span::raw(marker.to_string()),
                Span::styled(thinking_label(*m), style),
            ]));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}

fn thinking_label(m: ThinkingMode) -> String {
    match m {
        ThinkingMode::Off => "off",
        ThinkingMode::Low => "low",
        ThinkingMode::Medium => "medium",
        ThinkingMode::High => "high",
    }
    .to_string()
}

fn thinking_summary(modes: &[ThinkingMode]) -> String {
    modes
        .iter()
        .copied()
        .map(thinking_label)
        .collect::<Vec<_>>()
        .join("/")
}

/// Toggle the favorite flag on the currently-active model, persisting
/// the change to `config.json`. Returns the new favorite state, or
/// `Err` if there's no active model or no config to write to.
pub fn toggle_active_favorite(cwd: &Path) -> Result<(bool, String, String), String> {
    let dirs = discover_config_dirs(cwd);
    let dir = dirs
        .first()
        .ok_or_else(|| "no cockpit config found".to_string())?;
    let config_path = dir.path.join("config.json");
    let mut doc = ConfigDoc::load(&config_path).map_err(|e| e.to_string())?;
    let mut cfg = doc.providers();
    let active = cfg
        .active_model
        .clone()
        .ok_or_else(|| "no active model — run /model first".to_string())?;
    let entry = cfg
        .providers
        .get_mut(&active.provider)
        .ok_or_else(|| format!("provider `{}` not in config", active.provider))?;
    let model = entry
        .models
        .iter_mut()
        .find(|m| m.id == active.model)
        .ok_or_else(|| {
            format!(
                "model `{}` not in provider `{}` — refetch /models first",
                active.model, active.provider
            )
        })?;
    model.favorite = !model.favorite;
    let new = model.favorite;
    let p = active.provider.clone();
    let m = active.model.clone();
    doc.write(&cfg).map_err(|e| e.to_string())?;
    Ok((new, p, m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn empty_dialog() -> ModelPickerDialog {
        // Build a dialog with no entries — exercises only key routing.
        ModelPickerDialog {
            config_path: PathBuf::from("/tmp/cockpit.test"),
            cfg: ProvidersConfig::default(),
            entries: Vec::new(),
            filter: TextField::default(),
            cursor: 0,
            scroll: 0,
            step: Step::Pick,
            error: None,
            done: false,
        }
    }

    /// Typing while the picker is open must not bubble out. The picker
    /// returns `false` from `handle_key` (don't close) but App must
    /// still swallow the key so it never reaches the composer.
    #[test]
    fn typing_a_filter_char_does_not_request_close() {
        let mut d = empty_dialog();
        // `j` was the original repro: in handle_pick_key it lands in
        // the `_` arm and feeds the filter. The return value tells App
        // "stay open"; App is responsible for not propagating the key.
        assert!(!d.handle_key(press(KeyCode::Char('j'))));
        assert_eq!(d.filter.text(), "j");
        assert!(!d.handle_key(press(KeyCode::Char('k'))));
        assert_eq!(d.filter.text(), "jk");
    }

    #[test]
    fn esc_signals_close() {
        let mut d = empty_dialog();
        assert!(d.handle_key(press(KeyCode::Esc)));
    }
}
