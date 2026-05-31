//! The shared per-model / per-provider settings sub-dialog
//! (`prompts/model-provider-settings.md`).
//!
//! Both the model-settings and provider-settings sub-pages edit the **same**
//! seven-field set through one [`SettingsEditor`]. The only difference is the
//! scope ([`SettingsScope`]):
//!
//! - **Provider scope** edits the concrete `context` / `cache` / `shrink`
//!   values on the [`ProviderEntry`] (always present) plus its `mode`
//!   override.
//! - **Model scope** edits the `Option<…>` overrides on a single
//!   [`ModelEntry`]: each config group is either overridden (present) or
//!   inherits the provider value. Editing a field sets the override; `x`
//!   clears it back to inherit.
//!
//! The seven fields, in row order:
//!   1. Auto-compact ctx % (default 80)
//!   2. Auto-prune ctx % (default 50)
//!   3. Auto-prune prunable % (default 30)
//!   4. Cache time (seconds) (default 300)
//!   5. Cache mode (none | ephemeral)
//!   6. Shrink strategy (prune | compact)
//!   7. Mode (defensive | normal | undefined)
//!
//! Percentages 1–3 and the cache time are inline numeric text edits
//! (`Enter` opens the edit, validated/clamped on commit). Cache mode, shrink
//! strategy, and mode cycle in place on `Enter`. Back (`Esc`/`h`/`←`) writes
//! the working state into the parent [`EditState`]'s entry; the user still
//! commits to disk with `s` on the provider Edit page (model scope) or the
//! editor saves directly (see the handler).

use crossterm::event::{KeyCode, KeyEvent};

use crate::config::extended::LlmMode;
use crate::config::providers::{
    CacheConfig, CacheMode, ContextConfig, ModelEntry, ProviderEntry, ShrinkConfig, ShrinkStrategy,
};
use crate::tui::textfield::TextField;

/// Number of rows in the settings editor (the seven fields).
pub(super) const SETTINGS_FIELD_COUNT: usize = 7;

/// Which scope the editor is bound to.
#[derive(Clone)]
pub(super) enum SettingsScope {
    /// Editing a single model's `Option<…>` overrides. Carries the model id
    /// so the writeback can target the right row.
    Model { model_id: String },
    /// Editing the provider's concrete values.
    Provider,
}

/// The seven editable fields, in row order.
#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) enum SettingsField {
    AutoCompactPct,
    AutoPrunePct,
    AutoPrunePrunablePct,
    CacheTtlSecs,
    CacheMode,
    ShrinkStrategy,
    Mode,
}

impl SettingsField {
    fn from_row(row: usize) -> Self {
        match row {
            0 => Self::AutoCompactPct,
            1 => Self::AutoPrunePct,
            2 => Self::AutoPrunePrunablePct,
            3 => Self::CacheTtlSecs,
            4 => Self::CacheMode,
            5 => Self::ShrinkStrategy,
            _ => Self::Mode,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::AutoCompactPct => "Auto-compact ctx %",
            Self::AutoPrunePct => "Auto-prune ctx %",
            Self::AutoPrunePrunablePct => "Auto-prune prunable %",
            Self::CacheTtlSecs => "Cache time (seconds)",
            Self::CacheMode => "Cache mode",
            Self::ShrinkStrategy => "Shrink strategy",
            Self::Mode => "Mode",
        }
    }

    /// True for the inline numeric text-edit fields (the rest cycle).
    fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::AutoCompactPct
                | Self::AutoPrunePct
                | Self::AutoPrunePrunablePct
                | Self::CacheTtlSecs
        )
    }

    /// Which config group this field belongs to (for the model-scope
    /// override-present tracking).
    fn group(self) -> SettingsGroup {
        match self {
            Self::AutoCompactPct | Self::AutoPrunePct | Self::AutoPrunePrunablePct => {
                SettingsGroup::Context
            }
            Self::CacheTtlSecs | Self::CacheMode => SettingsGroup::Cache,
            Self::ShrinkStrategy => SettingsGroup::Shrink,
            Self::Mode => SettingsGroup::Mode,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum SettingsGroup {
    Context,
    Cache,
    Shrink,
    Mode,
}

/// The model/provider settings sub-dialog state.
pub(super) struct SettingsEditor {
    pub(super) scope: SettingsScope,
    pub(super) cursor: usize,
    /// Working concrete values. For model scope these are seeded from the
    /// override-or-provider-or-default chain so an inherited field shows its
    /// effective value; editing a field flips the group's `present` flag.
    context: ContextConfig,
    cache: CacheConfig,
    shrink: ShrinkConfig,
    /// `None` = mode undefined (inherit). Cycles defensive→normal→undefined.
    mode: Option<LlmMode>,
    /// Per-group "is this overridden on the model" flags. Always true for
    /// provider scope (the values are concrete). `mode` tracks override via
    /// `mode.is_some()` directly, so it has no flag here.
    context_present: bool,
    cache_present: bool,
    shrink_present: bool,
    /// Inline numeric edit buffer; `Some` while a numeric field is open.
    pub(super) editing: Option<SettingsField>,
    pub(super) buf: TextField,
    /// Transient validation status shown under the rows.
    pub(super) status: Option<String>,
}

impl SettingsEditor {
    /// Build the editor for a provider's concrete values.
    pub(super) fn for_provider(entry: &ProviderEntry) -> Self {
        Self {
            scope: SettingsScope::Provider,
            cursor: 0,
            context: entry.context.clone(),
            cache: entry.cache.clone(),
            shrink: entry.shrink.clone(),
            mode: entry.mode,
            context_present: true,
            cache_present: true,
            shrink_present: true,
            editing: None,
            buf: TextField::default(),
            status: None,
        }
    }

    /// Build the editor for a single model's overrides. Working values are
    /// seeded from the override if present, else the provider value, so an
    /// inherited field shows its effective (inherited) value.
    pub(super) fn for_model(entry: &ProviderEntry, model_id: &str) -> Self {
        let model = entry.models.iter().find(|m| m.id == model_id);
        let context = model
            .and_then(|m| m.context.clone())
            .unwrap_or_else(|| entry.context.clone());
        let cache = model
            .and_then(|m| m.cache.clone())
            .unwrap_or_else(|| entry.cache.clone());
        let shrink = model
            .and_then(|m| m.shrink.clone())
            .unwrap_or_else(|| entry.shrink.clone());
        let mode = model.and_then(|m| m.mode);
        Self {
            scope: SettingsScope::Model {
                model_id: model_id.to_string(),
            },
            cursor: 0,
            context,
            cache,
            shrink,
            mode,
            context_present: model.is_some_and(|m| m.context.is_some()),
            cache_present: model.is_some_and(|m| m.cache.is_some()),
            shrink_present: model.is_some_and(|m| m.shrink.is_some()),
            editing: None,
            buf: TextField::default(),
            status: None,
        }
    }

    fn is_model_scope(&self) -> bool {
        matches!(self.scope, SettingsScope::Model { .. })
    }

    /// Whether a field's group is currently an active override (model scope)
    /// — drives the "inherited" dimming. Always true for provider scope.
    pub(super) fn is_overridden(&self, field: SettingsField) -> bool {
        if !self.is_model_scope() {
            return true;
        }
        match field.group() {
            SettingsGroup::Context => self.context_present,
            SettingsGroup::Cache => self.cache_present,
            SettingsGroup::Shrink => self.shrink_present,
            SettingsGroup::Mode => self.mode.is_some(),
        }
    }

    /// The display value for a row (the working value, formatted).
    pub(super) fn value_str(&self, field: SettingsField) -> String {
        match field {
            SettingsField::AutoCompactPct => format!("{}%", self.context.auto_compact_pct),
            SettingsField::AutoPrunePct => format!("{}%", self.context.auto_prune_pct),
            SettingsField::AutoPrunePrunablePct => {
                format!("{}%", self.context.auto_prune_prunable_pct)
            }
            SettingsField::CacheTtlSecs => format!("{}s", self.cache.ttl_secs),
            SettingsField::CacheMode => match self.cache.mode {
                CacheMode::None => "none".to_string(),
                CacheMode::Ephemeral => "ephemeral".to_string(),
            },
            SettingsField::ShrinkStrategy => match self.shrink.strategy {
                ShrinkStrategy::Prune => "prune".to_string(),
                ShrinkStrategy::Compact => "compact".to_string(),
            },
            SettingsField::Mode => match self.mode {
                Some(LlmMode::Defensive) => "defensive".to_string(),
                Some(LlmMode::Normal) => "normal".to_string(),
                None => "undefined".to_string(),
            },
        }
    }

    fn mark_present(&mut self, field: SettingsField) {
        match field.group() {
            SettingsGroup::Context => self.context_present = true,
            SettingsGroup::Cache => self.cache_present = true,
            SettingsGroup::Shrink => self.shrink_present = true,
            SettingsGroup::Mode => {} // mode tracks override via Option
        }
    }

    /// Clear the field's group back to inherit (model scope only). On
    /// provider scope this is a no-op (no inherit state).
    fn clear_override(&mut self, field: SettingsField) {
        if !self.is_model_scope() {
            self.status = Some("provider settings can't inherit (model scope only)".to_string());
            return;
        }
        match field.group() {
            SettingsGroup::Context => self.context_present = false,
            SettingsGroup::Cache => self.cache_present = false,
            SettingsGroup::Shrink => self.shrink_present = false,
            SettingsGroup::Mode => self.mode = None,
        }
        self.status = Some("cleared to inherit".to_string());
    }

    /// Cycle a non-numeric field in place.
    fn cycle(&mut self, field: SettingsField) {
        match field {
            SettingsField::CacheMode => {
                self.cache.mode = match self.cache.mode {
                    CacheMode::None => CacheMode::Ephemeral,
                    CacheMode::Ephemeral => CacheMode::None,
                };
                self.mark_present(field);
            }
            SettingsField::ShrinkStrategy => {
                self.shrink.strategy = match self.shrink.strategy {
                    ShrinkStrategy::Prune => ShrinkStrategy::Compact,
                    ShrinkStrategy::Compact => ShrinkStrategy::Prune,
                };
                self.mark_present(field);
            }
            SettingsField::Mode => {
                // defensive → normal → undefined → defensive
                self.mode = match self.mode {
                    Some(LlmMode::Defensive) => Some(LlmMode::Normal),
                    Some(LlmMode::Normal) => None,
                    None => Some(LlmMode::Defensive),
                };
            }
            _ => {}
        }
        self.status = None;
    }

    fn begin_numeric_edit(&mut self, field: SettingsField) {
        let current = match field {
            SettingsField::AutoCompactPct => self.context.auto_compact_pct.to_string(),
            SettingsField::AutoPrunePct => self.context.auto_prune_pct.to_string(),
            SettingsField::AutoPrunePrunablePct => self.context.auto_prune_prunable_pct.to_string(),
            SettingsField::CacheTtlSecs => self.cache.ttl_secs.to_string(),
            _ => String::new(),
        };
        self.buf = TextField::new(current);
        self.editing = Some(field);
        self.status = None;
    }

    /// Validate + commit the numeric edit buffer. Percentages clamp to
    /// 0–100; the cache time accepts any non-negative integer. Non-numeric
    /// input is rejected inline (the field stays open).
    fn commit_numeric_edit(&mut self) {
        let Some(field) = self.editing else {
            return;
        };
        let raw = self.buf.text().trim();
        let parsed: u64 = match raw.parse() {
            Ok(n) => n,
            Err(_) => {
                self.status = Some("must be a number".to_string());
                return;
            }
        };
        match field {
            SettingsField::AutoCompactPct => {
                self.context.auto_compact_pct = parsed.min(100) as u8;
                self.mark_present(field);
            }
            SettingsField::AutoPrunePct => {
                self.context.auto_prune_pct = parsed.min(100) as u8;
                self.mark_present(field);
            }
            SettingsField::AutoPrunePrunablePct => {
                self.context.auto_prune_prunable_pct = parsed.min(100) as u8;
                self.mark_present(field);
            }
            SettingsField::CacheTtlSecs => {
                self.cache.ttl_secs = parsed;
                self.mark_present(field);
            }
            _ => {}
        }
        self.editing = None;
        self.status = None;
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> SettingsResult {
        // Inline numeric edit owns input until Enter/Esc.
        if self.editing.is_some() {
            match key.code {
                KeyCode::Enter => self.commit_numeric_edit(),
                KeyCode::Esc => {
                    self.editing = None;
                    self.status = None;
                }
                _ => {
                    self.buf.handle_key(key);
                }
            }
            return SettingsResult::Stay;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                SettingsResult::Back
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.cursor = crate::tui::nav::wrap_prev(self.cursor, SETTINGS_FIELD_COUNT);
                self.status = None;
                SettingsResult::Stay
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.cursor = crate::tui::nav::wrap_next(self.cursor, SETTINGS_FIELD_COUNT);
                self.status = None;
                SettingsResult::Stay
            }
            KeyCode::Char('x') => {
                self.clear_override(SettingsField::from_row(self.cursor));
                SettingsResult::Stay
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let field = SettingsField::from_row(self.cursor);
                if field.is_numeric() {
                    self.begin_numeric_edit(field);
                } else {
                    self.cycle(field);
                }
                SettingsResult::Stay
            }
            _ => SettingsResult::Stay,
        }
    }

    /// Write the working state back into `entry`, respecting the scope's
    /// override semantics. Called on Back so the parent Edit page carries the
    /// edits (committed to disk by the caller).
    pub(super) fn write_into(&self, entry: &mut ProviderEntry) {
        match &self.scope {
            SettingsScope::Provider => {
                entry.context = self.context.clone();
                entry.cache = self.cache.clone();
                entry.shrink = self.shrink.clone();
                entry.mode = self.mode;
            }
            SettingsScope::Model { model_id } => {
                // Ensure the row exists (it always should — the editor was
                // opened from it), then set the Option overrides per group.
                if let Some(m) = entry.models.iter_mut().find(|m| &m.id == model_id) {
                    apply_model_overrides(m, self);
                }
            }
        }
    }
}

/// Apply the editor's working state to a model row's `Option<…>` override
/// fields: a present group writes `Some(value)`, an absent group writes
/// `None` (inherit). `mode` writes its `Option` directly.
fn apply_model_overrides(m: &mut ModelEntry, e: &SettingsEditor) {
    m.context = if e.context_present {
        Some(e.context.clone())
    } else {
        None
    };
    m.cache = if e.cache_present {
        Some(e.cache.clone())
    } else {
        None
    };
    m.shrink = if e.shrink_present {
        Some(e.shrink.clone())
    } else {
        None
    };
    m.mode = e.mode;
}

pub(super) enum SettingsResult {
    Stay,
    Back,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_with_model() -> ProviderEntry {
        let mut entry = ProviderEntry {
            url: "https://x".into(),
            context: ContextConfig {
                auto_compact_pct: 85,
                auto_prune_pct: 55,
                auto_prune_prunable_pct: 35,
            },
            ..ProviderEntry::default()
        };
        entry.models.push(ModelEntry {
            id: "m1".into(),
            name: None,
            thinking_modes: vec![],
            inputs: None,
            context_length: Some(100_000),
            favorite: false,
            manual: false,
            cache: None,
            shrink: None,
            context: None,
            mode: None,
            extra: Default::default(),
        });
        entry
    }

    fn press(code: KeyCode) -> KeyEvent {
        use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn model_scope_seeds_from_inherited_then_overrides_on_edit() {
        let entry = provider_with_model();
        let mut e = SettingsEditor::for_model(&entry, "m1");
        // Inherited (no override yet) — shows the provider value, dimmed.
        assert_eq!(e.value_str(SettingsField::AutoCompactPct), "85%");
        assert!(!e.is_overridden(SettingsField::AutoCompactPct));
        // Edit the auto-compact %: open, type, commit.
        e.handle_key(press(KeyCode::Enter));
        e.buf = TextField::new("70".to_string());
        e.handle_key(press(KeyCode::Enter));
        assert_eq!(e.value_str(SettingsField::AutoCompactPct), "70%");
        assert!(e.is_overridden(SettingsField::AutoCompactPct));
        // Writeback sets the model override.
        let mut entry2 = entry.clone();
        e.write_into(&mut entry2);
        let m = entry2.models.iter().find(|m| m.id == "m1").unwrap();
        assert_eq!(m.context.as_ref().unwrap().auto_compact_pct, 70);
    }

    #[test]
    fn percentage_clamps_to_100_and_rejects_non_numeric() {
        let entry = provider_with_model();
        let mut e = SettingsEditor::for_provider(&entry);
        // Over 100 clamps.
        e.handle_key(press(KeyCode::Enter));
        e.buf = TextField::new("250".to_string());
        e.handle_key(press(KeyCode::Enter));
        assert_eq!(e.value_str(SettingsField::AutoCompactPct), "100%");
        // Non-numeric is rejected (field stays open, value unchanged).
        e.handle_key(press(KeyCode::Enter));
        e.buf = TextField::new("abc".to_string());
        e.handle_key(press(KeyCode::Enter));
        assert!(e.editing.is_some(), "field stays open on bad input");
        assert!(e.status.as_deref().unwrap_or("").contains("number"));
    }

    #[test]
    fn mode_cycles_defensive_normal_undefined() {
        let entry = provider_with_model();
        let mut e = SettingsEditor::for_provider(&entry);
        // Move to the Mode row (index 6).
        e.cursor = 6;
        assert_eq!(e.value_str(SettingsField::Mode), "undefined");
        e.handle_key(press(KeyCode::Enter));
        assert_eq!(e.value_str(SettingsField::Mode), "defensive");
        e.handle_key(press(KeyCode::Enter));
        assert_eq!(e.value_str(SettingsField::Mode), "normal");
        e.handle_key(press(KeyCode::Enter));
        assert_eq!(e.value_str(SettingsField::Mode), "undefined");
        // Writeback: undefined → None.
        let mut entry2 = entry.clone();
        e.write_into(&mut entry2);
        assert!(entry2.mode.is_none());
    }

    #[test]
    fn model_scope_clear_resets_to_inherit() {
        let entry = provider_with_model();
        let mut e = SettingsEditor::for_model(&entry, "m1");
        // Override the auto-compact %.
        e.handle_key(press(KeyCode::Enter));
        e.buf = TextField::new("70".to_string());
        e.handle_key(press(KeyCode::Enter));
        assert!(e.is_overridden(SettingsField::AutoCompactPct));
        // Clear it back to inherit with `x`.
        e.handle_key(press(KeyCode::Char('x')));
        assert!(!e.is_overridden(SettingsField::AutoCompactPct));
        let mut entry2 = entry.clone();
        e.write_into(&mut entry2);
        let m = entry2.models.iter().find(|m| m.id == "m1").unwrap();
        assert!(m.context.is_none(), "cleared override writes None");
    }
}
