//! `/settings → UI` and the `Instructions File` sub-page reached from it.
//!
//! UI page: vim mode, thinking display, markdown rendering toggles,
//! mouse capture, rich-text copy, name, packages dir, utility model. The
//! "instructions file" row at the bottom drills into the
//! [`InstructionsPage`] grab/reorder editor for
//! `extended.agent_guidance_files`.
//!
//! The `utility model` row edits `extended.utility_model`
//! (`"provider:model-id"`), the cheap model used for background work
//! (auto-titling §17d, skills auto-selection §5).

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::config::extended::{
    DefaultPrimaryAgent, InjectionThreshold, IsolationModeSetting, LlmMode, ThinkingDisplay,
    VimModeSetting,
};
use crate::config::providers::ProvidersConfig;
use crate::tui::textfield::TextField;
use crate::tui::theme::MUTED_COLOR_INDEX;

use super::reset::{ResetButton, ResetOutcome};
use super::{Nav, Page, SettingsDialog, save_status};

/// `/settings → UI` state.
pub(crate) struct UiPage {
    pub(super) cursor: usize,
    /// `Some(field)` when the user is inline-editing a text field.
    pub(super) editing: Option<UiField>,
    pub(super) buf: TextField,
    pub(super) status: Option<String>,
    /// Page-level "reset display toggles to defaults" confirm state (the
    /// last navigable row). Resets only [`crate::config::extended::TuiConfig`]
    /// — utility model, instructions, name, and packages dir are
    /// preserved.
    pub(super) reset: ResetButton,
    /// `Some` while the utility-model picker overlay is open. Replaces
    /// the page body until the user selects, types a custom id, clears,
    /// or cancels.
    /// Boxed: the picker (model list + text field) is much larger than
    /// the page's other fields, so inlining it inflates the `Dialog` /
    /// `Nav` enum variants (clippy::large_enum_variant).
    pub(super) utility_picker: Option<Box<UtilityModelPicker>>,
    /// Last value the user toggled the `mouse` setting to. The App
    /// reads this on dialog close to decide whether to push or pop
    /// crossterm's `EnableMouseCapture`. None = user didn't touch it.
    pub(crate) pending_mouse_capture: Option<bool>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) enum UiField {
    Name,
    PackagesDir,
    PlanBranchRoot,
    LoopGuardThreshold,
    InjectionCheckPrompt,
}

/// A single selectable model row in the utility-model picker, shown as
/// `provider:model-id` plus the human `name` when present. Built from
/// the configured providers in their natural order — no ranking.
#[derive(Clone)]
pub(super) struct UtilityModelEntry {
    pub(super) provider_id: String,
    pub(super) model_id: String,
    pub(super) display_name: Option<String>,
}

impl UtilityModelEntry {
    /// The stored form: `provider:model-id`.
    pub(super) fn value(&self) -> String {
        format!("{}:{}", self.provider_id, self.model_id)
    }
}

/// Number of model rows visible at once in the picker's scroll window.
const UTILITY_MODEL_WINDOW: usize = 10;

/// The utility-model picker overlay. Two modes:
///   - **List** — navigate the configured models (grouped by provider),
///     plus the synthetic `[clear]` and `[custom…]` actions.
///   - **Custom** — a free-text field for a `provider:model-id` not in
///     any provider's list (the fallback the spec requires).
///
/// Opens in Custom mode when there are no models to list, so the field
/// still works with an empty/unfetched config.
pub(super) struct UtilityModelPicker {
    /// Configured models in provider-grouped natural order.
    pub(super) entries: Vec<UtilityModelEntry>,
    /// `provider:model-id` currently stored, if any. Indicated in the
    /// list and pre-filled into the custom field.
    pub(super) current: Option<String>,
    pub(super) mode: PickerMode,
}

pub(super) enum PickerMode {
    /// Navigating the list. `cursor` indexes the synthetic navigable
    /// list (`[clear]`, `[custom…]`, then the model entries); `scroll`
    /// is the top of the visible window over the *model* entries.
    List { cursor: usize, scroll: usize },
    /// Typing a custom `provider:model-id`.
    Custom { buf: TextField },
}

/// Synthetic action rows that precede the model entries in List mode.
/// `[clear]` unsets the value; `[custom…]` switches to free-text entry.
const PICKER_ACTION_ROWS: usize = 2;
const PICKER_CLEAR_ROW: usize = 0;
const PICKER_CUSTOM_ROW: usize = 1;

impl UtilityModelPicker {
    /// Build the picker from the configured providers. Models are listed
    /// in provider order (the config's `BTreeMap` iteration), each
    /// provider's models in their stored order — no sort/rank. With no
    /// models configured the picker opens straight into free-text entry
    /// (pre-filled with the current value) so the field still works.
    pub(super) fn new(config: &ProvidersConfig, current: Option<String>) -> Self {
        let mut entries: Vec<UtilityModelEntry> = Vec::new();
        for (pid, entry) in &config.providers {
            for model in &entry.models {
                entries.push(UtilityModelEntry {
                    provider_id: pid.clone(),
                    model_id: model.id.clone(),
                    display_name: model.name.clone(),
                });
            }
        }
        let mode = if entries.is_empty() {
            PickerMode::Custom {
                buf: TextField::new(current.clone().unwrap_or_default()),
            }
        } else {
            // Pre-select the row matching the current value, if any;
            // otherwise land on the first model row (past the actions).
            let cursor = current
                .as_ref()
                .and_then(|cur| entries.iter().position(|e| &e.value() == cur))
                .map(|i| i + PICKER_ACTION_ROWS)
                .unwrap_or(PICKER_ACTION_ROWS);
            let scroll = crate::tui::app::windowed_scroll(
                cursor.saturating_sub(PICKER_ACTION_ROWS),
                0,
                entries.len(),
                UTILITY_MODEL_WINDOW,
            );
            PickerMode::List { cursor, scroll }
        };
        Self {
            entries,
            current,
            mode,
        }
    }
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

/// Labeled config rows on the UI page (vim mode, thinking, llm mode,
/// render-agent-markdown, render-user-markdown, mouse, rich-text-copy,
/// emojis, caffeinate display-awake, name, packages dir, utility model,
/// plan branch root, plan isolation, loop-guard threshold, default agent,
/// injection threshold, injection check-prompt, instructions file). The
/// `[reset to defaults]` button follows at cursor [`UI_CONFIG_ROWS`].
pub(super) const UI_CONFIG_ROWS: usize = 19;

/// Total navigable rows: the labeled config rows plus the trailing
/// `[reset to defaults]` button.
pub(super) const UI_ROWS: usize = UI_CONFIG_ROWS + 1;

/// Cursor index of the `[reset to defaults]` button (the last navigable
/// row).
pub(super) const UI_RESET_ROW: usize = UI_CONFIG_ROWS;

/// Cursor index of the `instructions file` drill-in row (the last config
/// row). The instructions sub-page's back-nav returns the UI cursor here.
pub(super) const UI_INSTRUCTIONS_ROW: usize = UI_CONFIG_ROWS - 1;

/// Recompute the model-entry scroll offset from a List-mode `cursor`
/// that includes the two synthetic action rows. The action rows live
/// above the window and never scroll; only the model entries do. When
/// the cursor is on an action row, the window stays pinned to the top.
fn picker_scroll(cursor: usize, scroll: usize, entries: usize) -> usize {
    let selected = cursor.saturating_sub(PICKER_ACTION_ROWS);
    crate::tui::app::windowed_scroll(selected, scroll, entries, UTILITY_MODEL_WINDOW)
}

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

fn cycle_llm_mode(m: LlmMode) -> LlmMode {
    m.toggled()
}

pub(super) fn isolation_mode_label(m: IsolationModeSetting) -> &'static str {
    match m {
        IsolationModeSetting::Worktree => {
            "worktree (default — one git worktree per parallel step + serial merge queue)"
        }
        IsolationModeSetting::SharedTree => {
            "shared_tree (one tree, serialized by the file-lock manager; no worktrees/merge queue)"
        }
    }
}

pub(super) fn default_primary_agent_label(a: DefaultPrimaryAgent) -> &'static str {
    match a {
        DefaultPrimaryAgent::Auto => {
            "auto (default — front-door router; converses, hands off to Plan/Build)"
        }
        DefaultPrimaryAgent::Build => "build (start on the coding agent — make the change now)",
        DefaultPrimaryAgent::Plan => "plan (start on the planning agent — author a plan)",
    }
}

pub(super) fn injection_threshold_label(t: InjectionThreshold) -> &'static str {
    match t {
        InjectionThreshold::Off => "off (default — no prompt-injection scanning)",
        InjectionThreshold::Low => "low (block prompts rated low or higher; needs a utility model)",
        InjectionThreshold::Medium => {
            "medium (block prompts rated medium or higher; needs a utility model)"
        }
        InjectionThreshold::High => "high (block only prompts rated high; needs a utility model)",
    }
}

pub(super) fn llm_mode_label(m: LlmMode) -> &'static str {
    match m {
        LlmMode::Defensive => {
            "defensive (default — explicit tool steering, more decomposition; for weaker models)"
        }
        LlmMode::Normal => {
            "normal (terse tool descriptions, episode sequencing; for strong models)"
        }
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
            utility_picker: None,
            pending_mouse_capture: None,
            reset: ResetButton::default(),
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
        if p.utility_picker.is_some() {
            self.handle_utility_picker_key(key, p);
            return Nav::Stay;
        }
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
                        UiField::PlanBranchRoot => {
                            // A blank value resets to the default rather
                            // than storing an empty prefix.
                            self.extended.plan_branch_root = if new.is_empty() {
                                "cockpit-plan".to_string()
                            } else {
                                new
                            };
                        }
                        UiField::LoopGuardThreshold => {
                            // Parse a positive integer; clamp to the
                            // minimum (2). A blank or unparseable value
                            // resets to the default rather than erroring —
                            // the field can't hold a nonsense threshold.
                            let parsed = new
                                .parse::<u32>()
                                .ok()
                                .map(|v| v.max(crate::config::extended::MIN_LOOP_GUARD_THRESHOLD))
                                .unwrap_or(crate::config::extended::MIN_LOOP_GUARD_THRESHOLD);
                            self.extended.loop_guard.repeat_threshold = parsed;
                        }
                        UiField::InjectionCheckPrompt => {
                            // A blank value resets to the user-authored
                            // default (unset) rather than storing an empty
                            // template.
                            self.extended.prompt_injection_guard.check_prompt =
                                if new.is_empty() { None } else { Some(new) };
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
                p.reset.disarm();
                p.cursor = crate::tui::nav::wrap_prev(p.cursor, rows);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                p.reset.disarm();
                p.cursor = crate::tui::nav::wrap_next(p.cursor, rows);
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
                    self.extended.llm_mode = cycle_llm_mode(self.extended.llm_mode);
                    p.status = save_status(self.save_extended());
                }
                3 => {
                    self.extended.tui.render_agent_markdown =
                        !self.extended.tui.render_agent_markdown;
                    p.status = save_status(self.save_extended());
                }
                4 => {
                    self.extended.tui.render_user_markdown =
                        !self.extended.tui.render_user_markdown;
                    p.status = save_status(self.save_extended());
                }
                5 => {
                    self.extended.tui.mouse_capture = !self.extended.tui.mouse_capture;
                    p.pending_mouse_capture = Some(self.extended.tui.mouse_capture);
                    p.status = save_status(self.save_extended());
                }
                6 => {
                    self.extended.tui.rich_text_copy = !self.extended.tui.rich_text_copy;
                    p.status = save_status(self.save_extended());
                }
                7 => {
                    self.extended.tui.use_emojis = !self.extended.tui.use_emojis;
                    p.status = save_status(self.save_extended());
                }
                8 => {
                    self.extended.tui.caffeinate_display_awake =
                        !self.extended.tui.caffeinate_display_awake;
                    p.status = save_status(self.save_extended());
                }
                9 => {
                    p.buf = TextField::new(self.extended.name.clone().unwrap_or_default());
                    p.editing = Some(UiField::Name);
                }
                10 => {
                    let cur = self
                        .extended
                        .packages_directory
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    p.buf = TextField::new(cur);
                    p.editing = Some(UiField::PackagesDir);
                }
                11 => {
                    p.utility_picker = Some(Box::new(UtilityModelPicker::new(
                        &self.config,
                        self.extended
                            .utility_model
                            .clone()
                            .filter(|s| !s.is_empty()),
                    )));
                    p.status = None;
                }
                12 => {
                    p.buf = TextField::new(self.extended.plan_branch_root.clone());
                    p.editing = Some(UiField::PlanBranchRoot);
                }
                13 => {
                    // Toggle the global default plan isolation mode (Q4c).
                    self.extended.default_isolation_mode =
                        self.extended.default_isolation_mode.toggled();
                    p.status = save_status(self.save_extended());
                }
                14 => {
                    p.buf =
                        TextField::new(self.extended.loop_guard.effective_threshold().to_string());
                    p.editing = Some(UiField::LoopGuardThreshold);
                }
                15 => {
                    // Cycle which primary agent new sessions start on
                    // (`auto` → `build` → `plan`, the auto-router feature).
                    self.extended.default_primary_agent =
                        self.extended.default_primary_agent.cycled();
                    p.status = save_status(self.save_extended());
                }
                16 => {
                    // Cycle the prompt-injection block threshold
                    // (`off` → `low` → `medium` → `high` → `off`).
                    self.extended.prompt_injection_guard.threshold =
                        self.extended.prompt_injection_guard.threshold.cycled();
                    p.status = save_status(self.save_extended());
                }
                17 => {
                    // Edit the injection check-prompt template. Pre-fill the
                    // current custom value; leave blank when unset so the
                    // user types a fresh template (blank keeps the default).
                    let cur = self
                        .extended
                        .prompt_injection_guard
                        .check_prompt
                        .clone()
                        .unwrap_or_default();
                    p.buf = TextField::new(cur);
                    p.editing = Some(UiField::InjectionCheckPrompt);
                }
                UI_INSTRUCTIONS_ROW => {
                    return Nav::Replace(Page::Instructions(InstructionsPage {
                        cursor: 0,
                        grabbed: None,
                        status: None,
                    }));
                }
                UI_RESET_ROW => {
                    // Page-level reset: arm on first activation, apply on
                    // the second. Resets only the display toggles
                    // (TuiConfig) — utility model, instructions, name, and
                    // packages dir are preserved.
                    if p.reset.activate() == ResetOutcome::Apply {
                        self.reset_ui_display_toggles(p);
                        p.status = save_status(self.save_extended());
                    } else {
                        p.status = None;
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Nav::Stay
    }

    /// Key handling while the utility-model picker overlay is open.
    /// Closes the overlay (returning to the page) on commit/clear/cancel.
    fn handle_utility_picker_key(&mut self, key: KeyEvent, p: &mut UiPage) {
        let Some(picker) = p.utility_picker.as_mut() else {
            return;
        };
        match &mut picker.mode {
            PickerMode::Custom { buf } => match key.code {
                KeyCode::Enter => {
                    let new = buf.text().trim().to_string();
                    // A blank custom entry clears the value (unset).
                    let value = if new.is_empty() { None } else { Some(new) };
                    self.commit_utility_model(p, value);
                }
                KeyCode::Esc => {
                    // From Custom mode, Esc backs out to the list when one
                    // exists; otherwise it closes the picker unchanged.
                    if picker.entries.is_empty() {
                        p.utility_picker = None;
                    } else {
                        let current = picker.current.clone();
                        let cursor = current
                            .as_ref()
                            .and_then(|cur| picker.entries.iter().position(|e| &e.value() == cur))
                            .map(|i| i + PICKER_ACTION_ROWS)
                            .unwrap_or(PICKER_ACTION_ROWS);
                        let scroll = crate::tui::app::windowed_scroll(
                            cursor.saturating_sub(PICKER_ACTION_ROWS),
                            0,
                            picker.entries.len(),
                            UTILITY_MODEL_WINDOW,
                        );
                        picker.mode = PickerMode::List { cursor, scroll };
                    }
                }
                _ => {
                    buf.handle_key(key);
                }
            },
            PickerMode::List { cursor, scroll } => {
                // List mode is a non-typing list: arrows and j/k navigate.
                let nav_len = PICKER_ACTION_ROWS + picker.entries.len();
                match key.code {
                    KeyCode::Esc => {
                        p.utility_picker = None;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *cursor = crate::tui::nav::wrap_prev(*cursor, nav_len);
                        *scroll = picker_scroll(*cursor, *scroll, picker.entries.len());
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *cursor = crate::tui::nav::wrap_next(*cursor, nav_len);
                        *scroll = picker_scroll(*cursor, *scroll, picker.entries.len());
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match *cursor {
                        PICKER_CLEAR_ROW => self.commit_utility_model(p, None),
                        PICKER_CUSTOM_ROW => {
                            let prefill = picker.current.clone().unwrap_or_default();
                            picker.mode = PickerMode::Custom {
                                buf: TextField::new(prefill),
                            };
                        }
                        idx => {
                            let value = picker
                                .entries
                                .get(idx - PICKER_ACTION_ROWS)
                                .map(|e| e.value());
                            if let Some(value) = value {
                                self.commit_utility_model(p, Some(value));
                            }
                        }
                    },
                    _ => {}
                }
            }
        }
    }

    /// Reset only the display toggles to their defaults: the whole
    /// [`crate::config::extended::TuiConfig`] (vim mode, thinking,
    /// agent/user markdown, mouse, rich-text copy, emojis, caffeinate
    /// display-awake, and the rest of the TUI block) goes back to
    /// `TuiConfig::default()`. Everything outside `TuiConfig` —
    /// `utility_model`, `instructions` (agent-guidance files), `name`,
    /// `packages_directory`, `llm_mode`, plan settings — is left
    /// untouched, per the UI-page reset contract.
    ///
    /// `pending_mouse_capture` is set so the App reconciles crossterm's
    /// mouse-capture mode on dialog close, exactly as a manual mouse
    /// toggle does.
    fn reset_ui_display_toggles(&mut self, p: &mut UiPage) {
        self.extended.tui = crate::config::extended::TuiConfig::default();
        p.pending_mouse_capture = Some(self.extended.tui.mouse_capture);
    }

    /// Persist the chosen utility model (or `None` to unset) and close
    /// the picker, reflecting saved status like every other UI-page edit.
    fn commit_utility_model(&mut self, p: &mut UiPage, value: Option<String>) {
        self.extended.utility_model = value;
        p.utility_picker = None;
        p.status = save_status(self.save_extended());
    }

    pub(super) fn render_ui_page(&self, frame: &mut Frame, area: Rect, p: &UiPage) {
        if let Some(picker) = &p.utility_picker {
            self.render_utility_picker(frame, area, picker);
            return;
        }
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let mut lines: Vec<Line<'static>> = Vec::new();

        lines.push(Line::from(Span::styled(
            "User-interface preferences".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        let rows: [(&str, String); 19] = [
            (
                "vim mode",
                vim_label(self.extended.tui.vim_mode).to_string(),
            ),
            (
                "thinking",
                thinking_label(self.extended.tui.thinking).to_string(),
            ),
            (
                "llm mode",
                llm_mode_label(self.extended.llm_mode).to_string(),
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
                "caffeinate display",
                bool_label(
                    self.extended.tui.caffeinate_display_awake,
                    "keep display on too (while /caffeinate is active)",
                    "system only (default — machine + lid-close, display may sleep)",
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
                "utility model",
                self.extended
                    .utility_model
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "(unset — provider:model-id)".to_string()),
            ),
            (
                "plan branch root",
                format!(
                    "{} (prefix for suggested plan branches: <root>/<feature>)",
                    self.extended.plan_branch_root
                ),
            ),
            (
                "plan isolation",
                isolation_mode_label(self.extended.default_isolation_mode).to_string(),
            ),
            (
                "loop-guard threshold",
                format!(
                    "{} (consecutive identical tool calls before approval prompt; 2 = first repeat)",
                    self.extended.loop_guard.effective_threshold()
                ),
            ),
            (
                "default agent",
                default_primary_agent_label(self.extended.default_primary_agent).to_string(),
            ),
            (
                "injection threshold",
                injection_threshold_label(self.extended.prompt_injection_guard.threshold)
                    .to_string(),
            ),
            (
                "injection check-prompt",
                if self.extended.prompt_injection_guard.check_prompt.is_some() {
                    "(custom)".to_string()
                } else {
                    "(default template)".to_string()
                },
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

        // `[reset to defaults]` button — the last navigable row. Resets
        // only the display toggles (TuiConfig); see `reset_ui_display_toggles`.
        lines.push(Line::default());
        lines.push(
            p.reset
                .render_line(p.cursor == UI_RESET_ROW, "reset display to defaults"),
        );

        if let Some(field) = p.editing {
            let prompt = match field {
                UiField::Name => "name: ",
                UiField::PackagesDir => "packages dir: ",
                UiField::PlanBranchRoot => "plan branch root: ",
                UiField::LoopGuardThreshold => "loop-guard threshold (>= 2): ",
                UiField::InjectionCheckPrompt => "injection check-prompt (blank = default): ",
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

    /// Render the utility-model picker overlay (replaces the page body).
    fn render_utility_picker(&self, frame: &mut Frame, area: Rect, picker: &UtilityModelPicker) {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let yellow = Style::default().fg(Color::Yellow);
        let mut lines: Vec<Line<'static>> = Vec::new();

        lines.push(Line::from(Span::styled(
            "Utility model — picks the cheap background model".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        match &picker.mode {
            PickerMode::Custom { buf } => {
                lines.push(Line::from(Span::styled(
                    "custom provider:model-id".to_string(),
                    muted,
                )));
                lines.push(Line::from(vec![
                    Span::styled("› ".to_string(), muted),
                    Span::styled(buf.text().to_string(), Style::default().fg(Color::White)),
                    Span::styled("▎".to_string(), Style::default().fg(Color::Yellow)),
                ]));
                lines.push(Line::default());
                if picker.entries.is_empty() {
                    // No-models path: hint where the list comes from.
                    lines.push(Line::from(Span::styled(
                        "No models fetched yet — type a provider:model-id, or fetch \
                         models from the Providers page."
                            .to_string(),
                        muted,
                    )));
                }
                lines.push(Line::from(Span::styled(
                    "enter: accept (blank clears)  esc: back".to_string(),
                    muted,
                )));
            }
            PickerMode::List { cursor, scroll } => {
                let cur_label = |value: &str| -> &'static str {
                    if picker.current.as_deref() == Some(value) {
                        "  (current)"
                    } else {
                        ""
                    }
                };
                // Synthetic action rows.
                let clear_active = *cursor == PICKER_CLEAR_ROW;
                let custom_active = *cursor == PICKER_CUSTOM_ROW;
                let action_style = |active: bool| {
                    if active {
                        yellow.add_modifier(Modifier::BOLD)
                    } else {
                        muted
                    }
                };
                let clear_suffix = if picker.current.is_none() {
                    "  (current)"
                } else {
                    ""
                };
                lines.push(Line::from(vec![
                    Span::raw(if clear_active { "▸ " } else { "  " }),
                    Span::styled(
                        format!("[clear — unset]{clear_suffix}"),
                        action_style(clear_active),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::raw(if custom_active { "▸ " } else { "  " }),
                    Span::styled(
                        "[custom provider:model-id…]".to_string(),
                        action_style(custom_active),
                    ),
                ]));

                // Model entries, grouped by provider in natural order,
                // with a one-row scroll window.
                let mut last_provider: Option<&str> = None;
                for (i, e) in picker
                    .entries
                    .iter()
                    .enumerate()
                    .skip(*scroll)
                    .take(UTILITY_MODEL_WINDOW)
                {
                    if last_provider != Some(e.provider_id.as_str()) {
                        lines.push(Line::from(Span::styled(
                            e.provider_id.clone(),
                            muted.add_modifier(Modifier::ITALIC),
                        )));
                        last_provider = Some(e.provider_id.as_str());
                    }
                    let active = *cursor == i + PICKER_ACTION_ROWS;
                    let marker = if active { "▸ " } else { "  " };
                    let label_style = if active {
                        yellow.add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    let value = e.value();
                    let mut spans = vec![
                        Span::raw(marker.to_string()),
                        Span::styled(value.clone(), label_style),
                    ];
                    if let Some(name) = &e.display_name {
                        spans.push(Span::raw("  "));
                        spans.push(Span::styled(name.clone(), muted));
                    }
                    let suffix = cur_label(&value);
                    if !suffix.is_empty() {
                        spans.push(Span::styled(suffix.to_string(), yellow));
                    }
                    lines.push(Line::from(spans));
                }

                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "↑/↓  enter: select  esc: cancel".to_string(),
                    muted,
                )));
            }
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
        // Navigable count = file rows + 1 synthetic `[+ add]` row at the
        // bottom (cursor `rows`).
        let nav_len = rows + 1;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Nav::Close,
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                return Nav::Replace(Page::Ui(UiPage {
                    cursor: UI_INSTRUCTIONS_ROW,
                    editing: None,
                    buf: TextField::default(),
                    status: None,
                    utility_picker: None,
                    pending_mouse_capture: None,
                    reset: ResetButton::default(),
                }));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                p.cursor = crate::tui::nav::wrap_prev(p.cursor, nav_len);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                p.cursor = crate::tui::nav::wrap_next(p.cursor, nav_len);
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
