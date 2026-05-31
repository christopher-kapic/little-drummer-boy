//! `/settings → Providers`: the largest settings page tree.
//!
//! Lives here so the `mod.rs` dispatcher and the unrelated UI/Tools
//! pages aren't drowned by ~2K lines of provider-specific state
//! machine. Owns:
//!   - the [`ProvidersPage`] state enum (List, Add wizard, Edit page,
//!     Headers sub-page, FetchAll, CopilotSetup)
//!   - per-page state types (`AddState` + `AddStep`, `EditState` +
//!     `EditField`, `HeaderEditor` + modes, `FetchAllState`,
//!     `CopilotSetupState`)
//!   - the corresponding handlers + renderers on [`SettingsDialog`]
//!     (multiple `impl` blocks across this file and `mod.rs`)
//!   - provider-only free helpers (`render_header_editor`,
//!     `render_field_row`, `valid_url`, `valid_id`,
//!     `apply_copilot_setup`, `render_copilot_setup_body`).

use std::path::PathBuf;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::auth::copilot_setup::{self, Shell as CopilotShell};
use crate::config::providers::{HeaderSpec, ModelEntry, OnUnlistedModelsFetch, ProviderEntry};
use crate::envref;
use crate::providers::models_fetch::{self, FetchOutcome};
use crate::providers::{self as templates, ProviderTemplate};
use crate::tui::textfield::TextField;
use crate::tui::theme::MUTED_COLOR_INDEX;

use super::auth::{CodexLoginProgress, CodexLoginState, FetchHandle};
use super::settings_editor::{SettingsEditor, SettingsField, SettingsResult};
use super::{Nav, Page, SettingsDialog};

/// Number of selectable rows in the Edit-provider action menu.
/// Index map: 0=URL · 1=Headers · 2=Models · 3=Settings · 4=Favorite ·
/// 5=Refetch · 6=Delete · 7=Back.
const EDIT_MENU_LEN: usize = 8;

#[allow(private_interfaces)]
pub(super) enum ProvidersPage {
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
    /// Manage the model list for the provider whose Edit state is in
    /// `parent`. Reached by Enter on the "Models" row of the Edit page.
    /// Browse rows; add a manual entry; edit a manual entry; delete any
    /// entry. Back navigation returns to `Edit(parent)` with
    /// `parent.entry.models` set from `editor.rows`.
    Models {
        editor: ModelEditor,
        parent: Box<EditState>,
    },
    /// Edit a single model's `Option<…>` settings overrides
    /// (`prompts/model-provider-settings.md`). Reached by Enter/l/→ on a
    /// model row in the Models sub-page (every model, fetched or manual).
    /// Back navigation returns to `Models { parent }` with the model's
    /// override fields written back into the editor's rows.
    ModelSettings {
        editor: SettingsEditor,
        models: ModelEditor,
        parent: Box<EditState>,
    },
    /// Edit the provider's concrete settings values
    /// (`prompts/model-provider-settings.md`). Reached by the "Settings" row
    /// on the Edit page. Back navigation returns to `Edit(parent)` with the
    /// concrete values written into `parent.entry`.
    ProviderSettings {
        editor: SettingsEditor,
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
pub(super) struct CopilotSetupState {
    /// Detected shell. `None` means we'll show manual instructions
    /// instead of a write button.
    pub(super) shell: Option<CopilotShell>,
    /// Absolute rc-file path we'd append to. `None` when shell is None.
    pub(super) rc_path: Option<PathBuf>,
    /// `Some(true)` if our marker is already in the rc file. The
    /// confirm prompt collapses to a "remove and re-add" hint.
    pub(super) already_configured: bool,
    /// Action result after the user confirms. On success, we also
    /// inject `GH_TOKEN` into the running process so the resolver
    /// picks it up before the user restarts.
    pub(super) outcome: Option<Result<String, String>>,
}

impl CopilotSetupState {
    pub(super) fn new() -> Self {
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

pub(super) struct AddState {
    pub(super) step: AddStep,
    pub(super) template: Option<&'static ProviderTemplate>,
    pub(super) id_field: TextField,
    pub(super) url_field: TextField,
    pub(super) headers: HeaderEditor,
    /// Active OAuth device-flow attempt, when the picked template uses
    /// `AuthKind::DeviceFlow`. Replaces the URL/Headers steps for
    /// those templates. Today only the Codex template ships a device
    /// flow; Copilot was migrated off device-code in favor of
    /// documented GitHub-token env vars (see `src/providers/mod.rs`).
    pub(super) codex_login: Option<CodexLoginState>,
    pub(super) error: Option<String>,
    pub(super) fetch: Option<FetchHandle>,
    pub(super) saved_provider_id: Option<String>,
}

pub(super) enum AddStep {
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

pub(super) struct EditState {
    pub(super) provider_id: String,
    pub(super) entry: ProviderEntry,
    /// Index into [`edit_menu_rows`].
    pub(super) cursor: usize,
    pub(super) editing_field: Option<EditField>,
    pub(super) field_buf: TextField,
    pub(super) status: Option<String>,
    pub(super) fetch: Option<FetchHandle>,
    pub(super) delete_pending: bool,
}

#[derive(Copy, Clone)]
pub(super) enum EditField {
    Url,
}

/// Multi-row header list. Browsing the rows is inline; adding or
/// editing a header opens a name/value popup (see
/// [`render_header_edit_popup`]).
///
/// Layout (visible "rows" the cursor can land on):
///   - 0..n               actual header rows
///   - n                  `[+ add header]`
///   - n+1                `[continue →]` (used by the Add wizard)
///
/// In Browse mode the cursor selects a row and `Tab`/`Shift+Tab` move
/// like `↓`/`↑`. With the popup open, `Tab`/`Shift+Tab` switch between
/// the name and value fields, `enter` saves, and `esc` cancels.
pub(super) struct HeaderEditor {
    pub(super) rows: Vec<HeaderSpec>,
    pub(super) cursor: usize,
    pub(super) mode: HeaderMode,
    pub(super) name_buf: TextField,
    pub(super) value_buf: TextField,
    /// Row the popup is editing; `None` while adding a brand-new header.
    /// A new header is committed to `rows` only on save, so cancelling
    /// an add leaves no blank row behind.
    pub(super) edit_target: Option<usize>,
    /// If false, the synthetic `[continue →]` row is suppressed (used
    /// from the Edit page, where there's no next step).
    pub(super) show_continue: bool,
}

pub(super) enum HeaderMode {
    Browse,
    /// Popup open, focused on the name field.
    EditName,
    /// Popup open, focused on the value field.
    EditValue,
}

pub(super) enum HeaderResult {
    Stay,
    Continue,
    Back,
}

impl HeaderEditor {
    pub(super) fn new(rows: Vec<HeaderSpec>, show_continue: bool) -> Self {
        Self {
            rows,
            cursor: 0,
            mode: HeaderMode::Browse,
            name_buf: TextField::default(),
            value_buf: TextField::default(),
            edit_target: None,
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

    /// Open the popup to add a brand-new header. The row is committed to
    /// `rows` only on save (see [`Self::commit_edit`]).
    fn begin_add(&mut self) {
        self.edit_target = None;
        self.name_buf = TextField::default();
        self.value_buf = TextField::default();
        self.mode = HeaderMode::EditName;
    }

    /// Open the popup to edit an existing row.
    fn begin_edit(&mut self, i: usize) {
        if let Some(row) = self.rows.get(i) {
            self.edit_target = Some(i);
            self.name_buf = TextField::new(row.name.clone());
            self.value_buf = TextField::new(row.value.clone());
            // Start on the value — the field most often changed when
            // editing an existing header.
            self.mode = HeaderMode::EditValue;
        }
    }

    /// Save the popup buffers and close it. A new header with an empty
    /// name is discarded so a stray `a` leaves no blank row; edits to an
    /// existing row are always written so a field can be cleared.
    fn commit_edit(&mut self) {
        let name = self.name_buf.text().trim().to_string();
        let value = self.value_buf.text().to_string();
        match self.edit_target {
            Some(i) => {
                if let Some(row) = self.rows.get_mut(i) {
                    row.name = name;
                    row.value = value;
                    self.cursor = i;
                }
            }
            None => {
                if !name.is_empty() {
                    self.rows.push(HeaderSpec { name, value });
                    self.cursor = self.rows.len() - 1;
                }
            }
        }
        self.edit_target = None;
        self.mode = HeaderMode::Browse;
    }

    /// Close the popup without saving.
    fn cancel_edit(&mut self) {
        self.edit_target = None;
        self.mode = HeaderMode::Browse;
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> HeaderResult {
        match self.mode {
            HeaderMode::Browse => self.handle_browse_key(key),
            HeaderMode::EditName | HeaderMode::EditValue => self.handle_edit_key(key),
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> HeaderResult {
        match key.code {
            // `Tab`/`Shift+Tab` move like `↓`/`↑` while browsing rows.
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.cursor = crate::tui::nav::wrap_prev(self.cursor, self.max_cursor() + 1);
                HeaderResult::Stay
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.cursor = crate::tui::nav::wrap_next(self.cursor, self.max_cursor() + 1);
                HeaderResult::Stay
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                HeaderResult::Back
            }
            KeyCode::Char('a') => {
                self.begin_add();
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
                    self.begin_edit(self.cursor);
                    HeaderResult::Stay
                } else if self.cursor == self.add_row_idx() {
                    self.begin_add();
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
                self.cancel_edit();
                HeaderResult::Stay
            }
            KeyCode::Enter => {
                self.commit_edit();
                HeaderResult::Stay
            }
            // Two fields, so forward and backward both toggle focus.
            KeyCode::Tab | KeyCode::BackTab => {
                self.mode = match self.mode {
                    HeaderMode::EditName => HeaderMode::EditValue,
                    _ => HeaderMode::EditName,
                };
                HeaderResult::Stay
            }
            _ => {
                match self.mode {
                    HeaderMode::EditName => {
                        self.name_buf.handle_key(key);
                    }
                    HeaderMode::EditValue => {
                        self.value_buf.handle_key(key);
                    }
                    HeaderMode::Browse => {}
                }
                HeaderResult::Stay
            }
        }
    }

    pub(super) fn rows(&self) -> &[HeaderSpec] {
        &self.rows
    }

    pub(super) fn is_editing(&self) -> bool {
        !matches!(self.mode, HeaderMode::Browse)
    }
}

/// Multi-row model list manager for the provider Edit page. Browsing the
/// rows is inline; adding or editing a *manual* entry opens an
/// id/name/context popup (see [`render_model_edit_popup`]).
///
/// Layout (visible "rows" the cursor can land on):
///   - 0..n   actual model rows (fetched + manual, in list order)
///   - n      `[+ add model]`
///
/// Only manual entries can be edited (id / name / context). Any entry —
/// fetched or manual — can be deleted; a deleted fetched entry reappears
/// on the next `/models` refetch.
pub(super) struct ModelEditor {
    pub(super) rows: Vec<ModelEntry>,
    pub(super) cursor: usize,
    pub(super) mode: ModelMode,
    pub(super) id_buf: TextField,
    pub(super) name_buf: TextField,
    pub(super) context_buf: TextField,
    /// Row the popup is editing; `None` while adding a brand-new entry.
    pub(super) edit_target: Option<usize>,
    /// Field the popup is focused on while editing.
    pub(super) focus: ModelField,
    /// Transient validation/status message shown under the editor.
    pub(super) status: Option<String>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) enum ModelField {
    Id,
    Name,
    Context,
}

pub(super) enum ModelMode {
    Browse,
    /// id/name/context popup open (add or edit).
    Edit,
}

pub(super) enum ModelResult {
    Stay,
    Back,
    /// Open the model-settings sub-dialog for the row at this index
    /// (`prompts/model-provider-settings.md`). Works on every model — these
    /// are overrides, not edits to fetched data.
    OpenSettings(usize),
}

impl ModelEditor {
    pub(super) fn new(rows: Vec<ModelEntry>) -> Self {
        Self {
            rows,
            cursor: 0,
            mode: ModelMode::Browse,
            id_buf: TextField::default(),
            name_buf: TextField::default(),
            context_buf: TextField::default(),
            edit_target: None,
            focus: ModelField::Id,
            status: None,
        }
    }

    fn n_rows(&self) -> usize {
        self.rows.len()
    }

    fn add_row_idx(&self) -> usize {
        self.n_rows()
    }

    fn max_cursor(&self) -> usize {
        self.add_row_idx()
    }

    /// Open the popup to add a brand-new manual entry. The row is
    /// committed to `rows` only on a valid save.
    fn begin_add(&mut self) {
        self.edit_target = None;
        self.id_buf = TextField::default();
        self.name_buf = TextField::default();
        self.context_buf = TextField::default();
        self.focus = ModelField::Id;
        self.status = None;
        self.mode = ModelMode::Edit;
    }

    /// Open the popup to edit an existing manual entry. Fetched entries
    /// are not editable; the caller gates on `rows[i].manual`.
    fn begin_edit(&mut self, i: usize) {
        if let Some(row) = self.rows.get(i) {
            self.edit_target = Some(i);
            self.id_buf = TextField::new(row.id.clone());
            self.name_buf = TextField::new(row.name.clone().unwrap_or_default());
            self.context_buf = TextField::new(
                row.context_length
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
            );
            self.focus = ModelField::Id;
            self.status = None;
            self.mode = ModelMode::Edit;
        }
    }

    /// Validate the popup buffers and, if valid, commit them to `rows`.
    /// Returns `Err(message)` on validation failure (kept open) and
    /// `Ok(())` on a successful commit (popup closed).
    fn commit_edit(&mut self) -> Result<(), String> {
        let id = self.id_buf.text().trim().to_string();
        if id.is_empty() {
            return Err("model id cannot be empty".to_string());
        }
        // Reject a duplicate id within this provider, ignoring the row
        // being edited so a no-op id keeps validating.
        let dup = self
            .rows
            .iter()
            .enumerate()
            .any(|(i, m)| m.id == id && Some(i) != self.edit_target);
        if dup {
            return Err(format!("a model with id `{id}` already exists"));
        }
        let name_raw = self.name_buf.text().trim();
        let name = if name_raw.is_empty() {
            None
        } else {
            Some(name_raw.to_string())
        };
        let context_raw = self.context_buf.text().trim();
        let context_length = if context_raw.is_empty() {
            None
        } else {
            match context_raw.parse::<u32>() {
                Ok(n) => Some(n),
                Err(_) => return Err("context length must be a number".to_string()),
            }
        };

        match self.edit_target {
            Some(i) => {
                if let Some(row) = self.rows.get_mut(i) {
                    row.id = id;
                    row.name = name;
                    row.context_length = context_length;
                    self.cursor = i;
                }
            }
            None => {
                self.rows.push(ModelEntry {
                    id,
                    name,
                    thinking_modes: Vec::new(),
                    inputs: None,
                    context_length,
                    favorite: false,
                    manual: true,
                    cache: None,
                    shrink: None,
                    context: None,
                    mode: None,
                    extra: Default::default(),
                });
                self.cursor = self.rows.len() - 1;
            }
        }
        self.edit_target = None;
        self.status = None;
        self.mode = ModelMode::Browse;
        Ok(())
    }

    /// Close the popup without saving.
    fn cancel_edit(&mut self) {
        self.edit_target = None;
        self.status = None;
        self.mode = ModelMode::Browse;
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> ModelResult {
        match self.mode {
            ModelMode::Browse => self.handle_browse_key(key),
            ModelMode::Edit => self.handle_edit_key(key),
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> ModelResult {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.cursor = crate::tui::nav::wrap_prev(self.cursor, self.max_cursor() + 1);
                self.status = None;
                ModelResult::Stay
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.cursor = crate::tui::nav::wrap_next(self.cursor, self.max_cursor() + 1);
                self.status = None;
                ModelResult::Stay
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                ModelResult::Back
            }
            KeyCode::Char('a') => {
                self.begin_add();
                ModelResult::Stay
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if self.cursor < self.rows.len() {
                    self.rows.remove(self.cursor);
                    if self.cursor > 0 && self.cursor >= self.rows.len() {
                        self.cursor -= 1;
                    }
                    self.status = None;
                }
                ModelResult::Stay
            }
            // `r` renames (id/name/context) — manual entries only, as before.
            KeyCode::Char('r') => {
                if self.cursor < self.rows.len() {
                    if self.rows[self.cursor].manual {
                        self.begin_edit(self.cursor);
                    } else {
                        self.status =
                            Some("fetched models can't be renamed (settings: enter)".to_string());
                    }
                }
                ModelResult::Stay
            }
            // Enter/l/→ opens the model-settings sub-dialog (every model) or
            // the add affordance on the synthetic row.
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if self.cursor < self.rows.len() {
                    ModelResult::OpenSettings(self.cursor)
                } else if self.cursor == self.add_row_idx() {
                    self.begin_add();
                    ModelResult::Stay
                } else {
                    ModelResult::Stay
                }
            }
            _ => ModelResult::Stay,
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) -> ModelResult {
        match key.code {
            KeyCode::Esc => {
                self.cancel_edit();
                ModelResult::Stay
            }
            KeyCode::Enter => {
                if let Err(msg) = self.commit_edit() {
                    self.status = Some(msg);
                }
                ModelResult::Stay
            }
            // Three fields cycled by Tab / Shift+Tab.
            KeyCode::Tab => {
                self.focus = match self.focus {
                    ModelField::Id => ModelField::Name,
                    ModelField::Name => ModelField::Context,
                    ModelField::Context => ModelField::Id,
                };
                ModelResult::Stay
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    ModelField::Id => ModelField::Context,
                    ModelField::Name => ModelField::Id,
                    ModelField::Context => ModelField::Name,
                };
                ModelResult::Stay
            }
            _ => {
                match self.focus {
                    ModelField::Id => {
                        self.id_buf.handle_key(key);
                    }
                    ModelField::Name => {
                        self.name_buf.handle_key(key);
                    }
                    ModelField::Context => {
                        self.context_buf.handle_key(key);
                    }
                }
                ModelResult::Stay
            }
        }
    }

    pub(super) fn rows(&self) -> &[ModelEntry] {
        &self.rows
    }

    pub(super) fn is_editing(&self) -> bool {
        matches!(self.mode, ModelMode::Edit)
    }
}

pub(super) struct FetchAllState {
    pub(super) providers: Vec<String>,
    pub(super) in_flight: Vec<FetchHandle>,
    pub(super) finished: Vec<FetchedSummary>,
    /// 0 = Keep (default), 1 = Remove, 2 = Save & close
    pub(super) cursor: usize,
    pub(super) dont_ask_again: bool,
    /// Aggregated set of (provider_id, missing_model_id) the user must rule on.
    pub(super) unlisted: Vec<(String, String)>,
}

impl FetchAllState {
    /// Kick off one background `/models` fetch per configured provider,
    /// reusing the same [`FetchHandle`] machinery the Add/Edit pages use.
    /// Providers whose request can't even be resolved (missing
    /// env/credentials) land directly in `finished` as an error so one
    /// bad provider never blocks the rest — `tick` drains the live
    /// handles as they complete.
    pub(super) fn spawn(providers: &crate::config::providers::ProvidersConfig) -> Self {
        let mut ids: Vec<String> = providers.providers.keys().cloned().collect();
        ids.sort();
        let mut in_flight = Vec::new();
        let mut finished = Vec::new();
        for id in &ids {
            let Some(entry) = providers.providers.get(id) else {
                continue;
            };
            match models_fetch::resolve_provider_request(id, entry) {
                Err(e) => finished.push(FetchedSummary {
                    provider_id: id.clone(),
                    outcome: Err(e.to_string()),
                }),
                Ok(_) => in_flight.push(FetchHandle::spawn(id.clone(), entry.clone())),
            }
        }
        Self {
            providers: ids,
            in_flight,
            finished,
            cursor: 0,
            dont_ask_again: false,
            unlisted: Vec::new(),
        }
    }

    /// True while at least one per-provider fetch is still running.
    pub(super) fn is_fetching(&self) -> bool {
        !self.in_flight.is_empty()
    }
}

pub(super) struct FetchedSummary {
    pub(super) provider_id: String,
    pub(super) outcome: Result<FetchOutcome, String>,
}

impl AddState {
    pub(super) fn new() -> Self {
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
    pub(super) fn new(provider_id: String, entry: ProviderEntry) -> Self {
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

// ── Handlers ─────────────────────────────────────────────────────────────

impl SettingsDialog {
    /// If the Add wizard is on the CodexLogin step and the device-flow
    /// background task has finished, finalize the provider entry.
    pub(super) fn advance_codex_login(&mut self) {
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
                    cache: Default::default(),
                    shrink: Default::default(),
                    context: Default::default(),
                    mode: None,
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

    pub(super) fn apply_fetch_result(
        &mut self,
        provider_id: &str,
        result: Result<FetchOutcome, String>,
    ) {
        let mut message = String::new();
        if let Some(entry) = self.config.providers.get_mut(provider_id) {
            match result {
                Ok(FetchOutcome::Models(models)) => {
                    entry.models =
                        crate::config::providers::merge_fetched_models(&entry.models, models);
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
            Page::Providers(ProvidersPage::Models { parent, .. }) => {
                // A refetch finished while the user is managing the model
                // list. The model editor owns the live (unsaved) rows, so
                // we don't touch them here — just record the outcome on
                // the parent so it surfaces when they return to Edit.
                parent.status = Some(message);
                parent.fetch = None;
            }
            Page::Providers(ProvidersPage::ModelSettings { parent, .. })
            | Page::Providers(ProvidersPage::ProviderSettings { parent, .. }) => {
                // Same as Models: the settings editors own their live state,
                // so just clear the in-flight handle and record the outcome.
                parent.status = Some(message);
                parent.fetch = None;
            }
            _ => {}
        }
    }

    /// Poll the in-flight handles of an active all-providers refetch.
    /// Each finished handle is removed from `in_flight`, its models are
    /// persisted into config (mirroring [`Self::apply_fetch_result`]'s
    /// `Models` arm), and its outcome is recorded in `finished`. When
    /// `in_flight` empties, the aggregated unlisted-models set is built
    /// so [`Self::render_fetch_all`] can show the Keep/Remove prompt.
    /// A per-provider failure is just an `Err` summary — it never aborts
    /// the others.
    pub(super) fn drain_fetch_all(&mut self) {
        let Page::Providers(ProvidersPage::FetchAll(s)) = &mut self.page else {
            return;
        };
        if s.in_flight.is_empty() {
            return;
        }

        // Collect the results of any handles that have completed, leaving
        // the still-running ones in place.
        let mut newly_done: Vec<FetchedSummary> = Vec::new();
        s.in_flight.retain(|handle| match handle.take() {
            Some(outcome) => {
                newly_done.push(FetchedSummary {
                    provider_id: handle.provider_id.clone(),
                    outcome,
                });
                false
            }
            None => true,
        });
        if newly_done.is_empty() {
            return;
        }

        // Persist successful fetches into config exactly as the
        // single-provider path does, then record every outcome.
        for summary in &newly_done {
            if let Ok(FetchOutcome::Models(models)) = &summary.outcome
                && let Some(entry) = self.config.providers.get_mut(&summary.provider_id)
            {
                entry.models =
                    crate::config::providers::merge_fetched_models(&entry.models, models.clone());
                entry.models_fetched_at = Some(Utc::now());
            }
        }
        let _ = self.save_config();

        let all_done = {
            let Page::Providers(ProvidersPage::FetchAll(s)) = &mut self.page else {
                return;
            };
            s.finished.extend(newly_done);
            s.in_flight.is_empty()
        };

        // Once every provider has reported, aggregate the set of
        // configured-but-unlisted models for the Keep/Remove prompt.
        // Done as a free function so it doesn't hold `self.page` and
        // `self.config` borrowed at once.
        if all_done {
            let unlisted = compute_unlisted(self);
            if let Page::Providers(ProvidersPage::FetchAll(s)) = &mut self.page {
                s.unlisted = unlisted;
            }
        }
    }

    pub(super) fn handle_providers_key(&mut self, key: KeyEvent) -> bool {
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
                // Row 0 is the synthetic `[refetch all models]` button;
                // provider rows are offset by one (1..=ids.len()).
                let ids: Vec<String> = self.config.providers.keys().cloned().collect();
                let row_count = ids.len() + 1;
                let provider_idx = cursor.checked_sub(1);
                let pressed_d = matches!(key.code, KeyCode::Char('d'));
                match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                        return Nav::Replace(Page::Root {
                            cursor: self.last_root_cursor,
                        });
                    }
                    KeyCode::Char('q') => return Nav::Close,
                    KeyCode::Up | KeyCode::Char('k') => {
                        *cursor = crate::tui::nav::wrap_prev(*cursor, row_count);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *cursor = crate::tui::nav::wrap_next(*cursor, row_count);
                    }
                    KeyCode::Char('a') => {
                        return Nav::Replace(Page::Providers(ProvidersPage::Add(AddState::new())));
                    }
                    // `R` triggers the all-providers refetch from anywhere
                    // on the list; Enter on the button row does the same.
                    KeyCode::Char('R') => {
                        return self.start_fetch_all();
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        if *cursor == 0 {
                            return self.start_fetch_all();
                        }
                        if let Some(idx) = provider_idx
                            && let Some(id) = ids.get(idx).cloned()
                            && let Some(entry) = self.config.providers.get(&id)
                        {
                            return Nav::Replace(Page::Providers(ProvidersPage::Edit(
                                EditState::new(id, entry.clone()),
                            )));
                        }
                    }
                    KeyCode::Char('d') => {
                        // Only arm/confirm when the cursor is on a
                        // provider row (not the refetch-all button).
                        let on_provider_row = provider_idx.is_some_and(|i| i < ids.len());
                        if !on_provider_row {
                            // Drop through to the post-match cleanup.
                        } else if *delete_pending {
                            let id = ids[provider_idx.unwrap()].clone();
                            self.config.providers.remove(&id);
                            let msg = match self.save_config() {
                                Ok(()) => format!("deleted `{id}`"),
                                Err(e) => format!("delete failed: {e}"),
                            };
                            let new_len = self.config.providers.len();
                            // Keep the cursor on a valid provider row (or
                            // the button if none remain).
                            let new_cursor = (*cursor).min(new_len);
                            return Nav::Replace(Page::Providers(ProvidersPage::List {
                                cursor: new_cursor,
                                status: Some(msg),
                                delete_pending: false,
                            }));
                        } else {
                            *delete_pending = true;
                            *status = Some(format!(
                                "press d again to delete `{}`",
                                ids[provider_idx.unwrap()]
                            ));
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
            ProvidersPage::Models { editor, parent } => self.handle_models_key(key, editor, parent),
            ProvidersPage::ModelSettings {
                editor,
                models,
                parent,
            } => self.handle_model_settings_key(key, editor, models, parent),
            ProvidersPage::ProviderSettings { editor, parent } => {
                self.handle_provider_settings_key(key, editor, parent)
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
                    *cursor = crate::tui::nav::wrap_prev(*cursor, templates::TEMPLATES.len());
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *cursor = crate::tui::nav::wrap_next(*cursor, templates::TEMPLATES.len());
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
                    cache: Default::default(),
                    shrink: Default::default(),
                    context: Default::default(),
                    mode: None,
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
                            cache: Default::default(),
                            shrink: Default::default(),
                            context: Default::default(),
                            mode: None,
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
                            cache: Default::default(),
                            shrink: Default::default(),
                            context: Default::default(),
                            mode: None,
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
                        cache: Default::default(),
                        shrink: Default::default(),
                        context: Default::default(),
                        mode: None,
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
                s.cursor = crate::tui::nav::wrap_prev(s.cursor, EDIT_MENU_LEN);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                s.cursor = crate::tui::nav::wrap_next(s.cursor, EDIT_MENU_LEN);
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
                        // Hand off to the Models sub-page, moving the
                        // EditState out so the sub-page can return it
                        // intact on back (mirrors the Headers row).
                        let editor = ModelEditor::new(s.entry.models.clone());
                        let owned = std::mem::replace(
                            s,
                            EditState::new(String::new(), ProviderEntry::default()),
                        );
                        return Nav::Replace(Page::Providers(ProvidersPage::Models {
                            editor,
                            parent: Box::new(owned),
                        }));
                    }
                    3 => {
                        // Hand off to the provider-settings sub-page, moving
                        // the EditState out so it returns intact on back
                        // (mirrors the Headers/Models rows).
                        let settings = SettingsEditor::for_provider(&s.entry);
                        let owned = std::mem::replace(
                            s,
                            EditState::new(String::new(), ProviderEntry::default()),
                        );
                        return Nav::Replace(Page::Providers(ProvidersPage::ProviderSettings {
                            editor: settings,
                            parent: Box::new(owned),
                        }));
                    }
                    4 => {
                        let new = !s.entry.favorite.unwrap_or(false);
                        s.entry.favorite = if new { Some(true) } else { None };
                        s.status = Some(if new {
                            "marked as favorite".into()
                        } else {
                            "removed favorite".into()
                        });
                    }
                    5 => {
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
                    6 => {
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
                    7 => {
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

    /// Handle keys on the Models sub-page. All keys go to the
    /// [`ModelEditor`] until it signals `Back`; on back, copy the
    /// editor's rows into `parent.entry.models` and return to the Edit
    /// page with the parent intact (so its cursor, status, and any
    /// unsaved entry-level edits survive the round trip). The user still
    /// commits to disk with `s` on the Edit page, like every other edit.
    fn handle_models_key(
        &mut self,
        key: KeyEvent,
        editor: &mut ModelEditor,
        parent: &mut Box<EditState>,
    ) -> Nav {
        match editor.handle_key(key) {
            ModelResult::Stay => Nav::Stay,
            ModelResult::Back => {
                let rows = std::mem::take(&mut editor.rows);
                let mut owned = std::mem::replace(
                    parent.as_mut(),
                    EditState::new(String::new(), ProviderEntry::default()),
                );
                owned.entry.models = rows;
                // Put the cursor back on the Models row and prompt the
                // user to persist with `s`.
                owned.cursor = 2;
                owned.status = Some("models updated; press s to save".into());
                Nav::Replace(Page::Providers(ProvidersPage::Edit(owned)))
            }
            ModelResult::OpenSettings(idx) => {
                let Some(model_id) = editor.rows.get(idx).map(|m| m.id.clone()) else {
                    return Nav::Stay;
                };
                // Seed the settings editor from the provider entry carrying
                // the *live* (unsaved) model rows so inherited values resolve
                // correctly. The ModelEditor and parent are moved into the
                // sub-page so they're recalled intact on back.
                let mut seed_entry = parent.entry.clone();
                seed_entry.models = editor.rows.clone();
                let settings = SettingsEditor::for_model(&seed_entry, &model_id);
                let models = std::mem::replace(editor, ModelEditor::new(Vec::new()));
                let owned = std::mem::replace(
                    parent.as_mut(),
                    EditState::new(String::new(), ProviderEntry::default()),
                );
                Nav::Replace(Page::Providers(ProvidersPage::ModelSettings {
                    editor: settings,
                    models,
                    parent: Box::new(owned),
                }))
            }
        }
    }

    /// Handle keys on the model-settings sub-dialog
    /// (`prompts/model-provider-settings.md`). Keys go to the
    /// [`SettingsEditor`] until it signals `Back`; on back, write the model's
    /// override fields into the live model rows and return to the Models
    /// sub-page (which returns to Edit on its own back, where `s` persists).
    fn handle_model_settings_key(
        &mut self,
        key: KeyEvent,
        editor: &mut SettingsEditor,
        models: &mut ModelEditor,
        parent: &mut Box<EditState>,
    ) -> Nav {
        match editor.handle_key(key) {
            SettingsResult::Stay => Nav::Stay,
            SettingsResult::Back => {
                // Write the overrides into a provider entry carrying the live
                // model rows, then lift the updated rows back into the model
                // editor so the Models page sees them.
                let mut tmp = parent.entry.clone();
                tmp.models = std::mem::take(&mut models.rows);
                editor.write_into(&mut tmp);
                let mut owned = std::mem::replace(
                    parent.as_mut(),
                    EditState::new(String::new(), ProviderEntry::default()),
                );
                // Persist immediately: "editing a field and leaving the
                // dialog persists it" (`prompts/model-provider-settings.md`).
                // The model-row edit is a self-contained override write, so
                // we save rather than wait for the Edit page's `s`.
                owned.entry.models = tmp.models.clone();
                self.config
                    .providers
                    .insert(owned.provider_id.clone(), owned.entry.clone());
                owned.status = Some(super::save_status(self.save_config()).unwrap_or_default());
                let new_models = ModelEditor::new(tmp.models);
                Nav::Replace(Page::Providers(ProvidersPage::Models {
                    editor: new_models,
                    parent: Box::new(owned),
                }))
            }
        }
    }

    /// Handle keys on the provider-settings sub-dialog. Keys go to the
    /// [`SettingsEditor`] until it signals `Back`; on back, write the concrete
    /// values into `parent.entry` and return to the Edit page (where `s`
    /// persists), mirroring the Headers/Models round trip.
    fn handle_provider_settings_key(
        &mut self,
        key: KeyEvent,
        editor: &mut SettingsEditor,
        parent: &mut Box<EditState>,
    ) -> Nav {
        match editor.handle_key(key) {
            SettingsResult::Stay => Nav::Stay,
            SettingsResult::Back => {
                let mut owned = std::mem::replace(
                    parent.as_mut(),
                    EditState::new(String::new(), ProviderEntry::default()),
                );
                editor.write_into(&mut owned.entry);
                owned.cursor = 3;
                // Persist immediately on leaving the dialog
                // (`prompts/model-provider-settings.md`).
                self.config
                    .providers
                    .insert(owned.provider_id.clone(), owned.entry.clone());
                owned.status = Some(super::save_status(self.save_config()).unwrap_or_default());
                Nav::Replace(Page::Providers(ProvidersPage::Edit(owned)))
            }
        }
    }

    /// Enter the all-providers refetch flow, reusing the existing
    /// [`FetchAll`](ProvidersPage::FetchAll) page and its per-provider
    /// [`FetchHandle`] machinery. No-op (with a status) when no providers
    /// are configured; never stacks a second concurrent run because the
    /// only entry point is the List page and entering replaces it.
    fn start_fetch_all(&mut self) -> Nav {
        if self.config.providers.is_empty() {
            return Nav::Replace(Page::Providers(ProvidersPage::List {
                cursor: 0,
                status: Some("no providers configured".into()),
                delete_pending: false,
            }));
        }
        let state = FetchAllState::spawn(&self.config);
        Nav::Replace(Page::Providers(ProvidersPage::FetchAll(state)))
    }

    fn handle_fetch_all_key(&mut self, key: KeyEvent, s: &mut FetchAllState) -> Nav {
        // While the per-provider fetches are still running, the only
        // accepted key is Esc (cancel + return). The prompt rows aren't
        // live yet — `tick`/`drain_fetch_all` populates them once every
        // handle has reported.
        if s.is_fetching() {
            if matches!(key.code, KeyCode::Esc) {
                return Nav::Replace(Page::Providers(ProvidersPage::List {
                    cursor: 0,
                    status: Some("refetch-all cancelled".into()),
                    delete_pending: false,
                }));
            }
            return Nav::Stay;
        }

        // If the fetch finished but no model drifted out of the upstream
        // list, there's nothing to rule on — any key returns to the list
        // with a per-provider summary.
        if s.unlisted.is_empty() {
            return Nav::Replace(Page::Providers(ProvidersPage::List {
                cursor: 0,
                status: Some(fetch_all_summary(s)),
                delete_pending: false,
            }));
        }

        match key.code {
            KeyCode::Esc => {
                return Nav::Replace(Page::Providers(ProvidersPage::List {
                    cursor: 0,
                    status: Some("refetch-all cancelled".into()),
                    delete_pending: false,
                }));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                // 3 rows: confirm / cancel / "don't ask again".
                s.cursor = crate::tui::nav::wrap_prev(s.cursor, 3);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                s.cursor = crate::tui::nav::wrap_next(s.cursor, 3);
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
                let summary = fetch_all_summary(s);
                let _ = self.save_config();
                return Nav::Replace(Page::Providers(ProvidersPage::List {
                    cursor: 0,
                    status: Some(summary),
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
}

// ── Rendering ────────────────────────────────────────────────────────────

impl SettingsDialog {
    pub(super) fn render_providers_page(
        &self,
        frame: &mut Frame,
        area: Rect,
        page: &ProvidersPage,
    ) {
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
            ProvidersPage::Models { editor, parent } => {
                self.render_models_page(frame, area, editor, parent.as_ref())
            }
            ProvidersPage::ModelSettings { editor, parent, .. } => {
                self.render_settings_editor(frame, area, editor, parent.as_ref())
            }
            ProvidersPage::ProviderSettings { editor, parent } => {
                self.render_settings_editor(frame, area, editor, parent.as_ref())
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

        // Row 0: the `[refetch all models]` button. Provider rows follow
        // at cursor indices 1..=ids.len().
        let button_selected = cursor == 0;
        let button_style = if button_selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            muted
        };
        lines.push(Line::from(vec![
            Span::raw(if button_selected { "▸ " } else { "  " }),
            Span::styled("[refetch all models]".to_string(), button_style),
        ]));
        lines.push(Line::default());

        if ids.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no providers configured)".to_string(),
                muted,
            )));
        } else {
            let id_w = ids.iter().map(|s| s.chars().count()).max().unwrap_or(0);
            for (i, id) in ids.iter().enumerate() {
                let row = i + 1;
                let entry = self.config.providers.get(*id).unwrap();
                let marker = if row == cursor { "▸ " } else { "  " };
                let label = format!("{:<width$}", id, width = id_w);
                let star = if entry.favorite.unwrap_or(false) {
                    " ★"
                } else {
                    "  "
                };
                let style = if row == cursor && delete_pending {
                    red.add_modifier(Modifier::BOLD)
                } else if row == cursor {
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
        if matches!(s.step, AddStep::EditHeaders) && s.headers.is_editing() {
            render_header_edit_popup(frame, area, &s.headers);
        }
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
        let manual_count = s.entry.models.iter().filter(|m| m.manual).count();
        let models_summary = if manual_count > 0 {
            format!(
                "{} model(s) ({} manual)",
                s.entry.models.len(),
                manual_count
            )
        } else {
            format!("{} model(s)", s.entry.models.len())
        };
        let settings_summary = {
            let ctx = &s.entry.context;
            let mode = match s.entry.mode {
                Some(crate::config::extended::LlmMode::Defensive) => "defensive",
                Some(crate::config::extended::LlmMode::Normal) => "normal",
                None => "undefined",
            };
            format!(
                "compact {}% · prune {}%/{}% · cache {}s · mode {mode}",
                ctx.auto_compact_pct,
                ctx.auto_prune_pct,
                ctx.auto_prune_prunable_pct,
                s.entry.cache.ttl_secs,
            )
        };
        let rows = [
            ("URL", s.entry.url.clone()),
            ("Headers", headers_summary),
            ("Models", models_summary),
            ("Settings", settings_summary),
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
        if editor.is_editing() {
            render_header_edit_popup(frame, area, editor);
        }
    }

    /// Full-pane render for the Models sub-page. Lists every model row
    /// (fetched + manual) plus the `[+ add model]` affordance; the parent
    /// Edit state is recalled on back.
    fn render_models_page(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &ModelEditor,
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
        render_model_editor(&mut lines, editor);
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "a: add manual model   enter: edit manual   d: delete   esc: back".to_string(),
            muted,
        )));
        if let Some(status) = &editor.status {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                status.clone(),
                Style::default().fg(Color::Yellow),
            )));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        if editor.is_editing() {
            render_model_edit_popup(frame, area, editor);
        }
    }

    /// Full-pane render for the model/provider settings sub-dialog
    /// (`prompts/model-provider-settings.md`). Lists the seven fields with
    /// their working values; an inherited (non-overridden) model-scope field
    /// is dimmed with an `(inherited)` tag. The active numeric edit shows its
    /// buffer inline.
    fn render_settings_editor(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &SettingsEditor,
        parent: &EditState,
    ) {
        use super::settings_editor::SETTINGS_FIELD_COUNT;
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let scope_label = match &editor.scope {
            super::settings_editor::SettingsScope::Model { model_id } => {
                format!("{} › {}", parent.provider_id, model_id)
            }
            super::settings_editor::SettingsScope::Provider => parent.provider_id.clone(),
        };
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(vec![
                Span::styled("Settings: ", muted),
                Span::styled(scope_label, Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::default(),
        ];

        let fields = [
            SettingsField::AutoCompactPct,
            SettingsField::AutoPrunePct,
            SettingsField::AutoPrunePrunablePct,
            SettingsField::CacheTtlSecs,
            SettingsField::CacheMode,
            SettingsField::ShrinkStrategy,
            SettingsField::Mode,
        ];
        debug_assert_eq!(fields.len(), SETTINGS_FIELD_COUNT);
        let label_w = fields
            .iter()
            .map(|f| f.label().chars().count())
            .max()
            .unwrap_or(0);

        for (i, field) in fields.iter().enumerate() {
            let selected = i == editor.cursor;
            let marker = if selected { "▸ " } else { "  " };
            let label_style = if selected {
                yellow.add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let overridden = editor.is_overridden(*field);
            // While editing a numeric field, show the live buffer with a
            // caret; otherwise the formatted working value.
            let value = if editor.editing == Some(*field) {
                format!("{}▎", editor.buf.text())
            } else {
                editor.value_str(*field)
            };
            let value_style = if !overridden {
                muted
            } else if selected {
                Style::default().fg(Color::White)
            } else {
                muted
            };
            let mut spans = vec![
                Span::raw(marker),
                Span::styled(
                    format!("{:<width$}", field.label(), width = label_w),
                    label_style,
                ),
                Span::raw("  "),
                Span::styled(value, value_style),
            ];
            if !overridden {
                spans.push(Span::styled("  (inherited)".to_string(), muted));
            }
            lines.push(Line::from(spans));
        }

        lines.push(Line::default());
        if let Some(status) = &editor.status {
            lines.push(Line::from(Span::styled(status.clone(), yellow)));
        } else if matches!(
            editor.scope,
            super::settings_editor::SettingsScope::Model { .. }
        ) {
            lines.push(Line::from(Span::styled(
                "enter: edit/cycle   x: clear to inherit   h: back".to_string(),
                muted,
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "enter: edit/cycle   h: back".to_string(),
                muted,
            )));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn render_fetch_all(&self, frame: &mut Frame, area: Rect, s: &FetchAllState) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let green = Style::default().fg(Color::Green);
        let red = Style::default().fg(Color::Red);
        let mut lines: Vec<Line<'static>> = Vec::new();

        // Progress view while fetches are in flight, plus the running
        // per-provider results so the user sees outcomes land one by one.
        if s.is_fetching() {
            let done = s.finished.len();
            let total = done + s.in_flight.len();
            lines.push(Line::from(Span::styled(
                format!("Refetching /models for all providers… ({done}/{total})"),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::default());
            render_fetch_all_results(&mut lines, s, muted, green, red);
            lines.push(Line::default());
            lines.push(Line::from(Span::styled("esc: cancel".to_string(), muted)));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        }

        // Fetch complete with no drifted models: show the per-provider
        // summary and wait for a keypress to return.
        if s.unlisted.is_empty() {
            lines.push(Line::from(Span::styled(
                "Refetch complete.".to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::default());
            render_fetch_all_results(&mut lines, s, muted, green, red);
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "Press any key to return.".to_string(),
                muted,
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        }

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

// ── Free helpers ─────────────────────────────────────────────────────────

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
        let name_style = if cursor_here {
            yellow.add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::raw(marker.to_string()),
            Span::styled(format!("{:<width$}", row.name, width = name_w), name_style),
            Span::raw("  "),
            Span::styled(row.value.clone(), muted),
        ]));
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

/// Centered name/value popup for adding or editing a header. Drawn on
/// top of the header list when the editor is in `EditName`/`EditValue`
/// mode. The `Clear` widget wipes the cells underneath so the list
/// doesn't bleed through.
fn render_header_edit_popup(frame: &mut Frame, area: Rect, h: &HeaderEditor) {
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let yellow = Style::default().fg(Color::Yellow);

    let name_focus = matches!(h.mode, HeaderMode::EditName);

    let mut body: Vec<Line<'static>> = Vec::new();
    render_field_row(&mut body, "Name ", &h.name_buf, name_focus);
    render_field_row(&mut body, "Value", &h.value_buf, !name_focus);

    // Env-var status for the value (headers commonly reference `$VAR`).
    let resolved = envref::resolve(h.value_buf.text());
    if resolved.has_missing() {
        body.push(Line::from(Span::styled(
            format!(
                "  Environment variable not detected, make sure to set it: ${}",
                resolved.missing.join(", $")
            ),
            yellow,
        )));
    } else if !resolved.referenced.is_empty() {
        body.push(Line::from(Span::styled(
            format!(
                "  env var(s) detected: ${}",
                resolved.referenced.join(", $")
            ),
            muted,
        )));
    } else {
        body.push(Line::default());
    }
    body.push(Line::default());
    body.push(Line::from(Span::styled(
        "Tab: switch field   enter: save   esc: cancel".to_string(),
        muted,
    )));

    let title = if h.edit_target.is_some() {
        " Edit header "
    } else {
        " Add header "
    };
    let width = area.width.saturating_sub(6).clamp(24, 70);
    let height = (body.len() as u16) + 2; // +2 for the top/bottom border
    let rect = centered_rect(area, width, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(yellow)
        .title(title);
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), inner);
}

/// Render a [`ModelEditor`] as rows + `[+ add model]`. Each row shows the
/// model id, an `M` tag for manual entries, the display name, and the
/// context length when set. The active cursor row is highlighted.
fn render_model_editor(lines: &mut Vec<Line<'static>>, m: &ModelEditor) {
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let yellow = Style::default().fg(Color::Yellow);
    let green = Style::default().fg(Color::Green);
    lines.push(Line::from(Span::styled(
        "Models:".to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )));

    if m.rows().is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no models — add one by hand or refetch /models)".to_string(),
            muted,
        )));
    } else {
        let id_w = m
            .rows()
            .iter()
            .map(|r| r.id.chars().count())
            .max()
            .unwrap_or(0);
        for (i, row) in m.rows().iter().enumerate() {
            let cursor_here = m.cursor == i;
            let marker = if cursor_here { "  ▸ " } else { "    " };
            let id_style = if cursor_here {
                yellow.add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let tag = if row.manual { "M" } else { " " };
            let mut detail = row.name.clone().unwrap_or_default();
            if let Some(ctx) = row.context_length {
                if !detail.is_empty() {
                    detail.push_str("  ");
                }
                detail.push_str(&format!("ctx {ctx}"));
            }
            lines.push(Line::from(vec![
                Span::raw(marker.to_string()),
                Span::styled(format!("{tag} "), green),
                Span::styled(format!("{:<width$}", row.id, width = id_w), id_style),
                Span::raw("  "),
                Span::styled(detail, muted),
            ]));
        }
    }

    let add_idx = m.rows().len();
    let add_cursor = m.cursor == add_idx;
    let add_marker = if add_cursor { "  ▸ " } else { "    " };
    let add_style = if add_cursor {
        yellow.add_modifier(Modifier::BOLD)
    } else {
        muted
    };
    lines.push(Line::from(vec![
        Span::raw(add_marker.to_string()),
        Span::styled("[+ add model]".to_string(), add_style),
    ]));
}

/// Centered id/name/context popup for adding or editing a manual model.
/// Drawn on top of the model list while the editor is in `Edit` mode.
fn render_model_edit_popup(frame: &mut Frame, area: Rect, m: &ModelEditor) {
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let yellow = Style::default().fg(Color::Yellow);
    let red = Style::default().fg(Color::Red);

    let mut body: Vec<Line<'static>> = Vec::new();
    render_field_row(&mut body, "Id     ", &m.id_buf, m.focus == ModelField::Id);
    render_field_row(
        &mut body,
        "Name   ",
        &m.name_buf,
        m.focus == ModelField::Name,
    );
    render_field_row(
        &mut body,
        "Context",
        &m.context_buf,
        m.focus == ModelField::Context,
    );
    body.push(Line::default());
    if let Some(status) = &m.status {
        body.push(Line::from(Span::styled(format!("  {status}"), red)));
    } else {
        body.push(Line::from(Span::styled(
            "  id required · name falls back to id · context optional (number)".to_string(),
            muted,
        )));
    }
    body.push(Line::from(Span::styled(
        "  Tab: switch field   enter: save   esc: cancel".to_string(),
        muted,
    )));

    let title = if m.edit_target.is_some() {
        " Edit model "
    } else {
        " Add model "
    };
    let width = area.width.saturating_sub(6).clamp(24, 70);
    let height = (body.len() as u16) + 2; // +2 for the top/bottom border
    let rect = centered_rect(area, width, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(yellow)
        .title(title);
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), inner);
}

/// A `width`×`height` rect centered within `area`, clamped to fit.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
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

/// Render the per-provider outcome rows of an all-providers refetch:
/// `✓ provider — N model(s)`, `· provider — no /models endpoint`, or
/// `✗ provider — <error>`. Shared by the in-flight and completed views.
fn render_fetch_all_results(
    lines: &mut Vec<Line<'static>>,
    s: &FetchAllState,
    muted: Style,
    green: Style,
    red: Style,
) {
    for f in &s.finished {
        let (glyph, text, style) = match &f.outcome {
            Ok(FetchOutcome::Models(models)) => (
                "✓",
                format!("{} — {} model(s)", f.provider_id, models.len()),
                green,
            ),
            Ok(FetchOutcome::Unsupported) => (
                "·",
                format!("{} — no /models endpoint (skipped)", f.provider_id),
                muted,
            ),
            Err(e) => ("✗", format!("{} — {e}", f.provider_id), red),
        };
        lines.push(Line::from(vec![
            Span::raw(format!("  {glyph} ")),
            Span::styled(text, style),
        ]));
    }
}

/// One-line per-provider summary of a finished all-providers refetch:
/// how many succeeded, how many failed, and (when any did) the first
/// failing provider so the user has a thread to pull on.
fn fetch_all_summary(s: &FetchAllState) -> String {
    let total = s.finished.len();
    let failed: Vec<&FetchedSummary> = s.finished.iter().filter(|f| f.outcome.is_err()).collect();
    let ok = total - failed.len();
    if failed.is_empty() {
        format!("refetched /models for {ok}/{total} provider(s)")
    } else {
        let first = &failed[0];
        let reason = match &first.outcome {
            Err(e) => e.as_str(),
            Ok(_) => "",
        };
        format!(
            "refetched {ok}/{total} provider(s); {} failed (e.g. `{}`: {reason})",
            failed.len(),
            first.provider_id,
        )
    }
}

/// Build the (provider_id, model_id) set of configured models that are
/// absent from the freshly-fetched upstream list, across every provider
/// that reported a successful `Models` outcome in the active FetchAll.
fn compute_unlisted(dialog: &SettingsDialog) -> Vec<(String, String)> {
    let Page::Providers(ProvidersPage::FetchAll(s)) = &dialog.page else {
        return Vec::new();
    };
    let mut unlisted: Vec<(String, String)> = Vec::new();
    for summary in &s.finished {
        if let Ok(FetchOutcome::Models(remote)) = &summary.outcome
            && let Some(entry) = dialog.config.providers.get(&summary.provider_id)
        {
            for m in &entry.models {
                // Manual entries are intentionally absent from upstream —
                // they're retained by the merge, not "drifted out".
                if !m.manual && !remote.iter().any(|r| r.id == m.id) {
                    unlisted.push((summary.provider_id.clone(), m.id.clone()));
                }
            }
        }
    }
    unlisted
}

/// Build the `ProvidersPage` for `/model-settings`: the active model's
/// model-settings sub-dialog (`prompts/model-provider-settings.md`). Falls
/// back to the providers list with an inline status when no model is active
/// or the active (provider, model) can't be resolved in config.
pub(super) fn active_model_settings_page(
    config: &crate::config::providers::ProvidersConfig,
) -> ProvidersPage {
    let no_model = |msg: &str| ProvidersPage::List {
        cursor: 0,
        status: Some(msg.to_string()),
        delete_pending: false,
    };
    let Some(active) = config.active_model.as_ref() else {
        return no_model("no model selected — pick one with /model first");
    };
    let Some(entry) = config.providers.get(&active.provider) else {
        return no_model(&format!(
            "active provider `{}` not found in config",
            active.provider
        ));
    };
    if !entry.models.iter().any(|m| m.id == active.model) {
        return no_model(&format!(
            "active model `{}/{}` not found in config",
            active.provider, active.model
        ));
    }
    let settings = SettingsEditor::for_model(entry, &active.model);
    let models = ModelEditor::new(entry.models.clone());
    let parent = EditState::new(active.provider.clone(), entry.clone());
    ProvidersPage::ModelSettings {
        editor: settings,
        models,
        parent: Box::new(parent),
    }
}

pub(super) fn valid_url(s: &str) -> bool {
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
