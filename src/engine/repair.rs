//! Tool-input repair — the §12 catalog.
//!
//! The repair flow is: validate the JSON against the tool's schema, and
//! on failure walk a fixed catalog of one-step repairs at the *paths*
//! the schema disagreed at. Inputs that pass validation are dispatched
//! unchanged. This is the inverse of a preprocessing pass: we never
//! rewrite a clean input, we only rewrite what failed.
//!
//! For v0 the catalog is intentionally tiny — the two repairs we see
//! most in OS-model output (per the surveyed crashes in the project's
//! `tool-correction.txt` notes):
//!
//!   1. `null`-for-optional → omit the field.
//!   2. bare string where the schema wants an array → wrap in `[s]`.
//!
//! Adding `parse_stringified_array` and `wrap_single_arg` is one line
//! each, but every new repair must justify its existence against a
//! logged failure mode (see plan.md §12 "Observability"). We'll add as
//! evidence arrives.

use serde_json::{Map, Value};

/// What the catalog did. One row per dispatched tool call, persisted to
/// `tool_call_events.recovery_kind` + `recovery_stage` per GOALS §14.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// Args were already valid; no repair needed.
    Clean,
    /// A shape repair fired. `stage` is the catalog name
    /// (`null_for_optional`, `wrap_bare_string`, etc.).
    ShapeRepair { stage: &'static str, path: String },
    /// The `edit` cascade matched at a stage past `exact`. `stage` is the
    /// stage that matched (`line_trim`, `block_anchor`, …); `path` names
    /// the argument the cascade rewrote (always `"old_string"` for v0).
    /// See GOALS §13c.
    EditCascade { stage: &'static str, path: String },
}

impl Recovery {
    /// `(recovery_kind, recovery_stage)` for the session-DB row.
    pub fn db_fields(&self) -> (Option<&'static str>, Option<&'static str>) {
        match self {
            Recovery::Clean => (None, None),
            Recovery::ShapeRepair { stage, .. } => (Some("shape_repair"), Some(stage)),
            Recovery::EditCascade { stage, .. } => (Some("edit_cascade"), Some(stage)),
        }
    }
}

/// Known cascade stage names. Used by the audit-row reader to round-trip
/// `Recovery::EditCascade` without leaking strings.
pub const EDIT_CASCADE_STAGES: &[&str] = &[
    "exact",
    "line_trim",
    "block_anchor",
    "whitespace_normalized",
    "indent_flexible",
    "escape_normalized",
    "trimmed_boundary",
    "context_aware",
];

/// Known shape-repair stage names. Same purpose as `EDIT_CASCADE_STAGES`.
pub const SHAPE_REPAIR_STAGES: &[&str] = &["null_for_optional", "wrap_bare_string"];

/// Try the catalog against `args` given knowledge of the tool's expected
/// fields. For v0 we don't validate against a full schema — we just look
/// at field names the tool exposes and apply the catalog by inspection.
///
/// Returns `(repaired_args, recovery)`. The repaired value is what the
/// dispatcher should pass to `Tool::call`; the recovery is what gets
/// written to the session-DB row (`Clean` for no-op).
pub fn repair(args: &mut Value, tool_array_fields: &[&str]) -> Recovery {
    // Step 1: drop nulls anywhere in the top-level object. This handles
    // `null`-for-optional cleanly because every cockpit tool treats
    // missing optional fields the same as null.
    let mut removed_null_path: Option<String> = None;
    if let Value::Object(map) = args {
        let null_keys: Vec<String> = map
            .iter()
            .filter(|(_, v)| v.is_null())
            .map(|(k, _)| k.clone())
            .collect();
        if let Some(first) = null_keys.first() {
            removed_null_path = Some(first.clone());
        }
        for k in null_keys {
            map.remove(&k);
        }
    }

    // Step 2: wrap bare strings into single-element arrays for fields
    // the tool declared as `array<string>`.
    let mut wrapped_path: Option<String> = None;
    if let Value::Object(map) = args {
        for field in tool_array_fields {
            if let Some(v) = map.get_mut(*field)
                && let Value::String(_) = v
            {
                let s = std::mem::replace(v, Value::Null);
                *v = Value::Array(vec![s]);
                wrapped_path = Some((*field).to_string());
                break; // one repair at a time; reinvoke on next failure
            }
        }
    }

    if let Some(p) = wrapped_path {
        Recovery::ShapeRepair {
            stage: "wrap_bare_string",
            path: p,
        }
    } else if let Some(p) = removed_null_path {
        Recovery::ShapeRepair {
            stage: "null_for_optional",
            path: p,
        }
    } else {
        Recovery::Clean
    }
}

/// Convenience: ensure `args` is a JSON object. Tools see this shape;
/// stray scalars become `{ "value": <scalar> }` so the field-extraction
/// code doesn't have to special-case.
///
/// Not used by v0 tools (all of ours expect an object) but kept here so
/// the catalog can grow into it.
pub fn ensure_object(args: &mut Value) {
    if !args.is_object() {
        let taken = std::mem::replace(args, Value::Object(Map::new()));
        if let Value::Object(map) = args {
            map.insert("value".to_string(), taken);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clean_passes_through() {
        let mut v = json!({ "path": "/x" });
        let r = repair(&mut v, &[]);
        assert_eq!(r, Recovery::Clean);
        assert_eq!(v, json!({ "path": "/x" }));
    }

    #[test]
    fn null_for_optional_dropped() {
        let mut v = json!({ "path": "/x", "offset": null });
        let r = repair(&mut v, &[]);
        assert!(matches!(r, Recovery::ShapeRepair { stage: "null_for_optional", .. }));
        assert_eq!(v, json!({ "path": "/x" }));
    }

    #[test]
    fn bare_string_wrapped_for_array_field() {
        let mut v = json!({ "files": "src/main.rs" });
        let r = repair(&mut v, &["files"]);
        assert!(matches!(r, Recovery::ShapeRepair { stage: "wrap_bare_string", .. }));
        assert_eq!(v, json!({ "files": ["src/main.rs"] }));
    }

    #[test]
    fn ensure_object_wraps_scalar() {
        let mut v = json!("just a string");
        ensure_object(&mut v);
        assert_eq!(v, json!({ "value": "just a string" }));
    }
}
