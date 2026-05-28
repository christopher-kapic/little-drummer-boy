//! `/settings → Tools` page: built-in custom-tool templates
//! (`webfetch`, `websearch`) and their per-tool command + description
//! + enabled fields under `extended-config.tools`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::config::extended::ToolCommandTemplate;
use crate::tui::textfield::TextField;
use crate::tui::theme::MUTED_COLOR_INDEX;

use super::{Nav, Page, SettingsDialog, save_status};

/// `/settings → Tools` state. Edits the user-defined bash-command
/// templates under `extended-config.tools`.
pub(super) struct ToolsPage {
    pub(super) cursor: usize,
    pub(super) editing: Option<ToolField>,
    pub(super) buf: TextField,
    /// Which tool's row is being edited, when `editing` is `Some`.
    pub(super) edit_target: Option<String>,
    pub(super) status: Option<String>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) enum ToolField {
    Command,
    Description,
}

/// Built-in custom-tool names surfaced on the Tools page. These are
/// also registered as live tools by the agent runtime (see
/// `src/tools/custom.rs`).
pub fn builtin_tool_names() -> &'static [&'static str] {
    &["webfetch", "websearch"]
}

/// Default bash command + description for a built-in tool. The defaults
/// rely only on widely-available CLI utilities (curl, ddgr) so a user
/// can land a working tool without configuring anything.
pub fn default_template_for(name: &str) -> ToolCommandTemplate {
    match name {
        "webfetch" => ToolCommandTemplate {
            enabled: true,
            command:
                "curl -sSL --max-time 20 --max-filesize 2000000 --user-agent 'cockpit-cli' {url}"
                    .to_string(),
            description: Some(
                "Fetch a URL. Pass `url` (the target). Returns the response body.".to_string(),
            ),
        },
        "websearch" => ToolCommandTemplate {
            enabled: true,
            command: "ddgr --json --num 8 -- {query}".to_string(),
            description: Some(
                "Search the web. Pass `query`. Returns JSON results from DuckDuckGo.".to_string(),
            ),
        },
        _ => ToolCommandTemplate {
            enabled: true,
            command: String::new(),
            description: None,
        },
    }
}

impl SettingsDialog {
    pub(super) fn handle_tools_key(&mut self, key: KeyEvent) -> bool {
        let placeholder = Page::Tools(ToolsPage {
            cursor: 0,
            editing: None,
            buf: TextField::default(),
            edit_target: None,
            status: None,
        });
        let mut page = std::mem::replace(&mut self.page, placeholder);
        let nav = if let Page::Tools(p) = &mut page {
            self.handle_tools_page_key(key, p)
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

    fn handle_tools_page_key(&mut self, key: KeyEvent, p: &mut ToolsPage) -> Nav {
        if let Some(field) = p.editing {
            match key.code {
                KeyCode::Enter => {
                    let new = p.buf.text().to_string();
                    if let Some(name) = p.edit_target.clone() {
                        let entry = self.extended.tools.entry(name).or_insert_with(|| {
                            ToolCommandTemplate {
                                enabled: true,
                                command: String::new(),
                                description: None,
                            }
                        });
                        match field {
                            ToolField::Command => entry.command = new,
                            ToolField::Description => {
                                entry.description = if new.is_empty() { None } else { Some(new) };
                            }
                        }
                    }
                    p.editing = None;
                    p.edit_target = None;
                    p.status = save_status(self.save_extended());
                }
                KeyCode::Esc => {
                    p.editing = None;
                    p.edit_target = None;
                }
                _ => {
                    p.buf.handle_key(key);
                }
            }
            return Nav::Stay;
        }

        // The tools page lays out a flat list:
        //   for each known tool: [command, description, enabled] (3 rows)
        // built-ins (webfetch, websearch) are always present; users can
        // also add their own under arbitrary names but we don't surface
        // an "add tool" affordance in v1 to keep the UI tight.
        let builtins = builtin_tool_names();
        let rows_per_tool = 3usize;
        let total_rows = builtins.len() * rows_per_tool;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Nav::Close,
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                return Nav::Replace(Page::Root {
                    cursor: self.last_root_cursor,
                });
            }
            KeyCode::Up | KeyCode::Char('k') => {
                p.cursor = crate::tui::nav::wrap_prev(p.cursor, total_rows);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                p.cursor = crate::tui::nav::wrap_next(p.cursor, total_rows);
            }
            KeyCode::Char('t') => {
                let tool_idx = p.cursor / rows_per_tool;
                if let Some(name) = builtins.get(tool_idx).copied() {
                    let entry = self
                        .extended
                        .tools
                        .entry(name.to_string())
                        .or_insert_with(|| default_template_for(name));
                    entry.enabled = !entry.enabled;
                    p.status = save_status(self.save_extended());
                }
            }
            KeyCode::Char('r') => {
                let tool_idx = p.cursor / rows_per_tool;
                if let Some(name) = builtins.get(tool_idx).copied() {
                    self.extended
                        .tools
                        .insert(name.to_string(), default_template_for(name));
                    p.status = save_status(self.save_extended());
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let tool_idx = p.cursor / rows_per_tool;
                let row_in_tool = p.cursor % rows_per_tool;
                if let Some(name) = builtins.get(tool_idx).copied() {
                    let entry = self
                        .extended
                        .tools
                        .entry(name.to_string())
                        .or_insert_with(|| default_template_for(name));
                    match row_in_tool {
                        0 => {
                            p.buf = TextField::new(entry.command.clone());
                            p.edit_target = Some(name.to_string());
                            p.editing = Some(ToolField::Command);
                        }
                        1 => {
                            p.buf = TextField::new(entry.description.clone().unwrap_or_default());
                            p.edit_target = Some(name.to_string());
                            p.editing = Some(ToolField::Description);
                        }
                        2 => {
                            entry.enabled = !entry.enabled;
                            p.status = save_status(self.save_extended());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Nav::Stay
    }

    pub(super) fn render_tools_page(&self, frame: &mut Frame, area: Rect, p: &ToolsPage) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let mut lines: Vec<Line<'static>> = Vec::new();

        lines.push(Line::from(Span::styled(
            "Custom bash-command tools".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        let builtins = builtin_tool_names();
        let mut row_idx = 0usize;
        for name in builtins.iter() {
            let entry = self.extended.tools.get(*name);
            let default = default_template_for(name);
            let cmd = entry
                .map(|e| e.command.as_str())
                .unwrap_or(&default.command);
            let desc = entry
                .and_then(|e| e.description.as_deref())
                .or(default.description.as_deref())
                .unwrap_or("");
            let enabled = entry.map(|e| e.enabled).unwrap_or(default.enabled);

            lines.push(Line::from(Span::styled(
                format!("[{name}]"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));

            let sub_rows: [(&str, String); 3] = [
                ("  command", cmd.to_string()),
                ("  description", desc.to_string()),
                (
                    "  enabled",
                    if enabled { "yes".into() } else { "no".into() },
                ),
            ];
            for (label, value) in &sub_rows {
                let marker = if row_idx == p.cursor { "▸ " } else { "  " };
                let label_style = if row_idx == p.cursor {
                    yellow.add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                lines.push(Line::from(vec![
                    Span::raw(marker),
                    Span::styled(format!("{:<14}", label), label_style),
                    Span::raw("  "),
                    Span::styled(value.clone(), muted),
                ]));
                row_idx += 1;
            }
            lines.push(Line::default());
        }

        if let Some(field) = p.editing {
            let prompt = match field {
                ToolField::Command => "command: ",
                ToolField::Description => "description: ",
            };
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
}
