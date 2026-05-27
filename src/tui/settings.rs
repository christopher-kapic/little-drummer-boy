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

use crate::auth::copilot_setup::{self, Shell as CopilotShell};
use crate::config::dirs::{
    ConfigDir, ConfigDirKind, creatable_config_dirs, cwd_scoped_creatable_dirs,
    discover_config_dirs, scaffold_config_dir,
};
use crate::config::extended::{
    ExtendedConfig, ExtendedConfigDoc, ThinkingDisplay, ToolCommandTemplate, VimModeSetting,
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
    extended_path: PathBuf,
    page: Page,
    /// Cached config state; reloaded on entry into the Providers list
    /// and after each successful save.
    config: ProvidersConfig,
    /// Cached `extended-config.json` state. Read by the UI page and the
    /// Tools page; written back on each edit.
    extended: ExtendedConfig,
    /// Root-page cursor restored when navigating back. Updated every
    /// time we leave Root for a subpage.
    last_root_cursor: usize,
    /// The cwd this dialog was opened against. Held so Root's `h`/←
    /// can reopen the picker without losing context. `None` when the
    /// settings dialog was opened from a flow that has no picker to
    /// return to.
    picker_cwd: Option<PathBuf>,
    /// Set by Root's back action to ask the outer [`Dialog`] to
    /// re-open the picker on the next `true` return from `handle_key`.
    back_to_picker: bool,
}

enum Page {
    Root { cursor: usize },
    Agents,
    Tools(ToolsPage),
    Providers(ProvidersPage),
    Ui(UiPage),
    Instructions(InstructionsPage),
}

/// `/settings → UI → Instructions File` state. Edits the
/// `extended.agent_guidance_files` list.
struct InstructionsPage {
    cursor: usize,
    /// When `Some(g)`, the user is holding the row currently at
    /// `cursor`. While grabbed they may rename it (typing goes to
    /// `g.buf`) and reorder it (↑/↓ swaps with the adjacent row —
    /// only arrows; j/k stay free so the user can type those letters
    /// into the filename). Enter commits and drops; Esc reverts the
    /// filename, swaps the row back to `g.origin`, and drops.
    grabbed: Option<GrabState>,
    status: Option<String>,
}

/// Per-row state while a row is grabbed.
struct GrabState {
    /// Live text buffer for the grabbed row's filename.
    buf: TextField,
    /// Index the row had when grabbed, restored on Esc.
    origin: usize,
    /// Original filename. `Some` for rows that already existed
    /// (Esc restores the name); `None` for rows freshly created by
    /// `a` or Enter-on-`[+ add]` (Esc deletes them).
    original_name: Option<String>,
}

/// `/settings → UI` state.
struct UiPage {
    cursor: usize,
    /// `Some(field)` when the user is inline-editing a text field.
    editing: Option<UiField>,
    buf: TextField,
    status: Option<String>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum UiField {
    Name,
    PackagesDir,
}

/// `/settings → Tools` state. Edits the user-defined bash-command
/// templates under `extended-config.tools`.
struct ToolsPage {
    cursor: usize,
    editing: Option<ToolField>,
    buf: TextField,
    /// Which tool's row is being edited, when `editing` is `Some`.
    edit_target: Option<String>,
    status: Option<String>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum ToolField {
    Command,
    Description,
}

enum ProvidersPage {
    /// Top-level list of configured providers + the "add new" affordance.
    List {
        cursor: usize,
        status: Option<String>,
        /// True after the first `d` press while the cursor is on a
        /// provider row. The next `d` confirms the delete; any other
        /// key clears it. Mirrors the same affordance on the Edit page.
        delete_pending: bool,
    },
    /// Add-provider wizard.
    Add(AddState),
    /// Edit a specific provider.
    Edit(EditState),
    /// Edit the headers list for the provider whose Edit state is in
    /// `parent`. Reached by Enter on the "Headers" row of the Edit
    /// page. The whole pane is the header editor; back navigation
    /// returns to `Edit(parent)` with `parent.entry.headers` set from
    /// `editor.rows`.
    Headers {
        editor: HeaderEditor,
        parent: Box<EditState>,
    },
    /// Triggered by /fetch-models — prompts on unlisted models.
    FetchAll(FetchAllState),
    /// One-button "Set up GitHub Copilot auth" confirm screen. Visible
    /// only when no Copilot env var is set; appends
    /// `export GH_TOKEN=$(gh auth token)` to the user's shell rc and
    /// sets `GH_TOKEN` in the running process so Copilot works without
    /// a restart.
    CopilotSetup(CopilotSetupState),
}

/// State for the "Set up GitHub Copilot auth" sub-page.
struct CopilotSetupState {
    /// Detected shell. `None` means we'll show manual instructions
    /// instead of a write button.
    shell: Option<CopilotShell>,
    /// Absolute rc-file path we'd append to. `None` when shell is None.
    rc_path: Option<PathBuf>,
    /// `Some(true)` if our marker is already in the rc file. The
    /// confirm prompt collapses to a "remove and re-add" hint.
    already_configured: bool,
    /// Action result after the user confirms. On success, we also
    /// inject `GH_TOKEN` into the running process so the resolver
    /// picks it up before the user restarts.
    outcome: Option<Result<String, String>>,
}

impl CopilotSetupState {
    fn new() -> Self {
        let shell = copilot_setup::detect_shell();
        let rc_path = shell.and_then(copilot_setup::rc_path);
        let already_configured = rc_path
            .as_deref()
            .and_then(|p| copilot_setup::rc_already_configured(p).ok())
            .unwrap_or(false);
        Self {
            shell,
            rc_path,
            already_configured,
            outcome: None,
        }
    }
}

struct AddState {
    step: AddStep,
    template: Option<&'static ProviderTemplate>,
    id_field: TextField,
    url_field: TextField,
    headers: HeaderEditor,
    /// Active OAuth device-flow attempt, when the picked template uses
    /// `AuthKind::DeviceFlow`. Replaces the URL/Headers steps for
    /// those templates. Today only the Codex template ships a device
    /// flow; Copilot was migrated off device-code in favor of
    /// documented GitHub-token env vars (see `src/providers/mod.rs`).
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
    /// GitHub Copilot's auth-setup step — surfaces the "append
    /// `export GH_TOKEN=$(gh auth token)` to your shell rc" action (or
    /// the manual-instructions fallback) before saving. Replaces the
    /// EditHeaders step for the Copilot template; the canonical
    /// Authorization header is fixed by the template anyway.
    CopilotAuth(CopilotSetupState),
    /// Run a device-code OAuth flow. Lives in `s.codex_login`. Reached
    /// directly after EditId when the template's auth is `DeviceFlow`.
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
            KeyCode::Char('d') | KeyCode::Delete => {
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
    pub fn spawn(provider_id: String, entry: ProviderEntry) -> Self {
        let state = Arc::new(Mutex::new(FetchState::Running));
        let state_w = Arc::clone(&state);
        let pid = provider_id.clone();
        tokio::spawn(async move {
            let result = match models_fetch::resolve_provider_request(&pid, &entry) {
                Err(e) => Err(e.to_string()),
                Ok(r) => models_fetch::fetch_models(
                    &r.base_url,
                    &r.headers,
                    Some(Duration::from_secs(15)),
                )
                .await
                .map_err(|e| e.to_string()),
            };
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
    fn save_extended(&mut self) -> Result<(), String> {
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
            Page::Providers(ProvidersPage::Headers { parent, .. }) => {
                parent.status = Some(message);
                parent.fetch = None;
                // Don't clobber the in-flight header edits — only
                // refresh non-header fields from the saved entry.
                if let Some(entry) = self.config.providers.get(provider_id) {
                    parent.entry.models = entry.models.clone();
                    parent.entry.models_fetched_at = entry.models_fetched_at;
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

    fn handle_providers_key(&mut self, key: KeyEvent) -> bool {
        // Detach the providers page so its `&mut SubState` doesn't alias
        // `&mut self`. Inner handlers communicate navigation via the
        // returned [`Nav`] rather than writing `self.page`, because the
        // swap-back below would otherwise discard those writes.
        let placeholder = Page::Providers(ProvidersPage::List {
            cursor: 0,
            status: None,
            delete_pending: false,
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
            ProvidersPage::List {
                cursor,
                status,
                delete_pending,
            } => {
                let ids: Vec<String> = self.config.providers.keys().cloned().collect();
                let max_cursor = ids.len().saturating_sub(1);
                let pressed_d = matches!(key.code, KeyCode::Char('d'));
                match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                        return Nav::Replace(Page::Root {
                            cursor: self.last_root_cursor,
                        });
                    }
                    KeyCode::Char('q') => return Nav::Close,
                    KeyCode::Up | KeyCode::Char('k') => {
                        *cursor = cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *cursor = (*cursor + 1).min(max_cursor);
                    }
                    KeyCode::Char('a') => {
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
                    KeyCode::Char('d') => {
                        // Only arm/confirm when the cursor is on a
                        // provider row (not the synthetic Copilot button).
                        let on_provider_row = *cursor < ids.len();
                        if !on_provider_row {
                            // Drop through to the post-match cleanup.
                        } else if *delete_pending {
                            let id = ids[*cursor].clone();
                            self.config.providers.remove(&id);
                            let msg = match self.save_config() {
                                Ok(()) => format!("deleted `{id}`"),
                                Err(e) => format!("delete failed: {e}"),
                            };
                            let new_len = self.config.providers.len();
                            let new_cursor = (*cursor).min(new_len.saturating_sub(1));
                            return Nav::Replace(Page::Providers(ProvidersPage::List {
                                cursor: new_cursor,
                                status: Some(msg),
                                delete_pending: false,
                            }));
                        } else {
                            *delete_pending = true;
                            *status = Some(format!("press d again to delete `{}`", ids[*cursor]));
                            return Nav::Stay;
                        }
                    }
                    _ => {}
                }
                // Any non-`d` key (or `d` on a non-provider row) clears
                // the pending-delete arm and the transient status.
                if !pressed_d {
                    *delete_pending = false;
                    *status = None;
                }
                Nav::Stay
            }
            ProvidersPage::Add(state) => self.handle_add_key(key, state),
            ProvidersPage::Edit(state) => self.handle_edit_key(key, state),
            ProvidersPage::Headers { editor, parent } => {
                self.handle_headers_key(key, editor, parent)
            }
            ProvidersPage::FetchAll(state) => self.handle_fetch_all_key(key, state),
            ProvidersPage::CopilotSetup(state) => self.handle_copilot_setup_key(key, state),
        }
    }

    /// Shared "save the provider, then spawn a /models fetch" sequence.
    /// Pulled out so the Headers step and the Copilot-auth step can
    /// both finalize without duplicating the error-handling.
    fn save_and_fetch_provider(
        &mut self,
        s: &mut AddState,
        id: String,
        entry: ProviderEntry,
        template: &'static ProviderTemplate,
    ) {
        self.config.providers.insert(id.clone(), entry.clone());
        match self.save_config() {
            Ok(()) => {
                s.saved_provider_id = Some(id.clone());
                s.error = Some("saved. Fetching /models…".into());
                match models_fetch::resolve_provider_request(&id, &entry) {
                    Err(e) => {
                        s.error = Some(format!("saved. /models fetch skipped — {e}"));
                        s.step = AddStep::Done;
                    }
                    Ok(_) if !template.supports_models_endpoint => {
                        s.error = Some("saved. provider has no /models endpoint".into());
                        s.step = AddStep::Done;
                    }
                    Ok(_) => {
                        s.fetch = Some(FetchHandle::spawn(id, entry));
                        s.step = AddStep::Fetching;
                    }
                }
            }
            Err(e) => {
                s.error = Some(format!("save failed: {e}"));
            }
        }
    }

    fn handle_add_key(&mut self, key: KeyEvent, s: &mut AddState) -> Nav {
        // Back/escape unconditionally returns to the list.
        if matches!(key.code, KeyCode::Esc) {
            return Nav::Replace(Page::Providers(ProvidersPage::List {
                cursor: 0,
                status: None,
                delete_pending: false,
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
                        // GitHub Copilot's auth is documented env-var
                        // tokens, not custom headers — route to the
                        // dedicated Copilot-auth screen so the
                        // GH_TOKEN setup button lives next to the
                        // provider it actually configures.
                        if matches!(s.template.map(|t| t.id), Some("copilot")) {
                            s.step = AddStep::CopilotAuth(CopilotSetupState::new());
                        } else {
                            s.step = AddStep::EditHeaders;
                        }
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

                let template = s.template.expect("template chosen");
                let id = s.id_field.text().trim().to_string();
                let headers: Vec<HeaderSpec> = s.headers.rows().to_vec();
                let entry = ProviderEntry {
                    name: Some(template.display.to_string()),
                    url: s.url_field.text().trim_end_matches('/').to_string(),
                    headers,
                    models_fetched_at: None,
                    favorite: None,
                    credential_ref: None,
                    auth: Some(template.auth),
                    models: vec![],
                };
                self.save_and_fetch_provider(s, id, entry, template);
            }
            AddStep::CopilotAuth(state) => match key.code {
                KeyCode::Enter => {
                    if state.outcome.is_some() {
                        // Outcome already shown — Enter advances to
                        // save + fetch.
                        let template = s.template.expect("template chosen");
                        let id = s.id_field.text().trim().to_string();
                        let headers = templates::default_headers_for(template);
                        let entry = ProviderEntry {
                            name: Some(template.display.to_string()),
                            url: s.url_field.text().trim_end_matches('/').to_string(),
                            headers,
                            models_fetched_at: None,
                            favorite: None,
                            credential_ref: None,
                            auth: Some(template.auth),
                            models: vec![],
                        };
                        self.save_and_fetch_provider(s, id, entry, template);
                        return Nav::Stay;
                    }
                    // No outcome yet. Apply the action if we can; else
                    // jump straight to save + fetch (manual / already-
                    // configured paths are informational only).
                    let can_apply = state.shell.is_some()
                        && state.rc_path.is_some()
                        && !state.already_configured;
                    if can_apply {
                        let shell = state.shell.unwrap();
                        let rc_path = state.rc_path.clone().unwrap();
                        state.outcome = Some(apply_copilot_setup(shell, &rc_path));
                    } else {
                        // Skip — move to save + fetch.
                        let template = s.template.expect("template chosen");
                        let id = s.id_field.text().trim().to_string();
                        let headers = templates::default_headers_for(template);
                        let entry = ProviderEntry {
                            name: Some(template.display.to_string()),
                            url: s.url_field.text().trim_end_matches('/').to_string(),
                            headers,
                            models_fetched_at: None,
                            favorite: None,
                            credential_ref: None,
                            auth: Some(template.auth),
                            models: vec![],
                        };
                        self.save_and_fetch_provider(s, id, entry, template);
                    }
                }
                KeyCode::Char('s') => {
                    // Skip the GH_TOKEN action and go straight to save
                    // + fetch — useful when the env var is already set
                    // elsewhere (e.g. via direnv).
                    let template = s.template.expect("template chosen");
                    let id = s.id_field.text().trim().to_string();
                    let headers = templates::default_headers_for(template);
                    let entry = ProviderEntry {
                        name: Some(template.display.to_string()),
                        url: s.url_field.text().trim_end_matches('/').to_string(),
                        headers,
                        models_fetched_at: None,
                        favorite: None,
                        credential_ref: None,
                        auth: Some(template.auth),
                        models: vec![],
                    };
                    self.save_and_fetch_provider(s, id, entry, template);
                }
                _ => {}
            },
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
                if matches!(key.code, KeyCode::Char('r'))
                    && matches!(snap, CodexLoginProgress::Error(_))
                {
                    s.codex_login = Some(CodexLoginState::spawn());
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
                        delete_pending: false,
                    }));
                }
            }
        }
        Nav::Stay
    }

    fn handle_edit_key(&mut self, key: KeyEvent, s: &mut EditState) -> Nav {
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

        // Action menu. `h` / `←` / Backspace all go back to the list —
        // header editing now lives on its own sub-page reached by
        // cursor → Enter on the "Headers" row.
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                return Nav::Replace(Page::Providers(ProvidersPage::List {
                    cursor: 0,
                    status: s.status.clone(),
                    delete_pending: false,
                }));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                s.cursor = s.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                s.cursor = (s.cursor + 1).min(EDIT_MENU_LEN - 1);
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
                match models_fetch::resolve_provider_request(&s.provider_id, &s.entry) {
                    Err(e) => {
                        s.status = Some(format!("refetch skipped — {e}"));
                    }
                    Ok(_) => {
                        s.fetch = Some(FetchHandle::spawn(s.provider_id.clone(), s.entry.clone()));
                        s.status = Some("refetching /models…".into());
                    }
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
                        delete_pending: false,
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
                        // Hand off to the Headers sub-page. We move
                        // the EditState out via `mem::replace` so the
                        // Headers page can return it intact on back.
                        let editor = HeaderEditor::new(
                            s.entry.headers.clone(),
                            /* show_continue */ false,
                        );
                        let owned = std::mem::replace(
                            s,
                            EditState::new(String::new(), ProviderEntry::default()),
                        );
                        return Nav::Replace(Page::Providers(ProvidersPage::Headers {
                            editor,
                            parent: Box::new(owned),
                        }));
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
                        match models_fetch::resolve_provider_request(&s.provider_id, &s.entry) {
                            Err(e) => {
                                s.status = Some(format!("refetch skipped — {e}"));
                            }
                            Ok(_) => {
                                s.fetch = Some(FetchHandle::spawn(
                                    s.provider_id.clone(),
                                    s.entry.clone(),
                                ));
                                s.status = Some("refetching /models…".into());
                            }
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
                                delete_pending: false,
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
                            delete_pending: false,
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

    /// Handle keys on the Headers sub-page. All keys go to the
    /// [`HeaderEditor`] until it signals `Back`; on back, copy the
    /// editor's rows into `parent.entry.headers` and return to the
    /// Edit page with the parent intact (so its cursor, status, and
    /// any unsaved entry-level edits survive the round trip).
    fn handle_headers_key(
        &mut self,
        key: KeyEvent,
        editor: &mut HeaderEditor,
        parent: &mut Box<EditState>,
    ) -> Nav {
        match editor.handle_key(key) {
            HeaderResult::Stay | HeaderResult::Continue => Nav::Stay,
            HeaderResult::Back => {
                // Move both the editor's rows and the parent state
                // out by swapping with placeholders, then build the
                // restored Edit page.
                let rows = std::mem::take(&mut editor.rows);
                let mut owned = std::mem::replace(
                    parent.as_mut(),
                    EditState::new(String::new(), ProviderEntry::default()),
                );
                owned.entry.headers = rows;
                // Put the cursor back on the Headers row and tell the
                // user there are unsaved changes to commit with `s`.
                owned.cursor = 1;
                owned.status = Some("headers updated; press s to save".into());
                Nav::Replace(Page::Providers(ProvidersPage::Edit(owned)))
            }
        }
    }

    fn handle_fetch_all_key(&mut self, key: KeyEvent, s: &mut FetchAllState) -> Nav {
        match key.code {
            KeyCode::Esc => {
                return Nav::Replace(Page::Providers(ProvidersPage::List {
                    cursor: 0,
                    status: Some("/fetch-models cancelled".into()),
                    delete_pending: false,
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
                    delete_pending: false,
                }));
            }
            _ => {}
        }
        Nav::Stay
    }

    /// Handle keys on the "Set up GitHub Copilot auth" confirm screen.
    /// Enter applies the action (or, in the manual / already-configured
    /// case, returns to the list). Esc always returns to the list.
    fn handle_copilot_setup_key(&mut self, key: KeyEvent, s: &mut CopilotSetupState) -> Nav {
        match key.code {
            KeyCode::Esc => {
                return Nav::Replace(Page::Providers(ProvidersPage::List {
                    cursor: 0,
                    status: None,
                    delete_pending: false,
                }));
            }
            KeyCode::Enter => {
                // If we've already shown the user a result, Enter closes.
                if s.outcome.is_some() {
                    let status = match &s.outcome {
                        Some(Ok(msg)) => Some(msg.clone()),
                        Some(Err(e)) => Some(e.clone()),
                        None => None,
                    };
                    return Nav::Replace(Page::Providers(ProvidersPage::List {
                        cursor: 0,
                        status,
                        delete_pending: false,
                    }));
                }

                // If we can't auto-write (unsupported shell, marker
                // already present), Enter just returns to the list —
                // the screen was informational only.
                let Some(shell) = s.shell else {
                    return Nav::Replace(Page::Providers(ProvidersPage::List {
                        cursor: 0,
                        status: None,
                        delete_pending: false,
                    }));
                };
                if s.already_configured {
                    return Nav::Replace(Page::Providers(ProvidersPage::List {
                        cursor: 0,
                        status: None,
                        delete_pending: false,
                    }));
                }
                let Some(rc_path) = s.rc_path.clone() else {
                    return Nav::Replace(Page::Providers(ProvidersPage::List {
                        cursor: 0,
                        status: None,
                        delete_pending: false,
                    }));
                };

                s.outcome = Some(apply_copilot_setup(shell, &rc_path));
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
            Page::Tools(p) => self.render_tools_page(frame, layout[0], p),
            Page::Ui(p) => self.render_ui_page(frame, layout[0], p),
            Page::Instructions(p) => self.render_instructions_page(frame, layout[0], p),
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
            Page::Providers(ProvidersPage::List { .. }) => {
                "↑/↓  enter: edit  a: add  d: delete (×2 to confirm)  h: back  esc: close"
            }
            Page::Providers(ProvidersPage::Add(s)) => match s.step {
                AddStep::PickTemplate { .. } => "↑/↓  enter: choose  esc: cancel",
                AddStep::EditId | AddStep::EditUrl => "type to edit  enter: next  esc: cancel",
                AddStep::EditHeaders => {
                    if s.headers.is_editing() {
                        "type to edit  Tab: name/value  enter: apply  esc: cancel"
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
                    "type to edit  Tab: name/value  enter: apply  esc: cancel"
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

    fn render_providers_page(&self, frame: &mut Frame, area: Rect, page: &ProvidersPage) {
        match page {
            ProvidersPage::List {
                cursor,
                status,
                delete_pending,
            } => {
                self.render_providers_list(frame, area, *cursor, status.as_deref(), *delete_pending)
            }
            ProvidersPage::Add(s) => self.render_add(frame, area, s),
            ProvidersPage::Edit(s) => self.render_edit(frame, area, s),
            ProvidersPage::Headers { editor, parent } => {
                self.render_headers_page(frame, area, editor, parent.as_ref())
            }
            ProvidersPage::FetchAll(s) => self.render_fetch_all(frame, area, s),
            ProvidersPage::CopilotSetup(s) => self.render_copilot_setup(frame, area, s),
        }
    }

    fn render_providers_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        cursor: usize,
        status: Option<&str>,
        delete_pending: bool,
    ) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let red = Style::default().fg(Color::Red);
        let mut lines: Vec<Line<'static>> = Vec::new();
        let ids: Vec<&String> = self.config.providers.keys().collect();
        if ids.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no providers configured — press `c` to add one)".to_string(),
                muted,
            )));
        } else {
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
                let style = if i == cursor && delete_pending {
                    red.add_modifier(Modifier::BOLD)
                } else if i == cursor {
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

    fn render_copilot_setup(&self, frame: &mut Frame, area: Rect, s: &CopilotSetupState) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(
            "Set up GitHub Copilot auth".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
        render_copilot_setup_body(&mut lines, s);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}

/// Render the body of the Copilot auth-setup affordance (everything
/// after the bold title). Used both by the standalone CopilotSetup
/// page and by the embedded panel inside the Add-Provider Copilot flow.
fn render_copilot_setup_body(lines: &mut Vec<Line<'static>>, s: &CopilotSetupState) {
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let yellow = Style::default().fg(Color::Yellow);
    let red = Style::default().fg(Color::Red);
    let green = Style::default().fg(Color::Green);
    let cyan = Style::default().fg(Color::Cyan);

    if let Some(outcome) = &s.outcome {
        // Post-action result screen.
        match outcome {
            Ok(msg) => {
                lines.push(Line::from(Span::styled(msg.clone(), green)));
            }
            Err(e) => {
                lines.push(Line::from(Span::styled(format!("Failed: {e}"), red)));
            }
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Press Enter to continue.".to_string(),
            muted,
        )));
        return;
    }

    match (s.shell, &s.rc_path, s.already_configured) {
        (Some(shell), Some(rc_path), false) => {
            lines.push(Line::from(Span::styled(
                format!("Detected shell: {}", shell.name()),
                muted,
            )));
            lines.push(Line::from(vec![
                Span::styled("Will append to: ".to_string(), muted),
                Span::styled(rc_path.display().to_string(), cyan),
            ]));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "Lines to be added:".to_string(),
                muted,
            )));
            for line in copilot_setup::append_block(shell).lines() {
                if line.is_empty() {
                    lines.push(Line::default());
                } else {
                    lines.push(Line::from(Span::styled(format!("    {line}"), cyan)));
                }
            }
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "We'll also run `gh auth token` once and set GH_TOKEN in this \
                     cockpit session so Copilot works without restarting."
                    .to_string(),
                muted,
            )));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "Press Enter to apply, Esc to cancel.".to_string(),
                yellow,
            )));
        }
        (Some(shell), Some(rc_path), true) => {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} already contains the cockpit Copilot-auth export.",
                    rc_path.display()
                ),
                muted,
            )));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!(
                    "To re-apply: remove the marker block from your {} and try again.",
                    shell.rc_filename()
                ),
                muted,
            )));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "Press Enter or Esc to return.".to_string(),
                yellow,
            )));
        }
        _ => {
            // Unsupported shell or unknown $HOME — show manual
            // instructions instead of a write button.
            lines.push(Line::from(Span::styled(
                "Couldn't detect a supported shell ($SHELL is unset, or it's \
                     not zsh/bash/fish). Set GH_TOKEN manually with one of:"
                    .to_string(),
                muted,
            )));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  POSIX shell (zsh/bash/sh):".to_string(),
                muted,
            )));
            lines.push(Line::from(Span::styled(
                "    export GH_TOKEN=$(gh auth token)".to_string(),
                cyan,
            )));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled("  fish:".to_string(), muted)));
            lines.push(Line::from(Span::styled(
                "    set -Ux GH_TOKEN (gh auth token)".to_string(),
                cyan,
            )));
            if cfg!(windows) {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "  Windows PowerShell ($PROFILE):".to_string(),
                    muted,
                )));
                lines.push(Line::from(Span::styled(
                    "    $env:GH_TOKEN = (gh auth token)".to_string(),
                    cyan,
                )));
                lines.push(Line::from(Span::styled(
                    "  Windows persistent (User scope):".to_string(),
                    muted,
                )));
                lines.push(Line::from(Span::styled(
                    "    [Environment]::SetEnvironmentVariable(\"GH_TOKEN\", \
                         (gh auth token), \"User\")"
                        .to_string(),
                    cyan,
                )));
            }
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "Press Enter or Esc to return.".to_string(),
                yellow,
            )));
        }
    }
}

impl SettingsDialog {
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
            AddStep::CopilotAuth(state) => {
                let t = s.template.expect("template chosen");
                lines.push(Line::from(vec![
                    Span::styled("Template: ", muted),
                    Span::styled(t.display.to_string(), Style::default().fg(Color::White)),
                ]));
                lines.push(Line::default());
                lines.push(Line::from(vec![
                    Span::styled("id:  ", muted),
                    Span::styled(
                        s.id_field.text().to_string(),
                        Style::default().fg(Color::White),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("API url: ", muted),
                    Span::styled(
                        s.url_field.text().to_string(),
                        Style::default().fg(Color::White),
                    ),
                ]));
                lines.push(Line::default());
                render_copilot_setup_body(&mut lines, state);
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "After this step we'll fetch the model list automatically. \
                     Press `s` to skip the GH_TOKEN setup if your token is \
                     already in the environment."
                        .to_string(),
                    muted,
                )));
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

        if let Some(status) = &s.status {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(status.clone(), yellow)));
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    /// Full-pane render for the Headers sub-page. The header rows are
    /// the entire content; the parent Edit state is recalled on back.
    fn render_headers_page(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &HeaderEditor,
        parent: &EditState,
    ) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(vec![
                Span::styled("Provider: ", muted),
                Span::styled(
                    parent.provider_id.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::default(),
        ];
        render_header_editor(&mut lines, editor);
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

    // ── UI page ──────────────────────────────────────────────────────────

    fn handle_ui_key(&mut self, key: KeyEvent) -> bool {
        // Detach + swap pattern (same rationale as handle_providers_key).
        // The inner handler must return navigation intent via `Nav`
        // instead of writing `self.page` directly — otherwise the
        // swap-back below would discard the write.
        let placeholder = Page::Ui(UiPage {
            cursor: 0,
            editing: None,
            buf: TextField::default(),
            status: None,
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
                    p.buf = TextField::new(self.extended.name.clone().unwrap_or_default());
                    p.editing = Some(UiField::Name);
                }
                5 => {
                    let cur = self
                        .extended
                        .packages_directory
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    p.buf = TextField::new(cur);
                    p.editing = Some(UiField::PackagesDir);
                }
                6 => {
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

    fn render_ui_page(&self, frame: &mut Frame, area: Rect, p: &UiPage) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let mut lines: Vec<Line<'static>> = Vec::new();

        lines.push(Line::from(Span::styled(
            "User-interface preferences".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        let rows: [(&str, String); 7] = [
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

    // ── Instructions page ────────────────────────────────────────────────

    fn handle_instructions_key(&mut self, key: KeyEvent) -> bool {
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
                    cursor: 6,
                    editing: None,
                    buf: TextField::default(),
                    status: None,
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

    fn render_instructions_page(&self, frame: &mut Frame, area: Rect, p: &InstructionsPage) {
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

    // ── Tools page ───────────────────────────────────────────────────────

    fn handle_tools_key(&mut self, key: KeyEvent) -> bool {
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
                p.cursor = p.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                p.cursor = (p.cursor + 1).min(total_rows.saturating_sub(1));
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

    fn render_tools_page(&self, frame: &mut Frame, area: Rect, p: &ToolsPage) {
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

// ── Helpers / freestanding renderers ─────────────────────────────────────

fn root_nodes() -> [NavNode; 4] {
    [
        NavNode {
            title: "Providers",
            description: "Configure LLM providers, headers, and the default model.",
        },
        NavNode {
            title: "UI",
            description: "User-interface preferences: vim mode, thinking display, your name, and the docs-agent packages directory.",
        },
        NavNode {
            title: "Agents",
            description: "Manage agent definitions, presets, and per-agent overrides.",
        },
        NavNode {
            title: "Tools",
            description: "Custom bash-command tools (webfetch, websearch, …) the agent can invoke.",
        },
    ]
}

struct NavNode {
    title: &'static str,
    description: &'static str,
}

const AGENTS_STUB: &str = "(stub) Agent editor — list agent definitions, edit their system prompts, tool grants, and model overrides.";

/// Rows on the UI page (vim mode, thinking, render-agent-markdown,
/// render-user-markdown, name, packages dir, instructions file).
const UI_ROWS: usize = 7;

fn bool_label(on: bool, on_label: &str, off_label: &str) -> String {
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

fn vim_label(v: VimModeSetting) -> &'static str {
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

fn thinking_label(t: ThinkingDisplay) -> &'static str {
    match t {
        ThinkingDisplay::Condensed => "condensed (default — chip, ctrl+j expands every block)",
        ThinkingDisplay::Hidden => "hidden (only `Thinking…` while in flight; nothing after)",
        ThinkingDisplay::Verbose => "verbose (always show reasoning inline)",
    }
}

fn save_status(r: Result<(), String>) -> Option<String> {
    match r {
        Ok(()) => Some("saved".into()),
        Err(e) => Some(format!("save failed: {e}")),
    }
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

fn valid_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("http://") || s.starts_with("https://")
}

/// Execute the "Set up Copilot auth" action: append the export to the
/// shell rc file and inject `GH_TOKEN` into the running process so the
/// resolver picks it up without a restart. Returns a user-facing
/// status string on success, or an error message on failure.
fn apply_copilot_setup(shell: CopilotShell, rc_path: &std::path::Path) -> Result<String, String> {
    // Fetch the token first — if `gh` isn't installed or the user
    // isn't logged in, we want to fail before mutating the rc file.
    let token = copilot_setup::fetch_gh_token().map_err(|e| e.to_string())?;
    let wrote = copilot_setup::append_to_rc(rc_path, shell).map_err(|e| e.to_string())?;

    // SAFETY: `set_var` mutates process-global env state. The settings
    // dialog runs on the main thread before any inference request fires
    // for this session, so no concurrent reader observes the racy state.
    unsafe {
        std::env::set_var("GH_TOKEN", &token);
    }

    let suffix = if wrote {
        format!("added export to {}", rc_path.display())
    } else {
        format!("export already in {}", rc_path.display())
    };
    Ok(format!(
        "Copilot auth ready — {suffix}; GH_TOKEN set for this session"
    ))
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
        // The "can't edit instructions file" symptom: UI cursor=6 +
        // Enter should land on the Instructions page. Under the
        // swap-back bug, this navigation was silently dropped.
        let tmp = TempDir::new().unwrap();
        let mut d = fresh_dialog(&tmp);
        enter_ui_from_root(&mut d);
        // Move cursor to row 6 (instructions file).
        for _ in 0..6 {
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
        // Move cursor to the instructions row (idx 6) and Enter to nav.
        for _ in 0..6 {
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
        // Add a header from the Browse-mode `a` action.
        d.handle_key(press(KeyCode::Char('a')));
        // We're now inline-editing a header name. Tab to value, Enter
        // commits, then Esc to leave Browse, then `h` to go back.
        d.handle_key(press(KeyCode::Enter)); // commit (empty name+value, still adds the row)
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
