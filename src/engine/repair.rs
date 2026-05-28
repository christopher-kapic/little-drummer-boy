//! Tool-input repair — the §12 catalog (schema-driven validate-then-repair).
//!
//! The flow is the inverse of a preprocessing pass:
//!
//!   1. Compile the tool's own `parameters()` JSON Schema and **validate
//!      `args` as-is**. If it validates, the input is dispatched
//!      *untouched* (`Recovery::Clean`) — a clean input is never mutated.
//!   2. On failure, the validator hands us the exact *instance paths* it
//!      disagreed at. For each disagreeing path we walk a fixed catalog
//!      of one-step repairs, applying the single repair whose
//!      (expected-type-from-schema, actual-type, actual-value) signature
//!      matches at that path.
//!   3. We **re-validate**. Clean now → the repair succeeded. Still
//!      invalid → hard-fail with a model-readable retry message; we do
//!      not loop.
//!
//! Letting the validator complain first means the *schema* is the prior:
//! repair budget is spent only at the paths that actually disagreed, and
//! a `writeunlock` whose `content` happens to be JSON-shaped is never
//! rewritten because the schema never complained about it.
//!
//! ## The catalog (order is load-bearing)
//!
//!   1. `null_for_optional`     — a `null` value → omit the field
//!      (every cockpit tool treats missing == null for optionals).
//!   2. `parse_stringified_array` — a JSON *string* that parses to an
//!      array where the schema wants an array → the real array.
//!   3. `wrap_bare_string`      — a bare string where the schema wants an
//!      array → `[s]`.
//!   4. `markdown_autolink_unwrap` — a degenerate markdown auto-link in a
//!      schema-declared **path** field → the bare path.
//!
//! `parse_stringified_array` MUST precede `wrap_bare_string`: otherwise
//! `'["a","b"]'` would be wrapped into `['["a","b"]']` before the parse
//! stage ever sees it. Path fields are marked declaratively in each
//! tool's schema with `"x-cockpit-kind": "path"` (a non-prose annotation,
//! so token economy holds) and read back here — that plugs the
//! auto-link leak for every path field at once.
//!
//! ## Deferred — item 1c (`{}`-placeholder → array)
//!
//! The "empty placeholder" repair (a single arg wrapped in `{}` where the
//! schema wanted an array) is **deliberately not implemented**. Its exact
//! JSON shape is ambiguous in `tool-correction.txt`, and this module's
//! rule is that every repair must justify itself against a *logged*
//! failure mode. The `tool_input_invalid` telemetry event (emitted on
//! every unrecoverable failure) is the trigger: once it reveals the real
//! shape models emit, 1c lands as a fifth catalog stage between
//! `parse_stringified_array` and `wrap_bare_string`. No stub ships before
//! then.

use serde_json::Value;

/// Schema annotation marking a property whose value is a filesystem path.
/// Read by [`markdown_autolink_unwrap`]; it is a single keyword, not prose
/// (token economy, §10).
pub const PATH_KIND_KEY: &str = "x-cockpit-kind";
pub const PATH_KIND_VALUE: &str = "path";

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

/// Known shape-repair stage names, in catalog order. Same purpose as
/// `EDIT_CASCADE_STAGES`. Order here matches the order they're attempted
/// in [`repair`].
pub const SHAPE_REPAIR_STAGES: &[&str] = &[
    "null_for_optional",
    "parse_stringified_array",
    "wrap_bare_string",
    "markdown_autolink_unwrap",
];

/// Outcome of a validate-then-repair pass.
///
/// `recovery` is what gets persisted to the audit row (`Clean` when the
/// input validated as-is). `valid` is `true` when `args` validates after
/// any repair — the dispatcher proceeds to `Tool::call` only then.
/// `error` carries the model-readable diagnostic for the unrecoverable
/// case (`valid == false`); it's `None` on success.
#[derive(Debug)]
pub struct RepairOutcome {
    pub recovery: Recovery,
    pub valid: bool,
    pub error: Option<String>,
}

/// Validate `args` against `schema`; repair the disagreeing paths if it
/// fails; re-validate. See the module docs for the full contract.
///
/// `args` is mutated in place only when a repair fires. A clean input is
/// returned byte-for-byte unchanged with `Recovery::Clean`.
///
/// On a successful repair this emits a `tool_input_repaired` tracing
/// event; on an unrecoverable failure it emits `tool_input_invalid` and
/// returns `valid == false` with a model-readable `error`.
pub fn repair(args: &mut Value, schema: &Value, tool: &str) -> RepairOutcome {
    // A null/absent schema means "no declared shape" — nothing to
    // validate against, so the input is trivially clean.
    let validator = match compile(schema) {
        Some(v) => v,
        None => {
            return RepairOutcome {
                recovery: Recovery::Clean,
                valid: true,
                error: None,
            };
        }
    };

    // Step 1: validate as-is. Clean inputs are dispatched untouched.
    if validator.is_valid(args) {
        return RepairOutcome {
            recovery: Recovery::Clean,
            valid: true,
            error: None,
        };
    }

    // Step 2: walk the failing instance paths and repair at each. We take
    // the *first* repair that fires as the recorded recovery (one row,
    // one recovery — GOALS §14 keeps the single-Recovery shape) but keep
    // applying repairs at every disagreeing path so a call broken in two
    // places can still validate.
    let failing_paths = failing_top_level_keys(&validator, args);
    let mut primary: Option<(&'static str, String)> = None;
    for key in &failing_paths {
        if let Some(stage) = apply_one(args, schema, key)
            && primary.is_none()
        {
            primary = Some((stage, key.clone()));
        }
    }

    // Step 3: re-validate.
    if validator.is_valid(args) {
        if let Some((stage, path)) = primary {
            tracing::info!(
                target: "repair",
                tool = tool,
                stage = stage,
                path = %path,
                "tool_input_repaired"
            );
            return RepairOutcome {
                recovery: Recovery::ShapeRepair { stage, path },
                valid: true,
                error: None,
            };
        }
        // Re-validated clean but no catalog stage claimed credit (e.g.
        // the only fault was a stray null we stripped via a path the
        // catalog touched). Treat as clean.
        return RepairOutcome {
            recovery: Recovery::Clean,
            valid: true,
            error: None,
        };
    }

    // Unrecoverable: build a model-readable message naming what the schema
    // expected vs what arrived, emit the failure telemetry, and hard-fail.
    let msg = model_readable_error(&validator, args, tool);
    tracing::warn!(
        target: "repair",
        tool = tool,
        error = %msg,
        "tool_input_invalid"
    );
    RepairOutcome {
        recovery: Recovery::Clean,
        valid: false,
        error: Some(msg),
    }
}

/// Compile a tool's `parameters()` schema into a reusable validator.
/// Returns `None` for `null`/empty schemas (no shape to enforce) or if
/// the schema itself is malformed (a build error in a hand-authored
/// schema is a programming bug, not a model fault — we degrade to "no
/// validation" rather than reject every call).
fn compile(schema: &Value) -> Option<jsonschema::Validator> {
    if schema.is_null() {
        return None;
    }
    match jsonschema::validator_for(schema) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::error!(target: "repair", error = %e, "tool schema failed to compile");
            None
        }
    }
}

/// Collect the distinct top-level object keys the validator disagreed at,
/// in deterministic order. A `Required` error has an empty instance path
/// but names the missing property; a per-field error (wrong type, etc.)
/// has an instance path whose first segment is the field. We localize to
/// the top-level key because every cockpit tool takes a flat object —
/// nested repair would need a path-walk the catalog doesn't yet have.
fn failing_top_level_keys(validator: &jsonschema::Validator, args: &Value) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for err in validator.iter_errors(args) {
        if let Some(key) = first_path_segment(err.instance_path()) {
            if !keys.contains(&key) {
                keys.push(key);
            }
        } else if let jsonschema::error::ValidationErrorKind::Required { property } = err.kind()
            && let Some(name) = property.as_str()
            && !keys.contains(&name.to_string())
        {
            keys.push(name.to_string());
        }
    }
    keys
}

/// First property segment of an instance location, e.g. `/files` →
/// `"files"`. `None` for the root location or an index-rooted path.
fn first_path_segment(loc: &jsonschema::paths::Location) -> Option<String> {
    match loc.iter().next()? {
        jsonschema::paths::LocationSegment::Property(p) => Some(p.to_string()),
        jsonschema::paths::LocationSegment::Index(_) => None,
    }
}

/// Apply the one catalog repair whose signature matches at top-level
/// `key`, in catalog order. Returns the stage name that fired, or `None`
/// if no repair applied at this path.
fn apply_one(args: &mut Value, schema: &Value, key: &str) -> Option<&'static str> {
    let Value::Object(map) = args else {
        return None;
    };

    // 1. null_for_optional — strip a null value. (Validation only flags a
    //    null at a path that's typed non-null, so this is always the
    //    intended repair for a null at a disagreeing path.)
    if map.get(key).is_some_and(Value::is_null) {
        map.remove(key);
        return Some("null_for_optional");
    }

    let expects_array = schema_expects_array(schema, key);
    let is_path = schema_field_is_path(schema, key);

    // 2. parse_stringified_array — a JSON string that parses to an array,
    //    where the schema wants an array. MUST precede wrap_bare_string.
    if expects_array
        && let Some(Value::String(s)) = map.get(key)
        && let Ok(Value::Array(parsed)) = serde_json::from_str::<Value>(s)
    {
        map.insert(key.to_string(), Value::Array(parsed));
        return Some("parse_stringified_array");
    }

    // 3. wrap_bare_string — a bare string where the schema wants an array.
    if expects_array && let Some(v @ Value::String(_)) = map.get_mut(key) {
        let s = std::mem::replace(v, Value::Null);
        *v = Value::Array(vec![s]);
        return Some("wrap_bare_string");
    }

    // 4. markdown_autolink_unwrap — a degenerate auto-link in a path field.
    if is_path
        && let Some(Value::String(s)) = map.get(key)
        && let Some(unwrapped) = unwrap_degenerate_autolink(s)
    {
        map.insert(key.to_string(), Value::String(unwrapped));
        return Some("markdown_autolink_unwrap");
    }

    None
}

/// True when the schema declares property `key` as `type: "array"`.
fn schema_expects_array(schema: &Value, key: &str) -> bool {
    property_schema(schema, key)
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        == Some("array")
}

/// True when the schema marks property `key` with `x-cockpit-kind: path`.
fn schema_field_is_path(schema: &Value, key: &str) -> bool {
    property_schema(schema, key)
        .and_then(|p| p.get(PATH_KIND_KEY))
        .and_then(Value::as_str)
        == Some(PATH_KIND_VALUE)
}

/// The sub-schema for top-level property `key`, if the schema is an object
/// schema declaring it.
fn property_schema<'a>(schema: &'a Value, key: &str) -> Option<&'a Value> {
    schema.get("properties").and_then(|p| p.get(key))
}

/// Unwrap the degenerate markdown auto-link `[text](proto://text)` where
/// the link text equals the URL minus its protocol — the failure mode
/// where a model's chat-formatting prior leaks a path into a tool arg
/// (`[notes.md](http://notes.md)` → `notes.md`). A *real* link, where the
/// text differs from the URL (`[click](https://x.com)`), returns `None`
/// and passes through untouched. Returns `None` for anything that isn't
/// the degenerate shape.
fn unwrap_degenerate_autolink(s: &str) -> Option<String> {
    let s = s.trim();
    let rest = s.strip_prefix('[')?;
    let close = rest.find("](")?;
    let text = &rest[..close];
    let after = &rest[close + 2..];
    let url = after.strip_suffix(')')?;
    if text.is_empty() || url.is_empty() {
        return None;
    }
    // Strip the protocol (`scheme://`) from the URL, then compare.
    let url_no_proto = match url.split_once("://") {
        Some((_, tail)) => tail,
        None => url,
    };
    if url_no_proto == text {
        Some(text.to_string())
    } else {
        None
    }
}

/// Build the model-readable hard-fail message: name the first disagreeing
/// path, what the schema expected there, and what arrived. An `Error:`
/// prefix is correct here — this is a genuine invocation failure (distinct
/// from the soft, un-reddened relational-default Note the `read` tool
/// emits). The dispatcher wraps this in `invalid_input`.
fn model_readable_error(validator: &jsonschema::Validator, args: &Value, tool: &str) -> String {
    let first = validator.iter_errors(args).next();
    match first {
        Some(err) => {
            let loc = err.instance_path().as_str();
            let where_ = if loc.is_empty() {
                String::new()
            } else {
                format!(" at `{loc}`")
            };
            format!(
                "`{tool}` arguments failed schema validation{where_}: {err}. Re-emit the call with arguments matching the tool's schema."
            )
        }
        None => format!(
            "`{tool}` arguments failed schema validation. Re-emit the call with arguments matching the tool's schema."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A schema with one required path field, one optional integer, and
    /// one array-of-string field — enough surface to exercise every
    /// catalog stage.
    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string", "x-cockpit-kind": "path" },
                "offset": { "type": "integer" },
                "files":  { "type": "array", "items": { "type": "string" } }
            },
            "required": ["path"]
        })
    }

    #[test]
    fn clean_passes_through_untouched() {
        let mut v = json!({ "path": "/x" });
        let before = v.clone();
        let out = repair(&mut v, &schema(), "read");
        assert_eq!(out.recovery, Recovery::Clean);
        assert!(out.valid);
        // Enforce: clean input is never mutated.
        assert_eq!(v, before);
    }

    #[test]
    fn null_for_optional_dropped() {
        let mut v = json!({ "path": "/x", "offset": null });
        let out = repair(&mut v, &schema(), "read");
        assert!(out.valid);
        assert!(matches!(
            out.recovery,
            Recovery::ShapeRepair {
                stage: "null_for_optional",
                ..
            }
        ));
        assert_eq!(v, json!({ "path": "/x" }));
    }

    #[test]
    fn bare_string_wrapped_for_array_field() {
        let mut v = json!({ "path": "/x", "files": "src/main.rs" });
        let out = repair(&mut v, &schema(), "tool");
        assert!(out.valid);
        assert!(matches!(
            out.recovery,
            Recovery::ShapeRepair {
                stage: "wrap_bare_string",
                ..
            }
        ));
        assert_eq!(v["files"], json!(["src/main.rs"]));
    }

    /// The load-bearing ordering: a stringified array must parse to a real
    /// array, NOT get wrapped into `['["a","b"]']`.
    #[test]
    fn stringified_array_parsed_not_wrapped() {
        let mut v = json!({ "path": "/x", "files": "[\"a\",\"b\"]" });
        let out = repair(&mut v, &schema(), "tool");
        assert!(out.valid);
        assert!(matches!(
            out.recovery,
            Recovery::ShapeRepair {
                stage: "parse_stringified_array",
                ..
            }
        ));
        assert_eq!(v["files"], json!(["a", "b"]));
        // Crucially NOT the double-wrapped form.
        assert_ne!(v["files"], json!(["[\"a\",\"b\"]"]));
    }

    #[test]
    fn parse_stringified_array_stage_precedes_wrap_in_catalog() {
        // Ordering invariant pinned at the data level too.
        let parse_idx = SHAPE_REPAIR_STAGES
            .iter()
            .position(|s| *s == "parse_stringified_array")
            .unwrap();
        let wrap_idx = SHAPE_REPAIR_STAGES
            .iter()
            .position(|s| *s == "wrap_bare_string")
            .unwrap();
        assert!(parse_idx < wrap_idx);
    }

    #[test]
    fn markdown_autolink_degenerate_unwrapped() {
        // A degenerate auto-link in a path field is a *valid* string for
        // the schema, so validation wouldn't flag it on its own. Drive the
        // unwrap helper directly — that's the unit under test — and assert
        // the catalog wires it for path fields.
        assert_eq!(
            unwrap_degenerate_autolink("[notes.md](http://notes.md)").as_deref(),
            Some("notes.md")
        );
        assert_eq!(
            unwrap_degenerate_autolink("[src/x.rs](https://src/x.rs)").as_deref(),
            Some("src/x.rs")
        );
    }

    #[test]
    fn markdown_autolink_real_link_preserved() {
        // text != url-minus-protocol → not the degenerate case → untouched.
        assert_eq!(unwrap_degenerate_autolink("[click](https://x.com)"), None);
        assert_eq!(unwrap_degenerate_autolink("plain/path.rs"), None);
        assert_eq!(unwrap_degenerate_autolink("[a](http://b)"), None);
    }

    #[test]
    fn autolink_repair_fires_only_on_path_fields() {
        // `path` is marked x-cockpit-kind=path; the degenerate link is a
        // valid string so the schema won't flag it — apply_one is what the
        // dispatcher would call if another path forced a re-walk. Assert
        // the stage on a forced apply.
        let mut v = json!({ "path": "[notes.md](http://notes.md)" });
        let stage = apply_one(&mut v, &schema(), "path");
        assert_eq!(stage, Some("markdown_autolink_unwrap"));
        assert_eq!(v["path"], json!("notes.md"));
    }

    #[test]
    fn validate_then_repair_localizes_and_revalidates() {
        // Two faults in one call: a null optional and a bare-string array.
        let mut v = json!({ "path": "/x", "offset": null, "files": "a.rs" });
        let out = repair(&mut v, &schema(), "tool");
        assert!(out.valid, "should re-validate clean after repairs");
        // Both faults fixed.
        assert!(v.get("offset").is_none());
        assert_eq!(v["files"], json!(["a.rs"]));
    }

    #[test]
    fn unrecoverable_input_hard_fails_with_model_readable_message() {
        // `path` is required and there's no catalog repair that conjures a
        // missing required string — this is a genuine hard fail.
        let mut v = json!({ "offset": 5 });
        let out = repair(&mut v, &schema(), "read");
        assert!(!out.valid);
        let msg = out.error.expect("expected a hard-fail message");
        assert!(msg.contains("`read`"), "got: {msg}");
        assert!(msg.contains("schema validation"), "got: {msg}");
    }

    #[test]
    fn wrong_type_required_field_is_unrecoverable() {
        // A required path sent as an integer: no catalog repair turns an
        // int into a string, so this hard-fails cleanly (no panic/loop).
        let mut v = json!({ "path": 7 });
        let out = repair(&mut v, &schema(), "read");
        assert!(!out.valid);
        assert!(out.error.is_some());
    }

    #[test]
    fn null_schema_treats_everything_as_clean() {
        let mut v = json!({ "anything": [1, 2, 3] });
        let before = v.clone();
        let out = repair(&mut v, &Value::Null, "noargs");
        assert_eq!(out.recovery, Recovery::Clean);
        assert!(out.valid);
        assert_eq!(v, before);
    }
}
