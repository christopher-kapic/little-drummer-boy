//! `/settings` dialog state machine + rendering.
//!
//! Lifecycle:
//!   - `Dialog::None`                  no overlay; viewport renders normally
//!   - `Dialog::PickConfig`            choose an existing config to edit
//!   - `Dialog::CreateConfig`          no config yet — pick a location to scaffold
//!   - `Dialog::Settings`              navigate the settings tree
//!
//! Pages are modeled by a `path: Vec<String>` plus a per-level cursor
//! stack so we can add nested subpages later without changing the
//! structure. `nav_children` returns the children at a given path —
//! today the root has Providers/Agents/Tools as leaves.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::config::dirs::{
    ConfigDir, ConfigDirKind, creatable_config_dirs, discover_config_dirs, scaffold_config_dir,
};
use crate::tui::theme::MUTED_COLOR_INDEX;

/// Height (in rows) the dialog wants when active. Pane grows to this
/// on first open; grow-only policy means subsequent closes don't shrink.
pub const DIALOG_HEIGHT: u16 = 14;

pub enum Dialog {
    None,
    PickConfig {
        dirs: Vec<ConfigDir>,
        cursor: usize,
    },
    CreateConfig {
        choices: Vec<ConfigDir>,
        cursor: usize,
    },
    Settings {
        config_path: PathBuf,
        path: Vec<String>,
        cursors: Vec<usize>,
    },
}

impl Dialog {
    pub fn is_active(&self) -> bool {
        !matches!(self, Dialog::None)
    }

    /// Open the dialog: pick the right variant based on what config
    /// directories already exist for `cwd`.
    pub fn open(cwd: &std::path::Path) -> Self {
        let dirs = discover_config_dirs(cwd);
        if dirs.is_empty() {
            Dialog::CreateConfig {
                choices: creatable_config_dirs(),
                cursor: 0,
            }
        } else {
            Dialog::PickConfig { dirs, cursor: 0 }
        }
    }

    /// Handle a key while the dialog is active.
    ///
    /// Returns `true` if the dialog should close (caller assigns
    /// `Dialog::None`). On internal transitions (picker → settings,
    /// create → settings, page navigation) self is mutated in place and
    /// `false` is returned.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        match self {
            Dialog::None => false,
            Dialog::PickConfig { dirs, cursor } => match list_key_action(key, cursor, dirs.len()) {
                ListAction::Stay => false,
                ListAction::Close => true,
                ListAction::Select(idx) => {
                    let chosen = dirs[idx].path.join("config.json");
                    *self = Dialog::Settings {
                        config_path: chosen,
                        path: Vec::new(),
                        cursors: vec![0],
                    };
                    false
                }
            },
            Dialog::CreateConfig { choices, cursor } => {
                match list_key_action(key, cursor, choices.len()) {
                    ListAction::Stay => false,
                    ListAction::Close => true,
                    ListAction::Select(idx) => match scaffold_config_dir(&choices[idx].path) {
                        Ok(config_path) => {
                            *self = Dialog::Settings {
                                config_path,
                                path: Vec::new(),
                                cursors: vec![0],
                            };
                            false
                        }
                        // If the scaffold fails (permissions, etc.) just close —
                        // we don't have a way to surface errors in-dialog yet.
                        Err(_) => true,
                    },
                }
            }
            Dialog::Settings { path, cursors, .. } => {
                if matches!(key.code, KeyCode::Esc) {
                    return true;
                }
                let children = nav_children(path);
                let level = cursors.len().saturating_sub(1);

                // Leaf page (no children): only back / esc do anything.
                if children.is_empty() {
                    if matches!(
                        key.code,
                        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace
                    ) {
                        ascend(path, cursors);
                    }
                    return false;
                }

                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        cursors[level] = cursors[level].saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let last = children.len().saturating_sub(1);
                        cursors[level] = (cursors[level] + 1).min(last);
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        if let Some(child) = children.get(cursors[level]) {
                            path.push(child.title.to_string());
                            cursors.push(0);
                        }
                    }
                    KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                        ascend(path, cursors);
                    }
                    _ => {}
                }
                false
            }
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        match self {
            Dialog::None => {}
            Dialog::PickConfig { dirs, cursor } => {
                render_picker(frame, area, "pick a config to edit", dirs, *cursor)
            }
            Dialog::CreateConfig { choices, cursor } => render_picker(
                frame,
                area,
                "no config found, create one?",
                choices,
                *cursor,
            ),
            Dialog::Settings {
                config_path,
                path,
                cursors,
            } => render_settings(frame, area, config_path, path, cursors),
        }
    }
}

enum ListAction {
    Stay,
    Close,
    Select(usize),
}

fn list_key_action(key: KeyEvent, cursor: &mut usize, len: usize) -> ListAction {
    match key.code {
        KeyCode::Esc => ListAction::Close,
        KeyCode::Up | KeyCode::Char('k') => {
            if *cursor > 0 {
                *cursor -= 1;
            }
            ListAction::Stay
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if *cursor + 1 < len {
                *cursor += 1;
            }
            ListAction::Stay
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') if *cursor < len => {
            ListAction::Select(*cursor)
        }
        _ => ListAction::Stay,
    }
}

fn ascend(path: &mut Vec<String>, cursors: &mut Vec<usize>) {
    if !path.is_empty() {
        path.pop();
        if cursors.len() > 1 {
            cursors.pop();
        }
    }
}

/// A node in the settings nav tree. Today everything below the root is
/// a leaf; the structure leaves room for nested subpages later.
struct NavNode {
    title: &'static str,
    description: &'static str,
}

fn nav_children(path: &[String]) -> Vec<NavNode> {
    let segs: Vec<&str> = path.iter().map(String::as_str).collect();
    match segs.as_slice() {
        [] => vec![
            NavNode {
                title: "Providers",
                description: "Configure LLM providers, API keys, and the default model.",
            },
            NavNode {
                title: "Agents",
                description: "Manage agent definitions, presets, and per-agent overrides.",
            },
            NavNode {
                title: "Tools",
                description: "Tune which tools are exposed to agents and their permission scopes.",
            },
        ],
        _ => Vec::new(),
    }
}

fn render_picker(
    frame: &mut Frame,
    area: Rect,
    subtitle: &str,
    entries: &[ConfigDir],
    cursor: usize,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Settings — {subtitle} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no candidates)",
            Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX)),
        )));
    } else {
        let path_w = entries
            .iter()
            .map(|e| display_path(&e.path).chars().count())
            .max()
            .unwrap_or(0);
        for (i, entry) in entries.iter().enumerate() {
            let marker = if i == cursor { "▸ " } else { "  " };
            let path_str = display_path(&entry.path);
            let kind_str = kind_label(&entry.kind);
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::raw(marker));
            spans.push(Span::styled(
                pad_right(&path_str, path_w),
                if i == cursor {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ));
            spans.push(Span::raw("   "));
            spans.push(Span::styled(
                kind_str.to_string(),
                Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX)),
            ));
            lines.push(Line::from(spans));
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), layout[0]);

    frame.render_widget(help_line("↑/↓/jk  enter: select  esc: cancel"), layout[1]);
}

fn render_settings(
    frame: &mut Frame,
    area: Rect,
    config_path: &std::path::Path,
    path: &[String],
    cursors: &[usize],
) {
    let breadcrumbs = if path.is_empty() {
        display_path(config_path)
    } else {
        format!("{} › {}", display_path(config_path), path.join(" › "))
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Settings — {breadcrumbs} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

    let children = nav_children(path);
    if children.is_empty() {
        render_leaf_body(frame, layout[0], path);
        frame.render_widget(
            help_line("←/h/backspace: back  esc: close & apply"),
            layout[1],
        );
    } else {
        let cursor = cursors.last().copied().unwrap_or(0).min(children.len() - 1);
        let cols =
            Layout::horizontal([Constraint::Length(20), Constraint::Min(0)]).split(layout[0]);

        let list_lines: Vec<Line<'static>> = children
            .iter()
            .enumerate()
            .map(|(i, node)| {
                let marker = if i == cursor { "▸ " } else { "  " };
                let style = if i == cursor {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(node.title.to_string(), style),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(list_lines), cols[0]);

        let desc = children[cursor].description;
        frame.render_widget(
            Paragraph::new(desc.to_string())
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX))),
            cols[1],
        );

        frame.render_widget(
            help_line("↑/↓/jk  enter/→/l: open  esc: close & apply"),
            layout[1],
        );
    }
}

fn render_leaf_body(frame: &mut Frame, area: Rect, path: &[String]) {
    let title = path.last().map(String::as_str).unwrap_or("");
    let body = match title {
        "Providers" => {
            "(stub) Provider editor — list configured providers, add/remove, set the default model."
        }
        "Agents" => {
            "(stub) Agent editor — list agent definitions, edit their system prompts, tool grants, and model overrides."
        }
        "Tools" => {
            "(stub) Tool registry — toggle availability per tool and configure permission scopes."
        }
        _ => "(stub) Page not yet implemented.",
    };
    let lines = vec![
        Line::from(Span::styled(
            title.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        Line::from(Span::styled(
            body.to_string(),
            Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX)),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn help_line(text: &str) -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX)),
    )))
}

fn kind_label(kind: &ConfigDirKind) -> &'static str {
    match kind {
        ConfigDirKind::HomeXdg => "(home / XDG)",
        ConfigDirKind::HomeDot => "(home / dotfile)",
        ConfigDirKind::Project => "(project)",
    }
}

fn display_path(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rel) = path.strip_prefix(&home)
    {
        if rel.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

fn pad_right(s: &str, target: usize) -> String {
    let len = s.chars().count();
    if len >= target {
        s.to_string()
    } else {
        let mut out = s.to_string();
        for _ in len..target {
            out.push(' ');
        }
        out
    }
}
