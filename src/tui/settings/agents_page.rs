//! `/settings → Agents` page (`prompts/user-definable-agents.md`).
//!
//! Lists the bundled cast (`Build`/`coder`/`explore`) — marked when an
//! on-disk override shadows the embedded default — followed by any
//! user-authored custom agents (marked `custom`). The docs pipeline is
//! deliberately absent: it is a fixed two-stage internal pipeline, never
//! a user-editable [`crate::agents::AgentDef`].
//!
//! Actions:
//!   - `enter` / `e` on a **built-in** row *ejects* its embedded default
//!     to `<config-dir>/agents/<name>.md` for editing — or, when an
//!     override already exists, selects the existing file (no clobber).
//!   - `R` opens a confirmation to **reset all** built-in overrides,
//!     restoring the embedded defaults. Custom agents are never touched.
//!
//! The page reads agents fresh from disk on entry and after each
//! eject/reset so the overridden/custom markers stay accurate.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::agents::{AgentKind, AgentListing, is_builtin_agent, list_all};
use crate::tui::theme::MUTED_COLOR_INDEX;

use super::{Nav, Page, SettingsDialog};

/// `/settings → Agents` state.
pub(super) struct AgentsPage {
    pub(super) cursor: usize,
    /// True while the "reset all built-in agents" confirmation is shown.
    pub(super) confirm_reset: bool,
    pub(super) status: Option<String>,
    /// One row per discovered agent (built-ins first, then custom).
    pub(super) rows: Vec<AgentRow>,
}

/// A flattened, render-ready view of one [`AgentListing`]. We snapshot the
/// fields the page needs so the page state doesn't borrow the (non-`Clone`,
/// error-carrying) listing.
pub(super) struct AgentRow {
    pub(super) name: String,
    pub(super) kind: AgentKind,
    /// `Some(description)` when the agent parsed cleanly; `Some(error)`
    /// rendered distinctly when its file is malformed.
    pub(super) detail: Result<String, String>,
}

impl AgentsPage {
    /// Build the page by discovering agents at `cwd`.
    pub(super) fn new(cwd: &std::path::Path) -> Self {
        Self {
            cursor: 0,
            confirm_reset: false,
            status: None,
            rows: rows_for(cwd),
        }
    }
}

fn rows_for(cwd: &std::path::Path) -> Vec<AgentRow> {
    list_all(cwd)
        .into_iter()
        .map(|l: AgentListing| AgentRow {
            name: l.name,
            kind: l.kind,
            detail: match l.def {
                Ok(def) => Ok(def.description),
                Err(e) => Err(format!("{e}")),
            },
        })
        .collect()
}

impl SettingsDialog {
    /// The cwd agents are discovered against: the picker's cwd when the
    /// dialog was opened from one, else the directory holding the config
    /// being edited, else the process cwd. Agents resolve through the
    /// layered-config walk rooted here.
    pub(super) fn agents_cwd(&self) -> PathBuf {
        if let Some(cwd) = &self.picker_cwd {
            return cwd.clone();
        }
        // `config_path` is `<dir>/.cockpit/config.json` or similar; walk
        // up past the `.cockpit/` segment to a plausible project cwd.
        self.config_path
            .parent()
            .and_then(|p| p.parent())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// The config directory eject writes into: the directory holding the
    /// `config.json` this settings dialog is editing (the `.cockpit/`
    /// layer the user selected in the picker).
    fn agents_config_dir(&self) -> PathBuf {
        self.config_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub(super) fn handle_agents_key(&mut self, key: KeyEvent) -> bool {
        let placeholder = Page::Agents(AgentsPage {
            cursor: 0,
            confirm_reset: false,
            status: None,
            rows: Vec::new(),
        });
        let mut page = std::mem::replace(&mut self.page, placeholder);
        let nav = if let Page::Agents(p) = &mut page {
            self.handle_agents_page_key(key, p)
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

    fn handle_agents_page_key(&mut self, key: KeyEvent, p: &mut AgentsPage) -> Nav {
        // ── Reset-all confirmation ──────────────────────────────────
        if p.confirm_reset {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    p.confirm_reset = false;
                    let cwd = self.agents_cwd();
                    match crate::agents::reset_all_builtins(&cwd) {
                        Ok(removed) => {
                            p.status = Some(format!(
                                "reset {} built-in override(s) to default",
                                removed.len()
                            ));
                        }
                        Err(e) => p.status = Some(format!("reset failed: {e}")),
                    }
                    p.rows = rows_for(&cwd);
                    p.cursor = p.cursor.min(p.rows.len().saturating_sub(1));
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    p.confirm_reset = false;
                    p.status = Some("reset cancelled".into());
                }
                _ => {}
            }
            return Nav::Stay;
        }

        let len = p.rows.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Nav::Close,
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                return Nav::Replace(Page::Root {
                    cursor: self.last_root_cursor,
                });
            }
            KeyCode::Up | KeyCode::Char('k') if len > 0 => {
                p.cursor = crate::tui::nav::wrap_prev(p.cursor, len);
                p.status = None;
            }
            KeyCode::Down | KeyCode::Char('j') if len > 0 => {
                p.cursor = crate::tui::nav::wrap_next(p.cursor, len);
                p.status = None;
            }
            KeyCode::Char('R') => {
                p.confirm_reset = true;
                p.status = None;
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('e') => {
                self.eject_selected(p);
            }
            _ => {}
        }
        Nav::Stay
    }

    /// Eject the built-in under the cursor (or select its existing
    /// override). Custom agents already live on disk, so there's nothing
    /// to eject — we report their path instead.
    fn eject_selected(&mut self, p: &mut AgentsPage) {
        let Some(row) = p.rows.get(p.cursor) else {
            return;
        };
        let name = row.name.clone();
        let cwd = self.agents_cwd();
        if !is_builtin_agent(&name) {
            // Custom agent: surface where its file lives.
            match crate::agents::find_override(&cwd, &name) {
                Some(path) => p.status = Some(format!("custom agent — edit {}", path.display())),
                None => p.status = Some("custom agent".into()),
            }
            return;
        }
        let config_dir = self.agents_config_dir();
        match crate::agents::eject_builtin(&cwd, &config_dir, &name) {
            Ok((path, true)) => {
                p.status = Some(format!("ejected to {} — edit it there", path.display()))
            }
            Ok((path, false)) => {
                p.status = Some(format!("already ejected — edit {}", path.display()))
            }
            Err(e) => p.status = Some(format!("eject failed: {e}")),
        }
        p.rows = rows_for(&cwd);
    }

    pub(super) fn render_agents_page(&self, frame: &mut Frame, area: Rect, p: &AgentsPage) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let red = Style::default().fg(Color::Red);

        let mut lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                "Agents".to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            Line::from(Span::styled(
                "Built-in agents are embedded; editing one ejects its default \
                 to .cockpit/agents/<name>.md, which then overrides it. Drop a \
                 <name>.md into an agents dir to add your own."
                    .to_string(),
                muted,
            )),
            Line::default(),
        ];

        for (i, row) in p.rows.iter().enumerate() {
            let on_cursor = i == p.cursor;
            let marker = if on_cursor { "▸ " } else { "  " };
            let name_style = if on_cursor {
                yellow.add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let tag = match row.kind {
                AgentKind::Builtin { overridden: true } => " (built-in, overridden)",
                AgentKind::Builtin { overridden: false } => " (built-in)",
                AgentKind::Custom => " (custom)",
            };
            let mut spans = vec![
                Span::raw(marker),
                Span::styled(row.name.clone(), name_style),
                Span::styled(tag.to_string(), muted),
            ];
            if let Err(e) = &row.detail {
                spans.push(Span::styled(format!("  ⚠ {e}"), red));
            }
            lines.push(Line::from(spans));
            if let Ok(desc) = &row.detail {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(desc.clone(), muted),
                ]));
            }
        }

        if p.confirm_reset {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "Reset ALL built-in agents to default? This deletes their \
                 on-disk overrides (custom agents are kept).  y: confirm  n: cancel"
                    .to_string(),
                red.add_modifier(Modifier::BOLD),
            )));
        }

        if let Some(status) = &p.status {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(status.clone(), yellow)));
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use std::fs;
    use tempfile::TempDir;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    /// A settings dialog whose `config.json` lives in `<tmp>/.cockpit/`
    /// and whose picker cwd is `<tmp>`, on the Agents page.
    fn agents_dialog(tmp: &TempDir) -> SettingsDialog {
        let cockpit = tmp.path().join(".cockpit");
        fs::create_dir_all(&cockpit).unwrap();
        let config_path = cockpit.join("config.json");
        fs::write(&config_path, "{}").unwrap();
        let mut d = SettingsDialog::open_from_picker(config_path, tmp.path().to_path_buf());
        d.page = Page::Agents(AgentsPage::new(tmp.path()));
        d
    }

    fn page(d: &SettingsDialog) -> &AgentsPage {
        match &d.page {
            Page::Agents(p) => p,
            _ => panic!("expected Agents page"),
        }
    }

    #[test]
    fn lists_builtins() {
        let tmp = TempDir::new().unwrap();
        let d = agents_dialog(&tmp);
        let names: Vec<&str> = page(&d).rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"Build"));
        assert!(names.contains(&"coder"));
        assert!(names.contains(&"explore"));
        // The docs pipeline is never listed.
        assert!(!names.iter().any(|n| n.starts_with("docs")));
    }

    #[test]
    fn enter_on_builtin_ejects_it() {
        let tmp = TempDir::new().unwrap();
        let mut d = agents_dialog(&tmp);
        // Cursor starts at row 0 (`Build`). Enter ejects it.
        d.handle_key(press(KeyCode::Enter));
        let ejected = tmp.path().join(".cockpit/agents/Build.md");
        assert!(ejected.exists(), "Enter on a built-in writes its override");
        // The row is now marked overridden.
        let build_row = page(&d).rows.iter().find(|r| r.name == "Build").unwrap();
        assert!(matches!(
            build_row.kind,
            AgentKind::Builtin { overridden: true }
        ));
    }

    #[test]
    fn reset_all_confirm_removes_overrides() {
        let tmp = TempDir::new().unwrap();
        let mut d = agents_dialog(&tmp);
        // Eject one built-in and add a custom agent.
        d.handle_key(press(KeyCode::Enter)); // eject Build
        let agents_dir = tmp.path().join(".cockpit/agents");
        fs::write(
            agents_dir.join("my-reviewer.md"),
            "---\ndescription: r\n---\nb\n",
        )
        .unwrap();
        // Refresh the page so it sees the custom agent.
        if let Page::Agents(p) = &mut d.page {
            *p = AgentsPage::new(tmp.path());
        }
        // `R` then `y` resets.
        d.handle_key(press(KeyCode::Char('R')));
        assert!(page(&d).confirm_reset);
        d.handle_key(press(KeyCode::Char('y')));
        assert!(!page(&d).confirm_reset);
        assert!(
            !agents_dir.join("Build.md").exists(),
            "built-in override removed"
        );
        assert!(
            agents_dir.join("my-reviewer.md").exists(),
            "custom agent kept"
        );
    }

    #[test]
    fn reset_cancel_keeps_overrides() {
        let tmp = TempDir::new().unwrap();
        let mut d = agents_dialog(&tmp);
        d.handle_key(press(KeyCode::Enter)); // eject Build
        d.handle_key(press(KeyCode::Char('R')));
        d.handle_key(press(KeyCode::Char('n')));
        assert!(!page(&d).confirm_reset);
        assert!(
            tmp.path().join(".cockpit/agents/Build.md").exists(),
            "cancel keeps the override"
        );
    }
}
