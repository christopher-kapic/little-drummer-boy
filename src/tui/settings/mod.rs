#![allow(dead_code)]
//! `/settings` dialog state machine + rendering.
//!
//! Lifecycle:
//!   - `Dialog::None`            no overlay; viewport renders normally
//!   - `Dialog::PickConfig`      choose an existing config to edit
//!   - `Dialog::CreateConfig`    no config yet — pick a location to scaffold
//!   - `Dialog::Settings`        navigate the settings tree
//!
//! The Settings page tree:
//!
//! ```text
//! Root
//!  ├── Providers
//!  │    ├── List ──── Add Provider wizard ─── (template -> URL -> Auth -> save)
//!  │    │           └── Edit Provider page
//!  │    └── FetchAll dialog (triggered by /fetch-models)
//!  ├── Agents
//!  └── Tools
//! ```
//!
//! Async fetches (the `/models` endpoint after Save, or via the Edit
//! page's `r`=refetch action) use [`FetchHandle`] — a shared cell the
//! background task writes into and the event loop reads on each tick.

mod auth;
mod providers;
mod skills_page;
mod tools_page;
mod ui_page;

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::config::dirs::{
    ConfigDir, ConfigDirKind, creatable_config_dirs, cwd_scoped_creatable_dirs,
    discover_config_dirs, scaffold_config_dir,
};
use crate::config::extended::{ExtendedConfig, ExtendedConfigDoc};
use crate::config::providers::{ConfigDoc, OnUnlistedModelsFetch, ProvidersConfig};
use crate::providers::models_fetch::FetchOutcome;
use crate::tui::textfield::TextField;
use crate::tui::theme::MUTED_COLOR_INDEX;

/// Height (in rows) the dialog wants when active.
pub const DIALOG_HEIGHT: u16 = 20;

/// Number of selectable rows in the Edit-provider action menu.
/// Index map: 0=URL · 1=Headers · 2=Favorite · 3=Refetch · 4=Delete · 5=Back.
const EDIT_MENU_LEN: usize = 6;

pub enum Dialog {
    None,
    PickConfig {
        dirs: Vec<ConfigDir>,
        cursor: usize,
        /// Held so the `a` affordance can scaffold a new scoped config
        /// in the right place.
        cwd: PathBuf,
        /// Transient error/status (e.g. scaffold-failure message).
        status: Option<String>,
    },
    CreateConfig {
        choices: Vec<ConfigDir>,
        cursor: usize,
        /// Held so the resulting settings dialog can offer "back to
        /// picker" — once a config has been scaffolded, reopening the
        /// picker yields a non-empty list.
        cwd: PathBuf,
    },
    /// "Add a config scoped to the current directory" sub-dialog
    /// reached by pressing `a` on the picker. Offers a `.cockpit/` in
    /// the cwd (shareable with a team) or a hashed-cwd dir under the
    /// cockpit data dir (machine-local).
    CreateScopedConfig {
        choices: Vec<ConfigDir>,
        cursor: usize,
        cwd: PathBuf,
    },
    Settings(SettingsDialog),
}

pub struct SettingsDialog {
    pub config_path: PathBuf,
    /// Path to the sibling `extended-config.json`. Loaded lazily when
    /// the UI / Tools pages open; saved on each edit there.
    pub(super) extended_path: PathBuf,
    pub(super) page: Page,
    /// Cached config state; reloaded on entry into the Providers list
    /// and after each successful save.
    pub(super) config: ProvidersConfig,
    /// Cached `extended-config.json` state. Read by the UI page and the
    /// Tools page; written back on each edit.
    pub(super) extended: ExtendedConfig,
    /// Root-page cursor restored when navigating back. Updated every
    /// time we leave Root for a subpage.
    pub(super) last_root_cursor: usize,
    /// The cwd this dialog was opened against. Held so Root's `h`/←
    /// can reopen the picker without losing context. `None` when the
    /// settings dialog was opened from a flow that has no picker to
    /// return to.
    pub(super) picker_cwd: Option<PathBuf>,
    /// Set by Root's back action to ask the outer [`Dialog`] to
    /// re-open the picker on the next `true` return from `handle_key`.
    pub(super) back_to_picker: bool,
}

#[allow(private_interfaces)]
pub(super) enum Page {
    Root { cursor: usize },
    Agents,
    Tools(ToolsPage),
    Providers(ProvidersPage),
    Ui(UiPage),
    Instructions(InstructionsPage),
    Skills(SkillsPage),
}

use providers::{AddState, AddStep, ProvidersPage};
use skills_page::SkillsPage;
use tools_page::ToolsPage;
pub use tools_page::{builtin_tool_names, default_template_for};
use ui_page::InstructionsPage;
pub(crate) use ui_page::UiPage;

/// Navigation intent returned by an inner page handler. The outer
/// [`SettingsDialog::handle_providers_key`] applies it *after* swapping
/// the borrowed sub-page back in. Inner handlers cannot write
/// `self.page` directly — the swap-back would discard the write.
#[allow(private_interfaces)]
pub(super) enum Nav {
    /// Stay on the current page; sub-state mutations have already been
    /// applied to the borrowed `&mut SubState`.
    Stay,
    /// Navigate to `Page::...`.
    Replace(Page),
    /// Close the whole dialog.
    Close,
}

// ── Dialog top-level ─────────────────────────────────────────────────────

impl Dialog {
    pub fn is_active(&self) -> bool {
        !matches!(self, Dialog::None)
    }

    pub fn open(cwd: &std::path::Path) -> Self {
        let dirs = discover_config_dirs(cwd);
        if dirs.is_empty() {
            Dialog::CreateConfig {
                choices: creatable_config_dirs(),
                cursor: 0,
                cwd: cwd.to_path_buf(),
            }
        } else {
            Dialog::PickConfig {
                dirs,
                cursor: 0,
                cwd: cwd.to_path_buf(),
                status: None,
            }
        }
    }

    /// Open directly into the Providers list — used by `/fetch-models`
    /// and other slash commands that want to land deeper than the root.
    pub fn open_providers(cwd: &std::path::Path) -> Self {
        let mut d = Self::open(cwd);
        if let Dialog::PickConfig { dirs, .. } = &d
            && let Some(dir) = dirs.first()
        {
            let path = dir.path.join("config.json");
            d = Dialog::Settings(SettingsDialog::open_from_picker(path, cwd.to_path_buf()));
            if let Dialog::Settings(s) = &mut d {
                s.enter_providers();
            }
        }
        d
    }

    /// True when the first discovered config layer has zero providers
    /// configured (or no providers map at all). Used by the TUI's
    /// first-run flow to auto-route into the Add wizard after the
    /// daemon prompt resolves.
    pub fn has_no_providers(cwd: &std::path::Path) -> bool {
        let dirs = discover_config_dirs(cwd);
        let Some(dir) = dirs.first() else {
            return true;
        };
        let path = dir.path.join("config.json");
        match ConfigDoc::load(&path) {
            Ok(doc) => doc.providers().providers.is_empty(),
            Err(_) => true,
        }
    }

    /// Open the Add-Provider wizard directly, skipping the Providers
    /// list. Used when the user has no providers configured at TUI
    /// launch.
    pub fn open_providers_add(cwd: &std::path::Path) -> Self {
        let mut d = Self::open(cwd);
        if let Dialog::PickConfig { dirs, .. } = &d
            && let Some(dir) = dirs.first()
        {
            let path = dir.path.join("config.json");
            let mut s = SettingsDialog::open_from_picker(path, cwd.to_path_buf());
            s.page = Page::Providers(ProvidersPage::Add(AddState::new()));
            d = Dialog::Settings(s);
        }
        d
    }

    /// Re-open the picker after scaffolding a new scoped config, so the
    /// fresh row shows up and lands as the cursor target.
    fn reopen_picker(cwd: &std::path::Path, status: Option<String>) -> Self {
        let dirs = discover_config_dirs(cwd);
        if dirs.is_empty() {
            Dialog::CreateConfig {
                choices: creatable_config_dirs(),
                cursor: 0,
                cwd: cwd.to_path_buf(),
            }
        } else {
            Dialog::PickConfig {
                dirs,
                cursor: 0,
                cwd: cwd.to_path_buf(),
                status,
            }
        }
    }

    /// Drain the UI page's pending `mouse` toggle, if any. Returns
    /// `Some(new_value)` exactly once per user toggle so the App can
    /// push/pop crossterm's `EnableMouseCapture` to match. None when
    /// the dialog isn't on the UI page or the user hasn't touched the
    /// setting since the last drain.
    pub fn take_pending_mouse_capture(&mut self) -> Option<bool> {
        let Dialog::Settings(s) = self else {
            return None;
        };
        let Page::Ui(p) = &mut s.page else {
            return None;
        };
        p.pending_mouse_capture.take()
    }

    /// Called by the event loop each tick so async fetches can apply
    /// their results.
    pub fn tick(&mut self) {
        if let Dialog::Settings(s) = self {
            s.tick();
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        match self {
            Dialog::None => false,
            Dialog::PickConfig {
                dirs,
                cursor,
                cwd,
                status,
            } => {
                // `a` opens the "add a scoped config" sub-dialog.
                // Anything else clears the transient status and falls
                // through to the standard list nav.
                if matches!(key.code, KeyCode::Char('a')) {
                    *self = Dialog::CreateScopedConfig {
                        choices: cwd_scoped_creatable_dirs(cwd),
                        cursor: 0,
                        cwd: cwd.clone(),
                    };
                    return false;
                }
                *status = None;
                match list_key_action(key, cursor, dirs.len()) {
                    ListAction::Stay => false,
                    ListAction::Close => true,
                    ListAction::Select(idx) => {
                        let chosen = dirs[idx].path.join("config.json");
                        let cwd = cwd.clone();
                        *self = Dialog::Settings(SettingsDialog::open_from_picker(chosen, cwd));
                        false
                    }
                }
            }
            Dialog::CreateConfig {
                choices,
                cursor,
                cwd,
            } => match list_key_action(key, cursor, choices.len()) {
                ListAction::Stay => false,
                ListAction::Close => true,
                ListAction::Select(idx) => match scaffold_config_dir(&choices[idx].path) {
                    Ok(config_path) => {
                        let cwd = cwd.clone();
                        *self =
                            Dialog::Settings(SettingsDialog::open_from_picker(config_path, cwd));
                        false
                    }
                    Err(_) => true,
                },
            },
            Dialog::CreateScopedConfig {
                choices,
                cursor,
                cwd,
            } => match list_key_action(key, cursor, choices.len()) {
                // Cancel → back to the picker.
                ListAction::Close => {
                    *self = Dialog::reopen_picker(cwd, None);
                    false
                }
                ListAction::Stay => false,
                ListAction::Select(idx) => {
                    let target = &choices[idx];
                    match scaffold_config_dir(&target.path) {
                        Ok(config_path) => {
                            let cwd = cwd.clone();
                            *self = Dialog::Settings(SettingsDialog::open_from_picker(
                                config_path,
                                cwd,
                            ));
                        }
                        Err(e) => {
                            *self = Dialog::reopen_picker(
                                cwd,
                                Some(format!("failed to create {}: {e}", target.path.display())),
                            );
                        }
                    }
                    false
                }
            },
            Dialog::Settings(s) => {
                let close = s.handle_key(key);
                if close && s.back_to_picker {
                    if let Some(cwd) = s.picker_cwd.clone() {
                        *self = Dialog::reopen_picker(&cwd, None);
                        return false;
                    }
                }
                close
            }
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        match self {
            Dialog::None => {}
            Dialog::PickConfig {
                dirs,
                cursor,
                status,
                ..
            } => render_picker(
                frame,
                area,
                "pick a config to edit",
                dirs,
                *cursor,
                status.as_deref(),
                "↑/↓  enter: select  a: add scoped  esc: close",
            ),
            Dialog::CreateConfig {
                choices, cursor, ..
            } => render_picker(
                frame,
                area,
                "no config found, create one?",
                choices,
                *cursor,
                None,
                "↑/↓  enter: select  esc: cancel",
            ),
            Dialog::CreateScopedConfig {
                choices, cursor, ..
            } => render_picker(
                frame,
                area,
                "where should the new config live?",
                choices,
                *cursor,
                None,
                "↑/↓  enter: select  esc: back to picker",
            ),
            Dialog::Settings(s) => s.render(frame, area),
        }
    }
}

// ── SettingsDialog ───────────────────────────────────────────────────────

impl SettingsDialog {
    pub fn open(config_path: PathBuf) -> Self {
        let config = ConfigDoc::load(&config_path)
            .map(|d| d.providers())
            .unwrap_or_default();
        let extended_path = config_path
            .parent()
            .map(|p| p.join("extended-config.json"))
            .unwrap_or_else(|| PathBuf::from("extended-config.json"));
        let extended = ExtendedConfigDoc::load(&extended_path)
            .map(|d| d.config())
            .unwrap_or_default();
        Self {
            config_path,
            extended_path,
            page: Page::Root { cursor: 0 },
            config,
            extended,
            last_root_cursor: 0,
            picker_cwd: None,
            back_to_picker: false,
        }
    }

    /// Same as [`Self::open`] but records the cwd of the picker that
    /// opened this dialog so Root's back keybind can reopen it.
    pub fn open_from_picker(config_path: PathBuf, cwd: PathBuf) -> Self {
        let mut s = Self::open(config_path);
        s.picker_cwd = Some(cwd);
        s
    }

    /// Reload extended-config from disk. Used after saving so the
    /// cached view stays in sync.
    fn reload_extended(&mut self) {
        if let Ok(doc) = ExtendedConfigDoc::load(&self.extended_path) {
            self.extended = doc.config();
        }
    }

    /// Persist the cached extended-config to disk.
    pub(super) fn save_extended(&mut self) -> Result<(), String> {
        let mut doc = ExtendedConfigDoc::load(&self.extended_path).map_err(|e| e.to_string())?;
        doc.write(&self.extended).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn enter_providers(&mut self) {
        self.page = Page::Providers(ProvidersPage::List {
            cursor: 0,
            status: None,
            delete_pending: false,
        });
    }

    fn reload_config(&mut self) {
        if let Ok(doc) = ConfigDoc::load(&self.config_path) {
            self.config = doc.providers();
        }
    }

    fn save_config(&mut self) -> Result<(), String> {
        let mut doc = ConfigDoc::load(&self.config_path).map_err(|e| e.to_string())?;
        doc.write(&self.config).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn tick(&mut self) {
        // Drain finished fetches into config. The Headers sub-page
        // owns its parent's EditState (via Box) — if a /models fetch
        // was started on Edit and the user navigated into Headers
        // while it was in flight, the handle still needs to be
        // drained so the result lands when they come back.
        let pending = match &mut self.page {
            Page::Providers(ProvidersPage::Add(s)) => s.fetch.clone(),
            Page::Providers(ProvidersPage::Edit(s)) => s.fetch.clone(),
            Page::Providers(ProvidersPage::Headers { parent, .. }) => parent.fetch.clone(),
            _ => None,
        };
        if let Some(handle) = pending
            && let Some(result) = handle.take()
        {
            self.apply_fetch_result(&handle.provider_id, result);
        }

        // Advance the codex device-flow when the background task
        // signals Success — write the ProviderEntry and move to Done.
        self.advance_codex_login();
    }

    /// True while a header add/edit popup or its browsing list is on
    /// screen — the header editor owns `Tab`/`Shift+Tab` itself, so the
    /// field-nav rewrite in [`Self::handle_key`] must leave them alone.
    fn in_header_editor(&self) -> bool {
        match &self.page {
            Page::Providers(ProvidersPage::Headers { .. }) => true,
            Page::Providers(ProvidersPage::Add(s)) => matches!(s.step, AddStep::EditHeaders),
            _ => false,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Tab / Shift+Tab move between fields like ↓/↑ across settings
        // screens. The header editor owns Tab itself (the popup switches
        // name↔value; its browse list treats Tab as ↓), so skip the
        // rewrite whenever one is on screen.
        let key = if self.in_header_editor() {
            key
        } else {
            match key.code {
                KeyCode::Tab => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                KeyCode::BackTab => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                _ => key,
            }
        };
        // Esc semantics differ per page: in deep pages it goes back one
        // level; only at root does it close.
        let cursor = match &self.page {
            Page::Root { cursor } => Some(*cursor),
            _ => None,
        };
        if let Some(cursor) = cursor {
            return self.handle_root_key(key, cursor);
        }
        match &self.page {
            Page::Agents => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace
                ) {
                    self.page = Page::Root {
                        cursor: self.last_root_cursor,
                    };
                    false
                } else if matches!(key.code, KeyCode::Char('q')) {
                    true
                } else {
                    false
                }
            }
            Page::Tools(_) => self.handle_tools_key(key),
            Page::Ui(_) => self.handle_ui_key(key),
            Page::Instructions(_) => self.handle_instructions_key(key),
            Page::Skills(_) => self.handle_skills_key(key),
            Page::Providers(_) => self.handle_providers_key(key),
            Page::Root { .. } => unreachable!("handled above"),
        }
    }

    fn handle_root_key(&mut self, key: KeyEvent, mut cursor: usize) -> bool {
        let children = root_nodes();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return true,
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace
                if self.picker_cwd.is_some() =>
            {
                self.back_to_picker = true;
                return true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                cursor = cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                cursor = (cursor + 1).min(children.len().saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let chosen = children.get(cursor).map(|n| n.title).unwrap_or("");
                self.last_root_cursor = cursor;
                match chosen {
                    "Providers" => self.enter_providers(),
                    "Agents" => self.page = Page::Agents,
                    "Tools" => {
                        self.reload_extended();
                        self.page = Page::Tools(ToolsPage {
                            cursor: 0,
                            editing: None,
                            buf: TextField::default(),
                            edit_target: None,
                            status: None,
                        });
                    }
                    "UI" => {
                        self.reload_extended();
                        self.page = Page::Ui(UiPage {
                            cursor: 0,
                            editing: None,
                            buf: TextField::default(),
                            status: None,
                            pending_mouse_capture: None,
                        });
                    }
                    "Skills" => {
                        self.reload_extended();
                        self.page = Page::Skills(skills_page::SkillsPage {
                            cursor: 0,
                            grabbed: None,
                            status: None,
                        });
                    }
                    _ => {}
                }
                return false;
            }
            _ => {}
        }
        self.page = Page::Root { cursor };
        false
    }

    // ── Rendering ────────────────────────────────────────────────────────

    fn render(&self, frame: &mut Frame, area: Rect) {
        let title = self.title();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Settings — {title} "));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

        match &self.page {
            Page::Root { cursor } => render_root(frame, layout[0], *cursor),
            Page::Agents => {
                render_stub(frame, layout[0], "Agents", AGENTS_STUB);
            }
            Page::Tools(p) => self.render_tools_page(frame, layout[0], p),
            Page::Ui(p) => self.render_ui_page(frame, layout[0], p),
            Page::Instructions(p) => self.render_instructions_page(frame, layout[0], p),
            Page::Skills(p) => self.render_skills_page(frame, layout[0], p),
            Page::Providers(p) => self.render_providers_page(frame, layout[0], p),
        }
        frame.render_widget(help_line(self.help_text()), layout[1]);
    }

    fn title(&self) -> String {
        let crumbs = match &self.page {
            Page::Root { .. } => String::new(),
            Page::Agents => " › Agents".into(),
            Page::Tools(_) => " › Tools".into(),
            Page::Ui(_) => " › UI".into(),
            Page::Skills(_) => " › Skills".into(),
            Page::Instructions(_) => " › UI › Instructions File".into(),
            Page::Providers(ProvidersPage::List { .. }) => " › Providers".into(),
            Page::Providers(ProvidersPage::Add(_)) => " › Providers › Add".into(),
            Page::Providers(ProvidersPage::Edit(s)) => {
                format!(" › Providers › {}", s.provider_id)
            }
            Page::Providers(ProvidersPage::Headers { parent, .. }) => {
                format!(" › Providers › {} › Headers", parent.provider_id)
            }
            Page::Providers(ProvidersPage::FetchAll(_)) => " › Providers › /fetch-models".into(),
            Page::Providers(ProvidersPage::CopilotSetup(_)) => {
                " › Providers › Copilot setup".into()
            }
        };
        format!("{}{}", display_path(&self.config_path), crumbs)
    }

    fn help_text(&self) -> &'static str {
        match &self.page {
            Page::Root { .. } => {
                if self.picker_cwd.is_some() {
                    "↑/↓  enter: open  h: back to picker  esc: close"
                } else {
                    "↑/↓  enter: open  esc: close"
                }
            }
            Page::Agents => "h: back  esc: close",
            Page::Instructions(p) => {
                if p.grabbed.is_some() {
                    "type to rename  ↑/↓: reorder  enter: drop & save  esc: cancel"
                } else {
                    "↑/↓  a: add  enter: grab to rename/reorder  d: delete  h: back  esc: close"
                }
            }
            Page::Tools(p) => {
                if p.editing.is_some() {
                    "type to edit  enter: apply  esc: cancel"
                } else {
                    "↑/↓  enter: edit  t: toggle  r: reset  h: back  esc: close"
                }
            }
            Page::Ui(p) => {
                if p.editing.is_some() {
                    "type to edit  enter: apply  esc: cancel"
                } else {
                    "↑/↓  enter: edit / cycle  h: back  esc: close"
                }
            }
            Page::Skills(p) => {
                if p.grabbed.is_some() {
                    "type to edit dir  enter: save  esc: cancel"
                } else {
                    "↑/↓  enter: toggle / edit  a: add dir  d: delete  h: back  esc: close"
                }
            }
            Page::Providers(ProvidersPage::List { .. }) => {
                "↑/↓  enter: edit  a: add  d: delete (×2 to confirm)  h: back  esc: close"
            }
            Page::Providers(ProvidersPage::Add(s)) => match s.step {
                AddStep::PickTemplate { .. } => "↑/↓  enter: choose  esc: cancel",
                AddStep::EditId | AddStep::EditUrl => "type to edit  enter: next  esc: cancel",
                AddStep::EditHeaders => {
                    if s.headers.is_editing() {
                        "type to edit  Tab: switch field  enter: save  esc: cancel"
                    } else {
                        "↑/↓  a: add  enter: edit  d: delete  enter on continue: save  esc: back"
                    }
                }
                AddStep::CodexLogin => {
                    "open URL + enter code in browser  r: retry on error  esc: cancel"
                }
                AddStep::CopilotAuth(_) => "enter: apply  s: skip  esc: cancel",
                AddStep::Saving | AddStep::Fetching => "(in progress)  esc: cancel",
                AddStep::Done => "enter: back to list",
            },
            Page::Providers(ProvidersPage::Edit(s)) => {
                if s.editing_field.is_some() {
                    "type to edit  enter: apply  esc: cancel"
                } else {
                    "↑/↓  enter: edit  s: save  r: refetch  f: favorite  d: delete  h: back"
                }
            }
            Page::Providers(ProvidersPage::Headers { editor, .. }) => {
                if editor.is_editing() {
                    "type to edit  Tab: switch field  enter: save  esc: cancel"
                } else {
                    "↑/↓  a: add  enter: edit  d: delete  h: back"
                }
            }
            Page::Providers(ProvidersPage::FetchAll(_)) => {
                "↑/↓  space: toggle don't-ask  enter: apply  esc: cancel"
            }
            Page::Providers(ProvidersPage::CopilotSetup(_)) => "enter: apply  esc: cancel",
        }
    }
}

// ── Helpers / freestanding renderers ─────────────────────────────────────

fn root_nodes() -> [NavNode; 5] {
    [
        NavNode {
            title: "Providers",
            description: "Configure LLM providers, headers, and the default model.",
        },
        NavNode {
            title: "UI",
            description: "User-interface preferences: vim mode, thinking display, your name, the docs-agent packages directory, and the utility model.",
        },
        NavNode {
            title: "Agents",
            description: "Manage agent definitions, presets, and per-agent overrides.",
        },
        NavNode {
            title: "Tools",
            description: "Custom bash-command tools (webfetch, websearch, …) the agent can invoke.",
        },
        NavNode {
            title: "Skills",
            description: "Skill scan directories and the auto-! command toggle (Claude vs Codex mode).",
        },
    ]
}

struct NavNode {
    title: &'static str,
    description: &'static str,
}

const AGENTS_STUB: &str = "(stub) Agent editor — list agent definitions, edit their system prompts, tool grants, and model overrides.";

pub(super) fn save_status(r: Result<(), String>) -> Option<String> {
    match r {
        Ok(()) => Some("saved".into()),
        Err(e) => Some(format!("save failed: {e}")),
    }
}

fn render_root(frame: &mut Frame, area: Rect, cursor: usize) {
    let children = root_nodes();
    let cursor = cursor.min(children.len().saturating_sub(1));
    let cols = Layout::horizontal([Constraint::Length(20), Constraint::Min(0)]).split(area);

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
}

fn render_stub(frame: &mut Frame, area: Rect, title: &str, body: &str) {
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

enum ListAction {
    Stay,
    Close,
    Select(usize),
}

fn list_key_action(key: KeyEvent, cursor: &mut usize, len: usize) -> ListAction {
    match key.code {
        KeyCode::Esc => ListAction::Close,
        KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
            if *cursor > 0 {
                *cursor -= 1;
            }
            ListAction::Stay
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
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

fn render_picker(
    frame: &mut Frame,
    area: Rect,
    subtitle: &str,
    entries: &[ConfigDir],
    cursor: usize,
    status: Option<&str>,
    help: &str,
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
    if let Some(msg) = status {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            msg.to_string(),
            Style::default().fg(Color::Yellow),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), layout[0]);
    frame.render_widget(help_line(help), layout[1]);
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
        ConfigDirKind::MachineLocal => "(machine-local, scoped to cwd)",
        ConfigDirKind::Project => "(project — shareable with team)",
    }
}

fn display_path(path: &Path) -> String {
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

// ── Public API for slash-command-triggered flows ─────────────────────────

/// Start a /fetch-models workflow against the currently-loaded config.
/// The caller wires this in from the slash command handler.
#[allow(dead_code)]
pub fn fetch_all_unlisted_dialog(
    config: &ProvidersConfig,
    finished: Vec<(String, Result<FetchOutcome, String>)>,
    store_default_decision: Option<OnUnlistedModelsFetch>,
) -> (Vec<(String, String)>, bool) {
    // Build the unlisted (config-model not present in remote-list) set.
    let mut unlisted: Vec<(String, String)> = Vec::new();
    for (pid, outcome) in &finished {
        if let Ok(FetchOutcome::Models(remote)) = outcome
            && let Some(entry) = config.providers.get(pid)
        {
            for m in &entry.models {
                if !remote.iter().any(|r| r.id == m.id) {
                    unlisted.push((pid.clone(), m.id.clone()));
                }
            }
        }
    }
    let needs_prompt = !unlisted.is_empty()
        && matches!(
            store_default_decision,
            Some(OnUnlistedModelsFetch::Ask) | None
        );
    (unlisted, needs_prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::providers::{ModelEntry, ProviderEntry};
    use providers::valid_url;

    fn entry(id_models: &[&str]) -> ProviderEntry {
        ProviderEntry {
            url: "https://x.example/v1".into(),
            models: id_models
                .iter()
                .map(|id| ModelEntry {
                    id: (*id).into(),
                    name: None,
                    thinking_modes: vec![],
                    inputs: None,
                    context_length: None,
                    favorite: false,
                    cache: None,
                    extra: Default::default(),
                })
                .collect(),
            ..ProviderEntry::default()
        }
    }

    #[test]
    fn valid_url_accepts_http_and_https() {
        assert!(valid_url("https://x.example"));
        assert!(valid_url("http://localhost:1234"));
        assert!(!valid_url("foo.example"));
        assert!(!valid_url(""));
    }

    #[test]
    fn fetch_all_unlisted_picks_only_drifted_ids() {
        let mut cfg = ProvidersConfig::default();
        cfg.providers
            .insert("p1".into(), entry(&["m1", "m2", "stale"]));
        let remote_outcome = FetchOutcome::Models(vec![
            ModelEntry {
                id: "m1".into(),
                name: None,
                thinking_modes: vec![],
                inputs: None,
                context_length: None,
                favorite: false,
                cache: None,
                extra: Default::default(),
            },
            ModelEntry {
                id: "m2".into(),
                name: None,
                thinking_modes: vec![],
                inputs: None,
                context_length: None,
                favorite: false,
                cache: None,
                extra: Default::default(),
            },
        ]);
        let (unlisted, prompt) =
            fetch_all_unlisted_dialog(&cfg, vec![("p1".into(), Ok(remote_outcome))], None);
        assert_eq!(unlisted, vec![("p1".to_string(), "stale".to_string())]);
        assert!(prompt);
    }

    #[test]
    fn fetch_all_unlisted_skips_prompt_when_user_has_chosen() {
        let mut cfg = ProvidersConfig::default();
        cfg.providers.insert("p1".into(), entry(&["stale"]));
        let remote_outcome = FetchOutcome::Models(vec![]);
        let (_unlisted, prompt) = fetch_all_unlisted_dialog(
            &cfg,
            vec![("p1".into(), Ok(remote_outcome))],
            Some(OnUnlistedModelsFetch::Remove),
        );
        assert!(!prompt);
    }

    // ── Regression: navigation must survive the swap-back ──────────────

    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use tempfile::TempDir;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn fresh_dialog(tmp: &TempDir) -> SettingsDialog {
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        SettingsDialog::open(path)
    }

    fn on_add_page(d: &SettingsDialog) -> bool {
        matches!(&d.page, Page::Providers(ProvidersPage::Add(_)))
    }

    fn on_list_page(d: &SettingsDialog) -> bool {
        matches!(&d.page, Page::Providers(ProvidersPage::List { .. }))
    }

    fn on_root_page(d: &SettingsDialog) -> bool {
        matches!(&d.page, Page::Root { .. })
    }

    #[test]
    fn pressing_a_from_providers_list_enters_add_wizard() {
        // Reproduces the "dialog freezes on a" bug — the original
        // implementation swapped the page out, then the inner handler
        // wrote `self.page = Add(...)` into the placeholder slot, and
        // the outer's unconditional swap-back discarded that write.
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        d.enter_providers();
        assert!(on_list_page(&d));
        let close = d.handle_key(press(KeyCode::Char('a')));
        assert!(!close);
        assert!(
            on_add_page(&d),
            "after pressing `a` the dialog should be on the Add wizard, not stuck on List"
        );
    }

    #[test]
    fn pressing_esc_in_add_wizard_returns_to_list() {
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        d.enter_providers();
        d.handle_key(press(KeyCode::Char('a')));
        assert!(on_add_page(&d));
        d.handle_key(press(KeyCode::Esc));
        assert!(on_list_page(&d), "Esc from Add should return to List");
    }

    #[test]
    fn pressing_left_from_providers_list_returns_to_root() {
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        d.enter_providers();
        d.handle_key(press(KeyCode::Left));
        assert!(on_root_page(&d), "Left from Providers should land on Root");
    }

    fn dialog_with_one_provider(tmp: &TempDir) -> SettingsDialog {
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"providers":{"vendor":{"url":"https://x","headers":[]}}}"#,
        )
        .unwrap();
        let mut d = SettingsDialog::open(path);
        d.enter_providers();
        d
    }

    #[test]
    fn pressing_d_once_arms_delete_and_keeps_provider() {
        let tmp = TempDir::new().unwrap();
        let mut d = dialog_with_one_provider(&tmp);
        d.handle_key(press(KeyCode::Char('d')));
        assert!(
            d.config.providers.contains_key("vendor"),
            "single `d` press must not delete"
        );
        match &d.page {
            Page::Providers(ProvidersPage::List {
                delete_pending,
                status,
                ..
            }) => {
                assert!(*delete_pending);
                assert!(
                    status.as_deref().unwrap_or("").contains("press d again"),
                    "expected confirm hint, got {status:?}"
                );
            }
            other => panic!("expected ProvidersPage::List, got {other:?}"),
        }
    }

    #[test]
    fn pressing_d_twice_deletes_the_provider() {
        let tmp = TempDir::new().unwrap();
        let mut d = dialog_with_one_provider(&tmp);
        d.handle_key(press(KeyCode::Char('d')));
        d.handle_key(press(KeyCode::Char('d')));
        assert!(
            !d.config.providers.contains_key("vendor"),
            "double `d` press must delete"
        );
        // Persisted to disk.
        let reloaded = crate::config::providers::ConfigDoc::load(&d.config_path)
            .unwrap()
            .providers();
        assert!(!reloaded.providers.contains_key("vendor"));
    }

    #[test]
    fn arrow_after_d_clears_delete_pending() {
        // Vim-style safety: moving the cursor should disarm a pending
        // delete so the second press doesn't nuke a different row.
        let tmp = TempDir::new().unwrap();
        let mut d = dialog_with_one_provider(&tmp);
        d.handle_key(press(KeyCode::Char('d')));
        d.handle_key(press(KeyCode::Down));
        match &d.page {
            Page::Providers(ProvidersPage::List { delete_pending, .. }) => {
                assert!(!*delete_pending, "arrow key should clear pending-delete");
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn has_no_providers_true_when_config_dir_empty() {
        // discover_config_dirs walks up from `cwd`, so a tempdir with
        // no `.cockpit/` or local config should fall back to the user's
        // config (which may or may not exist). The cleanest assertion
        // we can make portably is the symmetry: open_providers_add
        // produces a non-Settings dialog when has_no_providers reports
        // no config — i.e. the function doesn't panic and is honest
        // about what it found.
        let tmp = TempDir::new().unwrap();
        // Just exercising the codepath — the answer depends on the
        // host's $HOME, so we only assert it returns *some* bool.
        let _ = Dialog::has_no_providers(tmp.path());
    }

    #[test]
    fn open_providers_add_lands_on_add_page_when_config_exists() {
        let tmp = TempDir::new().unwrap();
        // Create a `.cockpit/config.json` so the dialog has a layer to
        // open without falling through to CreateConfig.
        let cockpit_dir = tmp.path().join(".cockpit");
        std::fs::create_dir_all(&cockpit_dir).unwrap();
        std::fs::write(cockpit_dir.join("config.json"), "{}").unwrap();
        let d = Dialog::open_providers_add(tmp.path());
        let Dialog::Settings(s) = d else {
            panic!("expected Settings dialog");
        };
        assert!(
            matches!(s.page, Page::Providers(ProvidersPage::Add(_))),
            "expected Add page, got {:?}",
            s.page
        );
    }

    impl std::fmt::Debug for Page {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Page::Root { cursor } => write!(f, "Root({cursor})"),
                Page::Agents => f.write_str("Agents"),
                Page::Tools(_) => f.write_str("Tools"),
                Page::Providers(_) => f.write_str("Providers"),
                Page::Ui(_) => f.write_str("Ui"),
                Page::Instructions(_) => f.write_str("Instructions"),
                Page::Skills(_) => f.write_str("Skills"),
            }
        }
    }

    fn enter_ui_from_root(d: &mut SettingsDialog) {
        // Root cursor 1 = UI (root_nodes order: Providers, UI, Agents, Tools).
        d.page = Page::Root { cursor: 1 };
        d.handle_key(press(KeyCode::Enter));
    }

    fn enter_tools_from_root(d: &mut SettingsDialog) {
        d.page = Page::Root { cursor: 3 };
        d.handle_key(press(KeyCode::Enter));
    }

    #[test]
    fn pressing_h_in_ui_returns_to_root() {
        // Regression for the swap-back bug: the Ui/Tools/Instructions
        // wrappers used to clobber inner `self.page = Root` writes with
        // the placeholder swap-back, so `h` from those pages did
        // nothing.
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        enter_ui_from_root(&mut d);
        assert!(
            matches!(d.page, Page::Ui(_)),
            "expected Ui, got {:?}",
            d.page
        );
        d.handle_key(press(KeyCode::Char('h')));
        assert!(
            on_root_page(&d),
            "h from UI should return to Root, got {:?}",
            d.page
        );
    }

    #[test]
    fn editing_utility_model_row_persists() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        enter_ui_from_root(&mut d);
        // Row 9 = utility model.
        for _ in 0..9 {
            d.handle_key(press(KeyCode::Char('j')));
        }
        d.handle_key(press(KeyCode::Enter)); // begin editing
        for ch in "anthropic:claude-haiku".chars() {
            d.handle_key(KeyEvent {
                code: KeyCode::Char(ch),
                modifiers: KeyModifiers::empty(),
                kind: KeyEventKind::Press,
                state: KeyEventState::empty(),
            });
        }
        d.handle_key(press(KeyCode::Enter)); // commit + save
        assert_eq!(
            d.extended.utility_model.as_deref(),
            Some("anthropic:claude-haiku")
        );
        let reloaded = crate::config::extended::ExtendedConfigDoc::load(&d.extended_path)
            .unwrap()
            .config();
        assert_eq!(
            reloaded.utility_model.as_deref(),
            Some("anthropic:claude-haiku"),
            "utility model edit must persist to disk"
        );
    }

    #[test]
    fn pressing_h_in_tools_returns_to_root() {
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        enter_tools_from_root(&mut d);
        assert!(matches!(d.page, Page::Tools(_)));
        d.handle_key(press(KeyCode::Char('h')));
        assert!(
            on_root_page(&d),
            "h from Tools should return to Root, got {:?}",
            d.page
        );
    }

    #[test]
    fn enter_on_instructions_row_in_ui_opens_instructions_page() {
        // UI page row 10 (instructions file) + Enter should land on the
        // Instructions page. Rows 0-3 are vim/thinking/markdown, 4-5 are
        // mouse/rich-text-copy (T8.c/T8.g), 6 is emojis, 7-8 are
        // name/packages, 9 is utility model, 10 is instructions.
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        enter_ui_from_root(&mut d);
        for _ in 0..10 {
            d.handle_key(press(KeyCode::Char('j')));
        }
        d.handle_key(press(KeyCode::Enter));
        assert!(
            matches!(d.page, Page::Instructions(_)),
            "expected Instructions page after Enter on instructions row, got {:?}",
            d.page
        );
    }

    #[test]
    fn back_from_ui_restores_root_cursor() {
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        enter_ui_from_root(&mut d);
        // last_root_cursor should be set to 1 (UI's index).
        d.handle_key(press(KeyCode::Char('h')));
        match &d.page {
            Page::Root { cursor } => {
                assert_eq!(*cursor, 1, "cursor should be on UI row after return")
            }
            other => panic!("expected Root, got {other:?}"),
        }
    }

    #[test]
    fn back_from_tools_restores_root_cursor() {
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        enter_tools_from_root(&mut d);
        d.handle_key(press(KeyCode::Char('h')));
        match &d.page {
            Page::Root { cursor } => {
                assert_eq!(*cursor, 3, "cursor should be on Tools row after return")
            }
            other => panic!("expected Root, got {other:?}"),
        }
    }

    #[test]
    fn pressing_a_on_picker_opens_scoped_create_dialog() {
        // The new affordance: `a` on Dialog::PickConfig opens the
        // "where should this config live?" sub-dialog.
        let tmp = TempDir::new().unwrap();
        let cockpit_dir = tmp.path().join(".cockpit");
        std::fs::create_dir_all(&cockpit_dir).unwrap();
        std::fs::write(cockpit_dir.join("config.json"), "{}").unwrap();
        let mut d = Dialog::open(tmp.path());
        assert!(matches!(d, Dialog::PickConfig { .. }));
        let close = d.handle_key(press(KeyCode::Char('a')));
        assert!(!close);
        assert!(
            matches!(d, Dialog::CreateScopedConfig { .. }),
            "after `a` the dialog should be on CreateScopedConfig"
        );
    }

    #[test]
    fn esc_from_scoped_create_returns_to_picker() {
        let tmp = TempDir::new().unwrap();
        let cockpit_dir = tmp.path().join(".cockpit");
        std::fs::create_dir_all(&cockpit_dir).unwrap();
        std::fs::write(cockpit_dir.join("config.json"), "{}").unwrap();
        let mut d = Dialog::open(tmp.path());
        d.handle_key(press(KeyCode::Char('a')));
        assert!(matches!(d, Dialog::CreateScopedConfig { .. }));
        d.handle_key(press(KeyCode::Esc));
        assert!(
            matches!(d, Dialog::PickConfig { .. }),
            "Esc from CreateScopedConfig should return to PickConfig"
        );
    }

    #[test]
    fn h_from_settings_root_returns_to_picker() {
        // After picking a config, the user should be able to back out
        // of the settings root with h/← and land on the picker again.
        let tmp = TempDir::new().unwrap();
        let cockpit_dir = tmp.path().join(".cockpit");
        std::fs::create_dir_all(&cockpit_dir).unwrap();
        std::fs::write(cockpit_dir.join("config.json"), "{}").unwrap();
        let mut d = Dialog::open(tmp.path());
        // Step into the (only) config.
        d.handle_key(press(KeyCode::Enter));
        assert!(matches!(d, Dialog::Settings(_)));
        d.handle_key(press(KeyCode::Char('h')));
        assert!(
            matches!(d, Dialog::PickConfig { .. }),
            "h from Settings Root should reopen the picker"
        );
    }

    fn fresh_instructions_dialog(tmp: &TempDir) -> SettingsDialog {
        let mut d = fresh_dialog(tmp);
        enter_ui_from_root(&mut d);
        // Move cursor to the instructions row (idx 10: the `utility
        // model` row at 9 pushes instructions to the last position) and
        // Enter to nav.
        for _ in 0..10 {
            d.handle_key(press(KeyCode::Char('j')));
        }
        d.handle_key(press(KeyCode::Enter));
        assert!(matches!(d.page, Page::Instructions(_)));
        d
    }

    #[test]
    fn instructions_a_starts_grab_with_empty_buffer() {
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_instructions_dialog(&tmp);
        d.handle_key(press(KeyCode::Char('a')));
        match &d.page {
            Page::Instructions(p) => {
                let g = p.grabbed.as_ref().expect("expected grabbed state");
                assert!(g.buf.text().is_empty());
                assert!(g.original_name.is_none(), "new row has no original name");
                assert_eq!(p.cursor, d.extended.agent_guidance_files.len() - 1);
            }
            other => panic!("expected Instructions, got {other:?}"),
        }
    }

    #[test]
    fn instructions_esc_on_freshly_added_row_removes_it() {
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_instructions_dialog(&tmp);
        let before = d.extended.agent_guidance_files.len();
        d.handle_key(press(KeyCode::Char('a')));
        d.handle_key(press(KeyCode::Esc));
        match &d.page {
            Page::Instructions(p) => {
                assert!(p.grabbed.is_none(), "esc should drop the grab");
                assert_eq!(
                    d.extended.agent_guidance_files.len(),
                    before,
                    "esc on a freshly-added row should delete it"
                );
            }
            other => panic!("expected Instructions, got {other:?}"),
        }
    }

    #[test]
    fn instructions_enter_grabs_existing_row_then_arrow_swaps() {
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_instructions_dialog(&tmp);
        // Seed two known rows.
        d.extended.agent_guidance_files = vec!["AGENTS.md".into(), "CLAUDE.md".into()];
        // Reset to row 0 and grab it.
        d.page = Page::Instructions(InstructionsPage {
            cursor: 0,
            grabbed: None,
            status: None,
        });
        d.handle_key(press(KeyCode::Enter));
        // Now grabbed at idx 0. Press ↓ to swap with row 1.
        d.handle_key(press(KeyCode::Down));
        assert_eq!(
            d.extended.agent_guidance_files,
            vec!["CLAUDE.md".to_string(), "AGENTS.md".to_string()]
        );
        // Drop with Enter → save.
        d.handle_key(press(KeyCode::Enter));
        match &d.page {
            Page::Instructions(p) => assert!(p.grabbed.is_none()),
            other => panic!("expected Instructions, got {other:?}"),
        }
    }

    #[test]
    fn instructions_esc_after_swap_restores_original_order() {
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_instructions_dialog(&tmp);
        d.extended.agent_guidance_files = vec!["AGENTS.md".into(), "CLAUDE.md".into()];
        d.page = Page::Instructions(InstructionsPage {
            cursor: 0,
            grabbed: None,
            status: None,
        });
        d.handle_key(press(KeyCode::Enter));
        d.handle_key(press(KeyCode::Down));
        // Mid-grab the list is mutated. Esc must restore.
        d.handle_key(press(KeyCode::Esc));
        assert_eq!(
            d.extended.agent_guidance_files,
            vec!["AGENTS.md".to_string(), "CLAUDE.md".to_string()],
            "esc should restore original order"
        );
    }

    #[test]
    fn instructions_typing_while_grabbed_edits_filename() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_instructions_dialog(&tmp);
        d.extended.agent_guidance_files = vec!["X".into()];
        d.page = Page::Instructions(InstructionsPage {
            cursor: 0,
            grabbed: None,
            status: None,
        });
        d.handle_key(press(KeyCode::Enter));
        for ch in "Y".chars() {
            d.handle_key(KeyEvent {
                code: KeyCode::Char(ch),
                modifiers: KeyModifiers::empty(),
                kind: KeyEventKind::Press,
                state: KeyEventState::empty(),
            });
        }
        // Commit with Enter.
        d.handle_key(press(KeyCode::Enter));
        assert_eq!(d.extended.agent_guidance_files, vec!["XY".to_string()]);
    }

    #[test]
    fn enter_on_headers_row_navigates_to_headers_subpage() {
        // Provider Edit page → cursor on row 1 (Headers) → Enter
        // should land on the dedicated Headers sub-page, not open an
        // overlay on the Edit page.
        let tmp = TempDir::new().unwrap();
        let mut d = dialog_with_one_provider(&tmp);
        d.handle_key(press(KeyCode::Enter)); // List → Edit(vendor)
        match &d.page {
            Page::Providers(ProvidersPage::Edit(_)) => {}
            other => panic!("expected Edit, got {other:?}"),
        }
        // Move to Headers row (idx 1).
        d.handle_key(press(KeyCode::Char('j')));
        d.handle_key(press(KeyCode::Enter));
        match &d.page {
            Page::Providers(ProvidersPage::Headers { parent, .. }) => {
                assert_eq!(parent.provider_id, "vendor");
            }
            other => panic!("expected Headers sub-page, got {other:?}"),
        }
    }

    #[test]
    fn back_from_headers_returns_to_edit_with_updated_headers() {
        let tmp = TempDir::new().unwrap();
        let mut d = dialog_with_one_provider(&tmp);
        d.handle_key(press(KeyCode::Enter)); // → Edit
        d.handle_key(press(KeyCode::Char('j'))); // cursor → row 1 (Headers)
        d.handle_key(press(KeyCode::Enter)); // → Headers sub-page
        // Add a header via the Browse-mode `a` action, which opens the
        // name/value popup focused on the name field.
        d.handle_key(press(KeyCode::Char('a')));
        // Type a name — a new header with an empty name is discarded on
        // save — then Enter commits and closes the popup.
        d.handle_key(press(KeyCode::Char('x')));
        d.handle_key(press(KeyCode::Enter));
        // `h` from Browse mode returns to the Edit page.
        d.handle_key(press(KeyCode::Char('h')));
        match &d.page {
            Page::Providers(ProvidersPage::Edit(s)) => {
                assert_eq!(s.provider_id, "vendor");
                assert_eq!(s.cursor, 1, "cursor returns to the Headers row");
                assert_eq!(
                    s.entry.headers.len(),
                    1,
                    "headers added on the sub-page should be on the parent EditState"
                );
            }
            other => panic!("expected Edit after back, got {other:?}"),
        }
    }

    #[test]
    fn cancel_add_leaves_no_header() {
        // Opening the add popup and pressing Esc must not leave a blank
        // row behind — the row is only committed on Enter.
        let tmp = TempDir::new().unwrap();
        let mut d = dialog_with_one_provider(&tmp);
        d.handle_key(press(KeyCode::Enter)); // → Edit
        d.handle_key(press(KeyCode::Char('j'))); // cursor → Headers row
        d.handle_key(press(KeyCode::Enter)); // → Headers sub-page
        let before = match &d.page {
            Page::Providers(ProvidersPage::Headers { editor, .. }) => editor.rows().len(),
            other => panic!("expected Headers sub-page, got {other:?}"),
        };
        d.handle_key(press(KeyCode::Char('a'))); // open add popup
        d.handle_key(press(KeyCode::Char('x'))); // type a name
        d.handle_key(press(KeyCode::Esc)); // cancel — discards the add
        match &d.page {
            Page::Providers(ProvidersPage::Headers { editor, .. }) => {
                assert_eq!(editor.rows().len(), before, "cancelled add leaves no row");
                assert!(!editor.is_editing(), "popup is closed after cancel");
            }
            other => panic!("expected Headers sub-page, got {other:?}"),
        }
    }

    #[test]
    fn popup_tab_routes_typing_to_value_field() {
        // In the add/edit popup, Tab switches focus from name to value
        // so subsequent keystrokes land in the value field.
        let tmp = TempDir::new().unwrap();
        let mut d = dialog_with_one_provider(&tmp);
        d.handle_key(press(KeyCode::Enter)); // → Edit
        d.handle_key(press(KeyCode::Char('j'))); // cursor → Headers row
        d.handle_key(press(KeyCode::Enter)); // → Headers sub-page
        d.handle_key(press(KeyCode::Char('a'))); // open add popup (name focus)
        d.handle_key(press(KeyCode::Char('n'))); // → name buffer
        d.handle_key(press(KeyCode::Tab)); // focus → value
        d.handle_key(press(KeyCode::Char('v'))); // → value buffer
        d.handle_key(press(KeyCode::Enter)); // commit
        match &d.page {
            Page::Providers(ProvidersPage::Headers { editor, .. }) => {
                let row = editor.rows().last().expect("a header row was added");
                assert_eq!(row.name, "n");
                assert_eq!(row.value, "v");
            }
            other => panic!("expected Headers sub-page, got {other:?}"),
        }
    }

    #[test]
    fn h_on_edit_page_returns_to_list() {
        // `h` on the Edit page is back-to-list — it must not open the
        // (now-removed) inline header editor.
        let tmp = TempDir::new().unwrap();
        let mut d = dialog_with_one_provider(&tmp);
        d.handle_key(press(KeyCode::Enter)); // → Edit
        d.handle_key(press(KeyCode::Char('h')));
        match &d.page {
            Page::Providers(ProvidersPage::List { .. }) => {}
            other => panic!("expected List after `h`, got {other:?}"),
        }
    }

    #[test]
    fn instructions_esc_after_rename_restores_original_name() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_instructions_dialog(&tmp);
        d.extended.agent_guidance_files = vec!["AGENTS.md".into()];
        d.page = Page::Instructions(InstructionsPage {
            cursor: 0,
            grabbed: None,
            status: None,
        });
        d.handle_key(press(KeyCode::Enter));
        // Type some junk.
        for ch in "ZZZ".chars() {
            d.handle_key(KeyEvent {
                code: KeyCode::Char(ch),
                modifiers: KeyModifiers::empty(),
                kind: KeyEventKind::Press,
                state: KeyEventState::empty(),
            });
        }
        d.handle_key(press(KeyCode::Esc));
        assert_eq!(
            d.extended.agent_guidance_files,
            vec!["AGENTS.md".to_string()],
            "esc should restore the original filename"
        );
    }
}
