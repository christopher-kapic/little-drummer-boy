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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::config::dirs::{
    ConfigDir, ConfigDirKind, creatable_config_dirs, discover_config_dirs, scaffold_config_dir,
};
use crate::config::providers::{
    ConfigDoc, HeaderSpec, OnUnlistedModelsFetch, ProviderEntry, ProvidersConfig,
};
use crate::envref;
use crate::providers::models_fetch::{self, FetchOutcome};
use crate::providers::{self as templates, ProviderTemplate};
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
    },
    CreateConfig {
        choices: Vec<ConfigDir>,
        cursor: usize,
    },
    Settings(SettingsDialog),
}

pub struct SettingsDialog {
    pub config_path: PathBuf,
    page: Page,
    /// Cached config state; reloaded on entry into the Providers list
    /// and after each successful save.
    config: ProvidersConfig,
}

enum Page {
    Root { cursor: usize },
    Agents,
    Tools,
    Providers(ProvidersPage),
}

enum ProvidersPage {
    /// Top-level list of configured providers + the "add new" affordance.
    List {
        cursor: usize,
        status: Option<String>,
    },
    /// Add-provider wizard.
    Add(AddState),
    /// Edit a specific provider.
    Edit(EditState),
    /// Triggered by /fetch-models — prompts on unlisted models.
    FetchAll(FetchAllState),
}

struct AddState {
    step: AddStep,
    template: Option<&'static ProviderTemplate>,
    id_field: TextField,
    url_field: TextField,
    headers: HeaderEditor,
    /// Active OAuth device-flow attempt, when the picked template uses
    /// `AuthKind::DeviceFlow`. Replaces the URL/Headers steps for
    /// those templates.
    codex_login: Option<CodexLoginState>,
    error: Option<String>,
    fetch: Option<FetchHandle>,
    saved_provider_id: Option<String>,
}

enum AddStep {
    /// Pick from `templates::TEMPLATES`. The user spec says
    /// `openai-compatible` is the first/default choice.
    PickTemplate { cursor: usize },
    /// Set the provider id (config-map key). Pre-filled from template.
    EditId,
    /// Set the base URL.
    EditUrl,
    /// Add/remove HTTP headers (`Authorization: Bearer $TOKEN`, etc.).
    EditHeaders,
    /// Run the codex device-code OAuth flow. Lives in `s.codex_login`.
    /// Reached directly after EditId when the template's auth is
    /// `DeviceFlow`.
    CodexLogin,
    /// Saving config + kicking off /models fetch.
    Saving,
    /// Background fetch is in flight.
    Fetching,
    /// Fetch finished (success or error); user must press Enter to return.
    Done,
}

struct EditState {
    provider_id: String,
    entry: ProviderEntry,
    /// Index into [`edit_menu_rows`].
    cursor: usize,
    editing_field: Option<EditField>,
    field_buf: TextField,
    status: Option<String>,
    fetch: Option<FetchHandle>,
    delete_pending: bool,
    /// `Some(...)` when the user opens the headers sub-editor with `h`.
    headers_editor: Option<HeaderEditor>,
}

#[derive(Copy, Clone)]
enum EditField {
    Url,
}

/// Multi-row header list with inline editing.
///
/// Layout (visible "rows" cursor can land on):
///   - 0..n               actual header rows
///   - n                  `[+ add header]`
///   - n+1                `[continue →]` (used by the Add wizard)
///
/// In Browse mode the cursor selects a row; in `EditName(i)` /
/// `EditValue(i)` keystrokes go to the matching field. Tab toggles
/// between name and value while editing.
struct HeaderEditor {
    rows: Vec<HeaderSpec>,
    cursor: usize,
    mode: HeaderMode,
    name_buf: TextField,
    value_buf: TextField,
    /// If false, the synthetic `[continue →]` row is suppressed (used
    /// from the Edit page, where there's no next step).
    show_continue: bool,
}

enum HeaderMode {
    Browse,
    EditName(usize),
    EditValue(usize),
}

enum HeaderResult {
    Stay,
    Continue,
    Back,
}

impl HeaderEditor {
    fn new(rows: Vec<HeaderSpec>, show_continue: bool) -> Self {
        Self {
            rows,
            cursor: 0,
            mode: HeaderMode::Browse,
            name_buf: TextField::default(),
            value_buf: TextField::default(),
            show_continue,
        }
    }

    fn n_rows(&self) -> usize {
        self.rows.len()
    }

    fn add_row_idx(&self) -> usize {
        self.n_rows()
    }

    fn continue_idx(&self) -> Option<usize> {
        if self.show_continue {
            Some(self.n_rows() + 1)
        } else {
            None
        }
    }

    fn max_cursor(&self) -> usize {
        self.continue_idx().unwrap_or(self.add_row_idx())
    }

    fn commit_edit(&mut self) {
        match self.mode {
            HeaderMode::EditName(i) => {
                if let Some(row) = self.rows.get_mut(i) {
                    row.name = self.name_buf.text().to_string();
                }
            }
            HeaderMode::EditValue(i) => {
                if let Some(row) = self.rows.get_mut(i) {
                    row.value = self.value_buf.text().to_string();
                }
            }
            HeaderMode::Browse => {}
        }
    }

    fn start_edit(&mut self, i: usize) {
        if let Some(row) = self.rows.get(i) {
            self.name_buf = TextField::new(row.name.clone());
            self.value_buf = TextField::new(row.value.clone());
            // Start on the value (the field the user usually wants to edit).
            self.mode = HeaderMode::EditValue(i);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> HeaderResult {
        match &mut self.mode {
            HeaderMode::Browse => self.handle_browse_key(key),
            HeaderMode::EditName(_) | HeaderMode::EditValue(_) => self.handle_edit_key(key),
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> HeaderResult {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                HeaderResult::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(self.max_cursor());
                HeaderResult::Stay
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                HeaderResult::Back
            }
            KeyCode::Char('a') => {
                self.rows.push(HeaderSpec {
                    name: String::new(),
                    value: String::new(),
                });
                let i = self.rows.len() - 1;
                self.cursor = i;
                self.name_buf = TextField::default();
                self.value_buf = TextField::default();
                self.mode = HeaderMode::EditName(i);
                HeaderResult::Stay
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if self.cursor < self.rows.len() {
                    self.rows.remove(self.cursor);
                    if self.cursor > 0 && self.cursor >= self.rows.len() {
                        self.cursor -= 1;
                    }
                }
                HeaderResult::Stay
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if self.cursor < self.rows.len() {
                    self.start_edit(self.cursor);
                    HeaderResult::Stay
                } else if self.cursor == self.add_row_idx() {
                    // [+ add header]
                    self.rows.push(HeaderSpec {
                        name: String::new(),
                        value: String::new(),
                    });
                    let i = self.rows.len() - 1;
                    self.cursor = i;
                    self.name_buf = TextField::default();
                    self.value_buf = TextField::default();
                    self.mode = HeaderMode::EditName(i);
                    HeaderResult::Stay
                } else if Some(self.cursor) == self.continue_idx() {
                    HeaderResult::Continue
                } else {
                    HeaderResult::Stay
                }
            }
            _ => HeaderResult::Stay,
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) -> HeaderResult {
        match key.code {
            KeyCode::Esc => {
                // Cancel the in-flight edit by reverting the buffers
                // (committed values came from the row originally, so
                // just exit Browse without commit).
                self.mode = HeaderMode::Browse;
                HeaderResult::Stay
            }
            KeyCode::Tab => {
                self.commit_edit();
                self.mode = match self.mode {
                    HeaderMode::EditName(i) => HeaderMode::EditValue(i),
                    HeaderMode::EditValue(i) => HeaderMode::EditName(i),
                    HeaderMode::Browse => HeaderMode::Browse,
                };
                HeaderResult::Stay
            }
            KeyCode::Enter => {
                self.commit_edit();
                self.mode = HeaderMode::Browse;
                HeaderResult::Stay
            }
            _ => {
                match self.mode {
                    HeaderMode::EditName(_) => {
                        self.name_buf.handle_key(key);
                    }
                    HeaderMode::EditValue(_) => {
                        self.value_buf.handle_key(key);
                    }
                    HeaderMode::Browse => {}
                }
                HeaderResult::Stay
            }
        }
    }

    fn rows(&self) -> &[HeaderSpec] {
        &self.rows
    }

    fn is_editing(&self) -> bool {
        !matches!(self.mode, HeaderMode::Browse)
    }
}

struct FetchAllState {
    providers: Vec<String>,
    in_flight: Vec<FetchHandle>,
    finished: Vec<FetchedSummary>,
    /// 0 = Keep (default), 1 = Remove, 2 = Save & close
    cursor: usize,
    dont_ask_again: bool,
    /// Aggregated set of (provider_id, missing_model_id) the user must rule on.
    unlisted: Vec<(String, String)>,
}

struct FetchedSummary {
    provider_id: String,
    outcome: Result<FetchOutcome, String>,
}

/// Navigation intent returned by an inner page handler. The outer
/// [`SettingsDialog::handle_providers_key`] applies it *after* swapping
/// the borrowed sub-page back in. Inner handlers cannot write
/// `self.page` directly — the swap-back would discard the write.
enum Nav {
    /// Stay on the current page; sub-state mutations have already been
    /// applied to the borrowed `&mut SubState`.
    Stay,
    /// Navigate to `Page::...`.
    Replace(Page),
    /// Close the whole dialog.
    Close,
}

/// Codex device-code OAuth login state, shared between the dialog's
/// render path and the background task driving the flow.
pub struct CodexLoginState {
    shared: Arc<Mutex<CodexLoginProgress>>,
}

#[derive(Clone)]
pub enum CodexLoginProgress {
    /// POSTing to the usercode endpoint.
    Requesting,
    /// Server returned a user code; waiting for the user to enter it
    /// in a browser and for the poll loop to receive an authorization
    /// code.
    AwaitingUser {
        verification_url: String,
        user_code: String,
    },
    /// Persisted; the dialog can finalize the ProviderEntry.
    Success {
        saved_at: chrono::DateTime<chrono::Utc>,
    },
    /// Flow failed at any step. The dialog should show the message
    /// and let the user retry.
    Error(String),
}

impl CodexLoginState {
    pub fn spawn() -> Self {
        let cfg = crate::auth::codex::LoginConfig::default();
        Self::spawn_with(cfg)
    }

    pub fn spawn_with(cfg: crate::auth::codex::LoginConfig) -> Self {
        let shared = Arc::new(Mutex::new(CodexLoginProgress::Requesting));
        let w = Arc::clone(&shared);
        tokio::spawn(async move {
            match crate::auth::codex::request_device_code(&cfg).await {
                Err(e) => set(&w, CodexLoginProgress::Error(e.to_string())),
                Ok(device) => {
                    set(
                        &w,
                        CodexLoginProgress::AwaitingUser {
                            verification_url: device.verification_url.clone(),
                            user_code: device.user_code.clone(),
                        },
                    );
                    match crate::auth::codex::complete_login(&cfg, &device).await {
                        Err(e) => set(&w, CodexLoginProgress::Error(e.to_string())),
                        Ok(stored) => set(
                            &w,
                            CodexLoginProgress::Success {
                                saved_at: stored.saved_at,
                            },
                        ),
                    }
                }
            }
        });
        Self { shared }
    }

    pub fn snapshot(&self) -> CodexLoginProgress {
        self.shared
            .lock()
            .map(|g| g.clone())
            .unwrap_or(CodexLoginProgress::Error("poisoned login state".into()))
    }
}

fn set(shared: &Arc<Mutex<CodexLoginProgress>>, value: CodexLoginProgress) {
    if let Ok(mut g) = shared.lock() {
        *g = value;
    }
}

/// Shared cell for an in-flight `/models` fetch. The background task
/// writes the result; the event loop polls it on each tick.
#[derive(Clone)]
pub struct FetchHandle {
    pub provider_id: String,
    pub state: Arc<Mutex<FetchState>>,
}

pub enum FetchState {
    Running,
    Done(Result<FetchOutcome, String>),
    /// Consumed already — left as a terminal marker so the dialog
    /// doesn't double-apply the result.
    Consumed,
}

impl FetchHandle {
    pub fn spawn(provider_id: String, url: String, headers: Vec<HeaderSpec>) -> Self {
        let state = Arc::new(Mutex::new(FetchState::Running));
        let state_w = Arc::clone(&state);
        tokio::spawn(async move {
            let result = models_fetch::fetch_models(&url, &headers, Some(Duration::from_secs(15)))
                .await
                .map_err(|e| e.to_string());
            if let Ok(mut s) = state_w.lock() {
                *s = FetchState::Done(result);
            }
        });
        Self { provider_id, state }
    }

    pub fn take(&self) -> Option<Result<FetchOutcome, String>> {
        let mut s = self.state.lock().ok()?;
        match std::mem::replace(&mut *s, FetchState::Consumed) {
            FetchState::Running => {
                *s = FetchState::Running;
                None
            }
            FetchState::Done(r) => Some(r),
            FetchState::Consumed => None,
        }
    }
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
            }
        } else {
            Dialog::PickConfig { dirs, cursor: 0 }
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
            d = Dialog::Settings(SettingsDialog::open(path));
            if let Dialog::Settings(s) = &mut d {
                s.enter_providers();
            }
        }
        d
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
            Dialog::PickConfig { dirs, cursor } => match list_key_action(key, cursor, dirs.len()) {
                ListAction::Stay => false,
                ListAction::Close => true,
                ListAction::Select(idx) => {
                    let chosen = dirs[idx].path.join("config.json");
                    *self = Dialog::Settings(SettingsDialog::open(chosen));
                    false
                }
            },
            Dialog::CreateConfig { choices, cursor } => {
                match list_key_action(key, cursor, choices.len()) {
                    ListAction::Stay => false,
                    ListAction::Close => true,
                    ListAction::Select(idx) => match scaffold_config_dir(&choices[idx].path) {
                        Ok(config_path) => {
                            *self = Dialog::Settings(SettingsDialog::open(config_path));
                            false
                        }
                        Err(_) => true,
                    },
                }
            }
            Dialog::Settings(s) => s.handle_key(key),
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
        Self {
            config_path,
            page: Page::Root { cursor: 0 },
            config,
        }
    }

    fn enter_providers(&mut self) {
        self.page = Page::Providers(ProvidersPage::List {
            cursor: 0,
            status: None,
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
        // Drain finished fetches into config.
        let pending = match &mut self.page {
            Page::Providers(ProvidersPage::Add(s)) => s.fetch.clone(),
            Page::Providers(ProvidersPage::Edit(s)) => s.fetch.clone(),
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

    /// If the Add wizard is on the CodexLogin step and the device-flow
    /// background task has finished, finalize the provider entry.
    fn advance_codex_login(&mut self) {
        let Page::Providers(ProvidersPage::Add(s)) = &mut self.page else {
            return;
        };
        if !matches!(s.step, AddStep::CodexLogin) {
            return;
        }
        let snap = match &s.codex_login {
            Some(c) => c.snapshot(),
            None => return,
        };
        match snap {
            CodexLoginProgress::Success { saved_at } => {
                let template = s.template.expect("template chosen");
                let id = s.id_field.text().trim().to_string();
                let entry = ProviderEntry {
                    name: Some(template.display.to_string()),
                    url: template.url.trim_end_matches('/').to_string(),
                    headers: Vec::new(),
                    models_fetched_at: None,
                    favorite: None,
                    credential_ref: Some("codex".to_string()),
                    auth: Some(crate::config::providers::AuthKind::DeviceFlow),
                    models: Vec::new(),
                };
                self.config.providers.insert(id.clone(), entry);
                let msg = match self.save_config() {
                    Ok(()) => format!(
                        "codex: logged in (saved {}); provider `{id}` added",
                        saved_at.format("%Y-%m-%d %H:%M UTC")
                    ),
                    Err(e) => format!("codex: logged in but config write failed: {e}"),
                };
                if let Page::Providers(ProvidersPage::Add(s)) = &mut self.page {
                    s.error = Some(msg);
                    s.codex_login = None;
                    s.step = AddStep::Done;
                }
            }
            CodexLoginProgress::Error(_)
            | CodexLoginProgress::Requesting
            | CodexLoginProgress::AwaitingUser { .. } => {
                // Nothing to advance yet; the renderer reads the
                // snapshot on its own.
            }
        }
    }

    fn apply_fetch_result(&mut self, provider_id: &str, result: Result<FetchOutcome, String>) {
        let mut message = String::new();
        if let Some(entry) = self.config.providers.get_mut(provider_id) {
            match result {
                Ok(FetchOutcome::Models(models)) => {
                    entry.models = models;
                    entry.models_fetched_at = Some(Utc::now());
                    message = format!("fetched {} model(s) from /models", entry.models.len());
                }
                Ok(FetchOutcome::Unsupported) => {
                    message = "provider has no /models endpoint (skipped)".to_string();
                }
                Err(e) => {
                    message = format!("fetch failed: {e}");
                }
            }
        }
        let _ = self.save_config();

        match &mut self.page {
            Page::Providers(ProvidersPage::Add(s)) => {
                s.error = Some(message);
                s.fetch = None;
                s.step = AddStep::Done;
            }
            Page::Providers(ProvidersPage::Edit(s)) => {
                s.status = Some(message);
                s.fetch = None;
                // refresh the entry view
                if let Some(entry) = self.config.providers.get(provider_id) {
                    s.entry = entry.clone();
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
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
            Page::Agents | Page::Tools => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace
                ) {
                    self.page = Page::Root { cursor: 0 };
                    false
                } else if matches!(key.code, KeyCode::Char('q')) {
                    true
                } else {
                    false
                }
            }
            Page::Providers(_) => self.handle_providers_key(key),
            Page::Root { .. } => unreachable!("handled above"),
        }
    }

    fn handle_root_key(&mut self, key: KeyEvent, mut cursor: usize) -> bool {
        let children = root_nodes();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => {
                cursor = cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                cursor = (cursor + 1).min(children.len().saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let chosen = children.get(cursor).map(|n| n.title).unwrap_or("");
                match chosen {
                    "Providers" => self.enter_providers(),
                    "Agents" => self.page = Page::Agents,
                    "Tools" => self.page = Page::Tools,
                    _ => {}
                }
                return false;
            }
            _ => {}
        }
        self.page = Page::Root { cursor };
        false
    }

    fn handle_providers_key(&mut self, key: KeyEvent) -> bool {
        // Detach the providers page so its `&mut SubState` doesn't alias
        // `&mut self`. Inner handlers communicate navigation via the
        // returned [`Nav`] rather than writing `self.page`, because the
        // swap-back below would otherwise discard those writes.
        let placeholder = Page::Providers(ProvidersPage::List {
            cursor: 0,
            status: None,
        });
        let mut page = std::mem::replace(&mut self.page, placeholder);
        let nav = if let Page::Providers(p) = &mut page {
            self.handle_providers_page_key(key, p)
        } else {
            Nav::Stay
        };
        self.page = page;
        match nav {
            Nav::Stay => false,
            Nav::Replace(new) => {
                self.page = new;
                false
            }
            Nav::Close => true,
        }
    }

    fn handle_providers_page_key(&mut self, key: KeyEvent, page: &mut ProvidersPage) -> Nav {
        match page {
            ProvidersPage::List { cursor, status } => {
                let ids: Vec<String> = self.config.providers.keys().cloned().collect();
                match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                        return Nav::Replace(Page::Root { cursor: 0 });
                    }
                    KeyCode::Char('q') => return Nav::Close,
                    KeyCode::Up | KeyCode::Char('k') => {
                        *cursor = cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *cursor = (*cursor + 1).min(ids.len().saturating_sub(1).max(0));
                    }
                    KeyCode::Char('c') => {
                        return Nav::Replace(Page::Providers(ProvidersPage::Add(AddState::new())));
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        if let Some(id) = ids.get(*cursor).cloned()
                            && let Some(entry) = self.config.providers.get(&id)
                        {
                            return Nav::Replace(Page::Providers(ProvidersPage::Edit(
                                EditState::new(id, entry.clone()),
                            )));
                        }
                    }
                    _ => {}
                }
                *status = None;
                Nav::Stay
            }
            ProvidersPage::Add(state) => self.handle_add_key(key, state),
            ProvidersPage::Edit(state) => self.handle_edit_key(key, state),
            ProvidersPage::FetchAll(state) => self.handle_fetch_all_key(key, state),
        }
    }

    fn handle_add_key(&mut self, key: KeyEvent, s: &mut AddState) -> Nav {
        // Back/escape unconditionally returns to the list.
        if matches!(key.code, KeyCode::Esc) {
            return Nav::Replace(Page::Providers(ProvidersPage::List {
                cursor: 0,
                status: None,
            }));
        }

        match &mut s.step {
            AddStep::PickTemplate { cursor } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    *cursor = cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *cursor = (*cursor + 1).min(templates::TEMPLATES.len() - 1);
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    let t = &templates::TEMPLATES[*cursor];
                    s.template = Some(t);
                    // Pre-fill id only for templates that map 1:1 to a
                    // single vendor; for `openai-compatible` the user
                    // must choose a unique name (they may add several).
                    if t.use_id_as_default {
                        s.id_field.set(t.id);
                    } else {
                        s.id_field.set("");
                    }
                    s.url_field.set(t.url);
                    s.headers = HeaderEditor::new(
                        templates::default_headers_for(t),
                        /* show_continue */ true,
                    );
                    s.step = AddStep::EditId;
                }
                _ => {}
            },
            AddStep::EditId => match key.code {
                KeyCode::Enter => {
                    let id = s.id_field.text().trim().to_string();
                    if id.is_empty() {
                        s.error = Some("id cannot be empty".into());
                    } else if !valid_id(&id) {
                        s.error = Some("id must be lowercase letters, digits, `-`, or `_`".into());
                    } else if self.config.providers.contains_key(&id) {
                        s.error = Some(format!("a provider with id `{id}` already exists"));
                    } else {
                        s.error = None;
                        // Device-flow templates skip URL/Headers — the
                        // OAuth login itself is the configuration.
                        if matches!(
                            s.template.map(|t| t.auth),
                            Some(crate::config::providers::AuthKind::DeviceFlow)
                        ) {
                            s.codex_login = Some(CodexLoginState::spawn());
                            s.step = AddStep::CodexLogin;
                        } else {
                            s.step = AddStep::EditUrl;
                        }
                    }
                }
                _ => {
                    s.id_field.handle_key(key);
                }
            },
            AddStep::EditUrl => match key.code {
                KeyCode::Enter => {
                    if !valid_url(s.url_field.text()) {
                        s.error = Some("url must start with http:// or https://".into());
                    } else {
                        s.error = None;
                        s.step = AddStep::EditHeaders;
                    }
                }
                _ => {
                    s.url_field.handle_key(key);
                }
            },
            AddStep::EditHeaders => {
                match s.headers.handle_key(key) {
                    HeaderResult::Stay => return Nav::Stay,
                    HeaderResult::Back => {
                        s.error = None;
                        s.step = AddStep::EditUrl;
                        return Nav::Stay;
                    }
                    HeaderResult::Continue => {
                        // fall through to the save+fetch block below
                    }
                }

                {
                    // Finalize and kick off the fetch.
                    let template = s.template.expect("template chosen");
                    let id = s.id_field.text().trim().to_string();
                    let headers: Vec<HeaderSpec> = s.headers.rows().to_vec();

                    let entry = ProviderEntry {
                        name: Some(template.display.to_string()),
                        url: s.url_field.text().trim_end_matches('/').to_string(),
                        headers: headers.clone(),
                        models_fetched_at: None,
                        favorite: None,
                        credential_ref: None,
                        auth: Some(template.auth),
                        models: vec![],
                    };

                    self.config.providers.insert(id.clone(), entry.clone());
                    match self.save_config() {
                        Ok(()) => {
                            s.saved_provider_id = Some(id.clone());
                            s.error = Some("saved. Fetching /models…".into());
                            // Don't fetch if env vars are missing — surface the warning.
                            let (_, missing) = models_fetch::resolve_headers(&headers);
                            if !missing.is_empty() {
                                s.error = Some(format!(
                                    "saved. /models fetch skipped — missing env var(s): {}",
                                    missing.join(", ")
                                ));
                                s.step = AddStep::Done;
                            } else if template.supports_models_endpoint {
                                s.fetch = Some(FetchHandle::spawn(id, entry.url.clone(), headers));
                                s.step = AddStep::Fetching;
                            } else {
                                s.error = Some("saved. provider has no /models endpoint".into());
                                s.step = AddStep::Done;
                            }
                        }
                        Err(e) => {
                            s.error = Some(format!("save failed: {e}"));
                        }
                    }
                }
            }
            AddStep::CodexLogin => {
                // Input handling for the device-code screen. Most
                // movement is automatic (driven by the background
                // task via `tick`); the user can press `r` to retry
                // after an error.
                let snap = s
                    .codex_login
                    .as_ref()
                    .map(|c| c.snapshot())
                    .unwrap_or(CodexLoginProgress::Error("no login state".into()));
                match key.code {
                    KeyCode::Char('r') if matches!(snap, CodexLoginProgress::Error(_)) => {
                        s.codex_login = Some(CodexLoginState::spawn());
                    }
                    _ => {}
                }
            }
            AddStep::Saving | AddStep::Fetching => {
                // Disable input while in-flight, except Esc (handled above).
            }
            AddStep::Done => {
                if matches!(key.code, KeyCode::Enter) {
                    return Nav::Replace(Page::Providers(ProvidersPage::List {
                        cursor: 0,
                        status: s.error.clone(),
                    }));
                }
            }
        }
        Nav::Stay
    }

    fn handle_edit_key(&mut self, key: KeyEvent, s: &mut EditState) -> Nav {
        // Headers sub-editor open: all keys go there until it signals Back.
        if let Some(editor) = s.headers_editor.as_mut() {
            match editor.handle_key(key) {
                HeaderResult::Stay | HeaderResult::Continue => {
                    return Nav::Stay;
                }
                HeaderResult::Back => {
                    if let Some(editor) = s.headers_editor.take() {
                        s.entry.headers = editor.rows;
                        s.status = Some("headers updated; press s to save".into());
                    }
                    return Nav::Stay;
                }
            }
        }

        // Inline-edit mode: keystrokes go to the field until Enter/Esc.
        if let Some(field) = s.editing_field {
            match key.code {
                KeyCode::Enter => {
                    let new = s.field_buf.text().to_string();
                    match field {
                        EditField::Url => {
                            if valid_url(&new) {
                                s.entry.url = new.trim_end_matches('/').to_string();
                                s.status = Some("url updated; press s to save".into());
                            } else {
                                s.status = Some("url must start with http:// or https://".into());
                                return Nav::Stay;
                            }
                        }
                    }
                    s.editing_field = None;
                }
                KeyCode::Esc => {
                    s.editing_field = None;
                }
                _ => {
                    s.field_buf.handle_key(key);
                }
            }
            return Nav::Stay;
        }

        // Action menu. Note: vim `h` is repurposed here as "open headers
        // editor"; ← and Backspace still go back to the list.
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                return Nav::Replace(Page::Providers(ProvidersPage::List {
                    cursor: 0,
                    status: s.status.clone(),
                }));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                s.cursor = s.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                s.cursor = (s.cursor + 1).min(EDIT_MENU_LEN - 1);
            }
            KeyCode::Char('h') => {
                s.headers_editor = Some(HeaderEditor::new(
                    s.entry.headers.clone(),
                    /* show_continue */ false,
                ));
                return Nav::Stay;
            }
            KeyCode::Char('s') => {
                self.config
                    .providers
                    .insert(s.provider_id.clone(), s.entry.clone());
                match self.save_config() {
                    Ok(()) => s.status = Some("saved".into()),
                    Err(e) => s.status = Some(format!("save failed: {e}")),
                }
            }
            KeyCode::Char('r') => {
                let (_, missing) = models_fetch::resolve_headers(&s.entry.headers);
                if !missing.is_empty() {
                    s.status = Some(format!(
                        "refetch skipped — missing env var(s): {}",
                        missing.join(", ")
                    ));
                } else {
                    s.fetch = Some(FetchHandle::spawn(
                        s.provider_id.clone(),
                        s.entry.url.clone(),
                        s.entry.headers.clone(),
                    ));
                    s.status = Some("refetching /models…".into());
                }
            }
            KeyCode::Char('f') => {
                let new = !s.entry.favorite.unwrap_or(false);
                s.entry.favorite = if new { Some(true) } else { None };
                s.status = Some(if new {
                    "marked as favorite".into()
                } else {
                    "removed favorite".into()
                });
            }
            KeyCode::Char('d') => {
                if s.delete_pending {
                    self.config.providers.remove(&s.provider_id);
                    let saved = self.save_config();
                    let msg = match saved {
                        Ok(()) => format!("deleted `{}`", s.provider_id),
                        Err(e) => format!("delete failed: {e}"),
                    };
                    return Nav::Replace(Page::Providers(ProvidersPage::List {
                        cursor: 0,
                        status: Some(msg),
                    }));
                } else {
                    s.delete_pending = true;
                    s.status = Some("press d again to confirm delete".into());
                }
                return Nav::Stay;
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                match s.cursor {
                    0 => {
                        s.field_buf = TextField::new(s.entry.url.clone());
                        s.editing_field = Some(EditField::Url);
                    }
                    1 => {
                        s.headers_editor = Some(HeaderEditor::new(
                            s.entry.headers.clone(),
                            /* show_continue */ false,
                        ));
                    }
                    2 => {
                        let new = !s.entry.favorite.unwrap_or(false);
                        s.entry.favorite = if new { Some(true) } else { None };
                        s.status = Some(if new {
                            "marked as favorite".into()
                        } else {
                            "removed favorite".into()
                        });
                    }
                    3 => {
                        // Same as 'r'
                        let (_, missing) = models_fetch::resolve_headers(&s.entry.headers);
                        if !missing.is_empty() {
                            s.status = Some(format!(
                                "refetch skipped — missing env var(s): {}",
                                missing.join(", ")
                            ));
                        } else {
                            s.fetch = Some(FetchHandle::spawn(
                                s.provider_id.clone(),
                                s.entry.url.clone(),
                                s.entry.headers.clone(),
                            ));
                            s.status = Some("refetching /models…".into());
                        }
                    }
                    4 => {
                        if s.delete_pending {
                            self.config.providers.remove(&s.provider_id);
                            let saved = self.save_config();
                            let msg = match saved {
                                Ok(()) => format!("deleted `{}`", s.provider_id),
                                Err(e) => format!("delete failed: {e}"),
                            };
                            return Nav::Replace(Page::Providers(ProvidersPage::List {
                                cursor: 0,
                                status: Some(msg),
                            }));
                        } else {
                            s.delete_pending = true;
                            s.status = Some("press Enter again to confirm delete".into());
                        }
                    }
                    5 => {
                        return Nav::Replace(Page::Providers(ProvidersPage::List {
                            cursor: 0,
                            status: s.status.clone(),
                        }));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        s.delete_pending = matches!(key.code, KeyCode::Char('d')) && s.delete_pending;
        Nav::Stay
    }

    fn handle_fetch_all_key(&mut self, key: KeyEvent, s: &mut FetchAllState) -> Nav {
        match key.code {
            KeyCode::Esc => {
                return Nav::Replace(Page::Providers(ProvidersPage::List {
                    cursor: 0,
                    status: Some("/fetch-models cancelled".into()),
                }));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                s.cursor = s.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                s.cursor = (s.cursor + 1).min(2);
            }
            KeyCode::Char(' ') => {
                if s.cursor == 2 {
                    s.dont_ask_again = !s.dont_ask_again;
                }
            }
            KeyCode::Enter => {
                let pick = match s.cursor {
                    0 => OnUnlistedModelsFetch::Keep,
                    1 => OnUnlistedModelsFetch::Remove,
                    _ => OnUnlistedModelsFetch::Keep,
                };
                if matches!(pick, OnUnlistedModelsFetch::Remove) {
                    for (pid, _mid) in &s.unlisted {
                        if let Some(entry) = self.config.providers.get_mut(pid) {
                            // The fetch already replaced `models`; nothing to do here.
                            let _ = entry;
                        }
                    }
                } else {
                    // Restore unlisted entries — they were dropped by the fetch.
                    // Implementation note: we kept the originals in `finished`.
                    for f in &s.finished {
                        if let Ok(FetchOutcome::Models(_)) = &f.outcome {
                            // already applied; for "keep" we'd merge here.
                            // (Currently the fetch always replaces; merging
                            // is a follow-up.)
                        }
                    }
                }
                if s.dont_ask_again {
                    self.config.on_unlisted_models_fetch = Some(pick);
                }
                let _ = self.save_config();
                return Nav::Replace(Page::Providers(ProvidersPage::List {
                    cursor: 0,
                    status: Some("/fetch-models applied".into()),
                }));
            }
            _ => {}
        }
        Nav::Stay
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
            Page::Tools => {
                render_stub(frame, layout[0], "Tools", TOOLS_STUB);
            }
            Page::Providers(p) => self.render_providers_page(frame, layout[0], p),
        }
        frame.render_widget(help_line(self.help_text()), layout[1]);
    }

    fn title(&self) -> String {
        let crumbs = match &self.page {
            Page::Root { .. } => String::new(),
            Page::Agents => " › Agents".into(),
            Page::Tools => " › Tools".into(),
            Page::Providers(ProvidersPage::List { .. }) => " › Providers".into(),
            Page::Providers(ProvidersPage::Add(_)) => " › Providers › Add".into(),
            Page::Providers(ProvidersPage::Edit(s)) => {
                format!(" › Providers › {}", s.provider_id)
            }
            Page::Providers(ProvidersPage::FetchAll(_)) => " › Providers › /fetch-models".into(),
        };
        format!("{}{}", display_path(&self.config_path), crumbs)
    }

    fn help_text(&self) -> &'static str {
        match &self.page {
            Page::Root { .. } => "↑/↓  enter: open  esc: close",
            Page::Agents | Page::Tools => "←/h/backspace: back  esc: close",
            Page::Providers(ProvidersPage::List { .. }) => {
                "↑/↓  enter: edit  c: add new  ←: back  esc: close"
            }
            Page::Providers(ProvidersPage::Add(s)) => match s.step {
                AddStep::PickTemplate { .. } => "↑/↓  enter: choose  esc: cancel",
                AddStep::EditId | AddStep::EditUrl => "type to edit  enter: next  esc: cancel",
                AddStep::EditHeaders => {
                    if s.headers.is_editing() {
                        "type to edit  Tab: name/value  enter: apply  esc: cancel"
                    } else {
                        "↑/↓  a: add  enter: edit  x: delete  enter on continue: save  esc: back"
                    }
                }
                AddStep::CodexLogin => {
                    "open URL + enter code in browser  r: retry on error  esc: cancel"
                }
                AddStep::Saving | AddStep::Fetching => "(in progress)  esc: cancel",
                AddStep::Done => "enter: back to list",
            },
            Page::Providers(ProvidersPage::Edit(s)) => {
                if s.headers_editor.is_some() {
                    let editing = s
                        .headers_editor
                        .as_ref()
                        .map(HeaderEditor::is_editing)
                        .unwrap_or(false);
                    if editing {
                        "type to edit  Tab: name/value  enter: apply  esc: cancel"
                    } else {
                        "↑/↓  a: add  enter: edit  x: delete  esc: close headers"
                    }
                } else if s.editing_field.is_some() {
                    "type to edit  enter: apply  esc: cancel"
                } else {
                    "↑/↓  enter: edit  h: headers  s: save  r: refetch  f: favorite  d: delete  esc: back"
                }
            }
            Page::Providers(ProvidersPage::FetchAll(_)) => {
                "↑/↓  space: toggle don't-ask  enter: apply  esc: cancel"
            }
        }
    }

    fn render_providers_page(&self, frame: &mut Frame, area: Rect, page: &ProvidersPage) {
        match page {
            ProvidersPage::List { cursor, status } => {
                self.render_providers_list(frame, area, *cursor, status.as_deref())
            }
            ProvidersPage::Add(s) => self.render_add(frame, area, s),
            ProvidersPage::Edit(s) => self.render_edit(frame, area, s),
            ProvidersPage::FetchAll(s) => self.render_fetch_all(frame, area, s),
        }
    }

    fn render_providers_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        cursor: usize,
        status: Option<&str>,
    ) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.config.providers.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no providers configured — press `c` to add one)".to_string(),
                muted,
            )));
        } else {
            let ids: Vec<&String> = self.config.providers.keys().collect();
            let id_w = ids.iter().map(|s| s.chars().count()).max().unwrap_or(0);
            for (i, id) in ids.iter().enumerate() {
                let entry = self.config.providers.get(*id).unwrap();
                let marker = if i == cursor { "▸ " } else { "  " };
                let label = format!("{:<width$}", id, width = id_w);
                let star = if entry.favorite.unwrap_or(false) {
                    " ★"
                } else {
                    "  "
                };
                let style = if i == cursor {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let model_count = format!("{} models", entry.models.len());
                lines.push(Line::from(vec![
                    Span::raw(marker),
                    Span::styled(label, style),
                    Span::raw(star.to_string()),
                    Span::raw("  "),
                    Span::styled(entry.url.clone(), muted),
                    Span::raw("  "),
                    Span::styled(model_count, muted),
                ]));
            }
        }
        if let Some(msg) = status {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                msg.to_string(),
                Style::default().fg(Color::Yellow),
            )));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn render_add(&self, frame: &mut Frame, area: Rect, s: &AddState) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let red = Style::default().fg(Color::Red);
        let mut lines: Vec<Line<'static>> = Vec::new();

        match &s.step {
            AddStep::PickTemplate { cursor } => {
                lines.push(Line::from(Span::styled(
                    "Which provider would you like to add?".to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::default());
                for (i, t) in templates::TEMPLATES.iter().enumerate() {
                    let marker = if i == *cursor { "▸ " } else { "  " };
                    let style = if i == *cursor {
                        yellow.add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    lines.push(Line::from(vec![
                        Span::raw(marker),
                        Span::styled(t.display.to_string(), style),
                        Span::raw("  "),
                        Span::styled(format!("({})", t.id), muted),
                    ]));
                }
                if let Some(t) = templates::TEMPLATES.get(*cursor)
                    && let Some(hint) = t.hint
                {
                    lines.push(Line::default());
                    lines.push(Line::from(Span::styled(hint.to_string(), muted)));
                }
            }
            AddStep::EditId | AddStep::EditUrl | AddStep::EditHeaders => {
                let t = s.template.expect("template chosen");
                lines.push(Line::from(vec![
                    Span::styled("Template: ", muted),
                    Span::styled(t.display.to_string(), Style::default().fg(Color::White)),
                ]));
                lines.push(Line::default());
                render_field_row(
                    &mut lines,
                    "id",
                    &s.id_field,
                    matches!(s.step, AddStep::EditId),
                );
                render_field_row(
                    &mut lines,
                    "url",
                    &s.url_field,
                    matches!(s.step, AddStep::EditUrl),
                );
                if matches!(s.step, AddStep::EditHeaders) {
                    lines.push(Line::default());
                    render_header_editor(&mut lines, &s.headers);
                }
                if matches!(s.step, AddStep::EditUrl)
                    && let Some(hint) = t.hint
                {
                    lines.push(Line::default());
                    lines.push(Line::from(Span::styled(hint.to_string(), muted)));
                }
            }
            AddStep::CodexLogin => {
                let snap = s
                    .codex_login
                    .as_ref()
                    .map(|c| c.snapshot())
                    .unwrap_or(CodexLoginProgress::Error("no login state".into()));
                lines.push(Line::from(Span::styled(
                    "Codex device-code login".to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::default());
                match snap {
                    CodexLoginProgress::Requesting => {
                        lines.push(Line::from(Span::styled(
                            "Requesting a device code from auth.openai.com…".to_string(),
                            yellow,
                        )));
                    }
                    CodexLoginProgress::AwaitingUser {
                        verification_url,
                        user_code,
                    } => {
                        lines.push(Line::from(vec![Span::styled(
                            "1. Open this URL in a browser:".to_string(),
                            muted,
                        )]));
                        lines.push(Line::from(vec![
                            Span::raw("     "),
                            Span::styled(
                                verification_url,
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::UNDERLINED),
                            ),
                        ]));
                        lines.push(Line::default());
                        lines.push(Line::from(Span::styled(
                            "2. Enter this code (expires in 15 minutes):".to_string(),
                            muted,
                        )));
                        lines.push(Line::from(vec![
                            Span::raw("     "),
                            Span::styled(
                                user_code,
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]));
                        lines.push(Line::default());
                        lines.push(Line::from(Span::styled(
                            "Waiting for authorization…".to_string(),
                            muted,
                        )));
                    }
                    CodexLoginProgress::Success { saved_at } => {
                        lines.push(Line::from(Span::styled(
                            format!("Logged in. Tokens saved at {saved_at}."),
                            Style::default().fg(Color::Green),
                        )));
                    }
                    CodexLoginProgress::Error(e) => {
                        lines.push(Line::from(Span::styled(format!("Login failed: {e}"), red)));
                        lines.push(Line::default());
                        lines.push(Line::from(Span::styled(
                            "Press r to retry, esc to cancel.".to_string(),
                            muted,
                        )));
                    }
                }
            }
            AddStep::Saving | AddStep::Fetching => {
                lines.push(Line::from(Span::styled(
                    if matches!(s.step, AddStep::Saving) {
                        "Saving config…"
                    } else {
                        "Fetching /models…"
                    }
                    .to_string(),
                    yellow,
                )));
            }
            AddStep::Done => {
                lines.push(Line::from(Span::styled(
                    "Done.".to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
            }
        }
        if let Some(err) = &s.error {
            lines.push(Line::default());
            let style = if err.contains("failed") {
                red
            } else if err.starts_with("saved") || err.starts_with("Done") {
                muted
            } else {
                yellow
            };
            lines.push(Line::from(Span::styled(err.clone(), style)));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn render_edit(&self, frame: &mut Frame, area: Rect, s: &EditState) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let mut lines: Vec<Line<'static>> = Vec::new();

        lines.push(Line::from(vec![
            Span::styled("Provider: ", muted),
            Span::styled(
                s.provider_id.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                if s.entry.favorite.unwrap_or(false) {
                    "★ favorite"
                } else {
                    ""
                }
                .to_string(),
                yellow,
            ),
        ]));
        lines.push(Line::default());

        let headers_summary = if s.entry.headers.is_empty() {
            "(none)".to_string()
        } else {
            format!(
                "{} header(s): {}",
                s.entry.headers.len(),
                s.entry
                    .headers
                    .iter()
                    .map(|h| h.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let rows = [
            ("URL", s.entry.url.clone()),
            ("Headers", headers_summary),
            (
                "Favorite",
                if s.entry.favorite.unwrap_or(false) {
                    "yes"
                } else {
                    "no"
                }
                .to_string(),
            ),
            (
                "Refetch /models",
                format!(
                    "{} model(s){}",
                    s.entry.models.len(),
                    s.entry
                        .models_fetched_at
                        .map(|t| format!(" — last {}", t.format("%Y-%m-%d %H:%M UTC")))
                        .unwrap_or_default()
                ),
            ),
            (
                "Delete",
                if s.delete_pending {
                    "(press Enter again to confirm)".to_string()
                } else {
                    String::new()
                },
            ),
            ("Back to list", String::new()),
        ];

        let label_w = rows
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0);

        for (i, (label, value)) in rows.iter().enumerate() {
            let marker = if i == s.cursor { "▸ " } else { "  " };
            let style = if i == s.cursor {
                yellow.add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled(format!("{:<width$}", label, width = label_w), style),
                Span::raw("  "),
                Span::styled(value.clone(), muted),
            ]));
        }

        if let Some(field) = s.editing_field {
            let prompt = match field {
                EditField::Url => "URL: ",
            };
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled(prompt.to_string(), muted),
                Span::styled(
                    s.field_buf.text().to_string(),
                    Style::default().fg(Color::White),
                ),
            ]));
        }

        if let Some(editor) = &s.headers_editor {
            lines.push(Line::default());
            render_header_editor(&mut lines, editor);
        }

        if let Some(status) = &s.status {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(status.clone(), yellow)));
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn render_fetch_all(&self, frame: &mut Frame, area: Rect, s: &FetchAllState) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(
            "Some configured models are not in the upstream /models list:".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for (pid, mid) in s.unlisted.iter().take(10) {
            lines.push(Line::from(Span::styled(format!("  {pid} › {mid}"), muted)));
        }
        if s.unlisted.len() > 10 {
            lines.push(Line::from(Span::styled(
                format!("  … and {} more", s.unlisted.len() - 10),
                muted,
            )));
        }
        lines.push(Line::default());
        let opts = [
            "Don't remove unlisted models (default)",
            "Remove unlisted models",
        ];
        for (i, label) in opts.iter().enumerate() {
            let marker = if i == s.cursor { "▸ " } else { "  " };
            let style = if i == s.cursor {
                yellow.add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled(label.to_string(), style),
            ]));
        }
        let check = if s.dont_ask_again { "[x]" } else { "[ ]" };
        let style = if s.cursor == 2 {
            yellow.add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::raw(if s.cursor == 2 { "▸ " } else { "  " }),
            Span::styled(format!("{check} Do not show again"), style),
        ]));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}

// ── Helpers / freestanding renderers ─────────────────────────────────────

fn root_nodes() -> [NavNode; 3] {
    [
        NavNode {
            title: "Providers",
            description: "Configure LLM providers, headers, and the default model.",
        },
        NavNode {
            title: "Agents",
            description: "Manage agent definitions, presets, and per-agent overrides.",
        },
        NavNode {
            title: "Tools",
            description: "Tune which tools are exposed to agents and their permission scopes.",
        },
    ]
}

struct NavNode {
    title: &'static str,
    description: &'static str,
}

const AGENTS_STUB: &str = "(stub) Agent editor — list agent definitions, edit their system prompts, tool grants, and model overrides.";
const TOOLS_STUB: &str =
    "(stub) Tool registry — toggle availability per tool and configure permission scopes.";

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

/// Render a [`HeaderEditor`] as rows + `[+ add header]` + (optional)
/// `[continue →]`. The active cursor row is highlighted in yellow; the
/// in-flight name/value buffer (when editing) replaces the row's value.
fn render_header_editor(lines: &mut Vec<Line<'static>>, h: &HeaderEditor) {
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let yellow = Style::default().fg(Color::Yellow);
    lines.push(Line::from(Span::styled(
        "Headers:".to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    let name_w = h
        .rows()
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(13);

    for (i, row) in h.rows().iter().enumerate() {
        let cursor_here = h.cursor == i;
        let marker = if cursor_here { "  ▸ " } else { "    " };
        let editing_name = matches!(h.mode, HeaderMode::EditName(j) if j == i);
        let editing_value = matches!(h.mode, HeaderMode::EditValue(j) if j == i);
        let name_text = if editing_name {
            h.name_buf.text().to_string()
        } else {
            row.name.clone()
        };
        let value_text = if editing_value {
            h.value_buf.text().to_string()
        } else {
            row.value.clone()
        };
        let name_style = if editing_name {
            Style::default().fg(Color::Yellow)
        } else if cursor_here {
            yellow.add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let value_style = if editing_value {
            Style::default().fg(Color::White)
        } else {
            muted
        };
        lines.push(Line::from(vec![
            Span::raw(marker.to_string()),
            Span::styled(format!("{:<width$}", name_text, width = name_w), name_style),
            Span::raw("  "),
            Span::styled(value_text.clone(), value_style),
        ]));

        // Missing-env warning for the row currently being edited.
        if editing_value {
            let resolved = envref::resolve(&value_text);
            if resolved.has_missing() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "      Environment variable not detected, make sure to set it: ${}",
                        resolved.missing.join(", $")
                    ),
                    yellow,
                )));
            } else if !resolved.referenced.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "      env var(s) detected: ${}",
                        resolved.referenced.join(", $")
                    ),
                    muted,
                )));
            }
        }
    }

    let add_idx = h.add_row_idx();
    let add_cursor = h.cursor == add_idx;
    let add_marker = if add_cursor { "  ▸ " } else { "    " };
    let add_style = if add_cursor {
        yellow.add_modifier(Modifier::BOLD)
    } else {
        muted
    };
    lines.push(Line::from(vec![
        Span::raw(add_marker.to_string()),
        Span::styled("[+ add header]".to_string(), add_style),
    ]));

    if let Some(cont_idx) = h.continue_idx() {
        let cont_cursor = h.cursor == cont_idx;
        let marker = if cont_cursor { "  ▸ " } else { "    " };
        let style = if cont_cursor {
            yellow.add_modifier(Modifier::BOLD)
        } else {
            muted
        };
        lines.push(Line::from(vec![
            Span::raw(marker.to_string()),
            Span::styled("[continue → save & fetch /models]".to_string(), style),
        ]));
    }
}

fn render_field_row(lines: &mut Vec<Line<'static>>, label: &str, field: &TextField, active: bool) {
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let value_style = if active {
        Style::default().fg(Color::White)
    } else {
        muted
    };
    let marker = if active { "▸ " } else { "  " };
    lines.push(Line::from(vec![
        Span::raw(marker),
        Span::styled(
            format!("{label}: "),
            if active {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted
            },
        ),
        Span::styled(field.text().to_string(), value_style),
        if active {
            Span::styled("▎".to_string(), Style::default().fg(Color::Yellow))
        } else {
            Span::raw("")
        },
    ]));
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

fn valid_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("http://") || s.starts_with("https://")
}

/// Provider ids are config-map keys. Restrict to a conservative
/// shell/filename-safe set so they're easy to reference from the CLI.
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

// ── Constructors for the inner states ────────────────────────────────────

impl AddState {
    fn new() -> Self {
        Self {
            step: AddStep::PickTemplate { cursor: 0 },
            template: None,
            id_field: TextField::default(),
            url_field: TextField::default(),
            headers: HeaderEditor::new(Vec::new(), true),
            codex_login: None,
            error: None,
            fetch: None,
            saved_provider_id: None,
        }
    }
}

impl EditState {
    fn new(provider_id: String, entry: ProviderEntry) -> Self {
        Self {
            provider_id,
            entry,
            cursor: 0,
            editing_field: None,
            field_buf: TextField::default(),
            status: None,
            fetch: None,
            delete_pending: false,
            headers_editor: None,
        }
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
    use crate::config::providers::ModelEntry;

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
                extra: Default::default(),
            },
            ModelEntry {
                id: "m2".into(),
                name: None,
                thinking_modes: vec![],
                inputs: None,
                context_length: None,
                favorite: false,
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
    fn pressing_c_from_providers_list_enters_add_wizard() {
        // Reproduces the "dialog freezes on c" bug — the original
        // implementation swapped the page out, then the inner handler
        // wrote `self.page = Add(...)` into the placeholder slot, and
        // the outer's unconditional swap-back discarded that write.
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        d.enter_providers();
        assert!(on_list_page(&d));
        let close = d.handle_key(press(KeyCode::Char('c')));
        assert!(!close);
        assert!(
            on_add_page(&d),
            "after pressing `c` the dialog should be on the Add wizard, not stuck on List"
        );
    }

    #[test]
    fn pressing_esc_in_add_wizard_returns_to_list() {
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        d.enter_providers();
        d.handle_key(press(KeyCode::Char('c')));
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
}
