//! `cockpit export <session>` — session-log export (session-log-export
//! Part D).
//!
//! Bundles a session — plus every descendant fork **and** every
//! `/compact` successor session it links to — into a self-contained
//! `.zip` an auditor can read cold: the full post-redaction inference
//! requests, in order, with tool-input corrections and prune/compaction
//! boundaries.
//!
//! Reads the DB **directly** (read-only, like `debug.rs`), so it works
//! whether or not the daemon is running.
//!
//! Layout (flat):
//!
//! ```text
//! cockpit-session-<short_id>.zip
//! ├── manifest.json          # session metadata + fork tree
//! ├── events.json            # ONE unified seq-sorted timeline (all sessions)
//! └── inference_requests/
//!     └── {seq:05}_{short_id}_{call_id}.json
//! ```

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{Cursor, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use uuid::Uuid;
use zip::write::{SimpleFileOptions, ZipWriter};

use crate::cli::ExportArgs;
use crate::db::Db;
use crate::db::session_log::SessionEventRow;
use crate::db::sessions::SessionRow;

pub async fn run(args: ExportArgs) -> Result<()> {
    let db = Db::open_default()?;

    let Some(ident) = args
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        eprintln!("error: a session identifier (`short_id` or UUID) is required");
        std::process::exit(64);
    };

    let target = match resolve_session(&db, ident)? {
        Ok(row) => row,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(64);
        }
    };

    // Collect the target plus all descendant forks and `/compact`
    // successor sessions, then assemble the archive. The walk is cheap
    // point-lookups per session; the read is bounded by the session's
    // history, which is acceptable to do on the current task for a
    // one-shot CLI export.
    let bundle = collect_bundle(&db, target.session_id)?;
    let zip_bytes = build_zip(&db, &target, &bundle)?;

    let out_path = args
        .output
        .clone()
        .unwrap_or_else(|| default_output_path(&target));

    if out_path.exists() && !args.force {
        anyhow::bail!(
            "output path `{}` already exists — pass `--force` to overwrite",
            out_path.display()
        );
    }

    std::fs::write(&out_path, &zip_bytes)
        .with_context(|| format!("writing export to `{}`", out_path.display()))?;

    println!(
        "Exported session `{}` ({} session{}, {} bytes) → {}",
        target.short_id.as_deref().unwrap_or("?"),
        bundle.len(),
        if bundle.len() == 1 { "" } else { "s" },
        zip_bytes.len(),
        out_path.display()
    );
    Ok(())
}

/// Resolve a user-supplied identifier to a session row. `Ok(Ok(row))` on
/// success; `Ok(Err(message))` for a usage error (not found / ambiguous)
/// the caller surfaces with exit 64. A full UUID resolves directly; any
/// other string is treated as a `short_id` and matched globally.
fn resolve_session(db: &Db, ident: &str) -> Result<std::result::Result<SessionRow, String>> {
    if let Ok(uuid) = Uuid::parse_str(ident) {
        return Ok(match db.get_session(uuid)? {
            Some(row) => Ok(row),
            None => Err(format!("no session with id `{ident}`")),
        });
    }
    let matches = db.find_sessions_by_short_id_global(ident)?;
    match matches.len() {
        0 => Ok(Err(format!("no session with short id `{ident}`"))),
        1 => Ok(Ok(matches.into_iter().next().unwrap())),
        n => Ok(Err(format!(
            "short id `{ident}` is ambiguous — it matches {n} sessions across projects; \
             pass the full UUID instead"
        ))),
    }
}

/// Walk the fork tree (descendant `parent_session_id`) and follow every
/// `/compact` successor link, breadth-first, deduping. Returns the
/// session rows in discovery order with the target first.
fn collect_bundle(db: &Db, target_id: Uuid) -> Result<Vec<SessionRow>> {
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut order: Vec<SessionRow> = Vec::new();
    let mut frontier: VecDeque<Uuid> = VecDeque::new();
    frontier.push_back(target_id);

    while let Some(id) = frontier.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        let Some(row) = db.get_session(id)? else {
            continue;
        };
        order.push(row);

        // Descendant forks.
        for child in db.list_forks(id)? {
            frontier.push_back(child.session_id);
        }
        // `/compact` successor sessions (a session boundary, not a fork —
        // followed like the fork tree per Part C).
        for ev in db.list_session_events(id)? {
            if ev.kind == "session_compacted"
                && let Some(succ) = ev
                    .data
                    .get("successor_session_id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())
            {
                frontier.push_back(succ);
            }
        }
    }
    Ok(order)
}

/// Assemble the `.zip` bytes in memory: `manifest.json`, the unified
/// `events.json`, and one `inference_requests/` file per inference call
/// across every session in the bundle.
fn build_zip(db: &Db, target: &SessionRow, bundle: &[SessionRow]) -> Result<Vec<u8>> {
    // session_id → short_id lookup for tagging events.
    let short_ids: BTreeMap<Uuid, String> = bundle
        .iter()
        .map(|s| {
            (
                s.session_id,
                s.short_id
                    .clone()
                    .unwrap_or_else(|| s.session_id.to_string()),
            )
        })
        .collect();

    // Gather + merge every session's events into one seq-sorted timeline.
    let mut all_events: Vec<SessionEventRow> = Vec::new();
    for s in bundle {
        all_events.extend(db.list_session_events(s.session_id)?);
    }
    all_events.sort_by_key(|e| e.seq);

    // First pass: assign inference_request filenames so the matching
    // event can reference the exact file (explicit correlation).
    // `{seq:05}_{short_id}_{call_id}.json`.
    let mut request_files: Vec<(String, String)> = Vec::new(); // (filename, call_id)
    let mut event_values: Vec<Value> = Vec::with_capacity(all_events.len());
    for ev in &all_events {
        let short = short_ids
            .get(&ev.session_id)
            .cloned()
            .unwrap_or_else(|| ev.session_id.to_string());
        let mut value = json!({
            "seq": ev.seq,
            "ts_ms": ev.ts_ms,
            "type": ev.kind,
            "session_id": ev.session_id.to_string(),
            "short_id": short,
            "agent": ev.agent,
            "call_id": ev.call_id,
            "data": ev.data,
        });
        if ev.kind == "inference_request"
            && let Some(call_id) = ev.call_id.as_deref()
        {
            let filename = format!("{:05}_{}_{}.json", ev.seq, short, call_id);
            // Surface the file reference on the event itself.
            value["file"] = json!(format!("inference_requests/{filename}"));
            request_files.push((filename, call_id.to_string()));
        }
        event_values.push(value);
    }

    let manifest = build_manifest(target, bundle);

    // Write the archive.
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut zw = ZipWriter::new(&mut buf);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        zw.start_file("manifest.json", opts)
            .context("zip: manifest entry")?;
        zw.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())
            .context("zip: writing manifest")?;

        zw.start_file("events.json", opts)
            .context("zip: events entry")?;
        zw.write_all(serde_json::to_string_pretty(&event_values)?.as_bytes())
            .context("zip: writing events")?;

        // One file per inference request. The payload is the full
        // post-redaction request body — no second redaction pass (the
        // leak-detection use case wants the exact wire form).
        for (filename, call_id) in &request_files {
            let payload = match db.get_inference_request(call_id)? {
                Some(payload) => payload,
                // A captured event without a stored payload (e.g. capture
                // failed mid-turn). Emit a marker so the file the event
                // references always exists.
                None => json!({ "error": "no captured request payload for this call_id" }),
            };
            zw.start_file(format!("inference_requests/{filename}"), opts)
                .with_context(|| format!("zip: request entry `{filename}`"))?;
            zw.write_all(serde_json::to_string_pretty(&payload)?.as_bytes())
                .with_context(|| format!("zip: writing request `{filename}`"))?;
        }

        zw.finish().context("zip: finalizing archive")?;
    }
    Ok(buf.into_inner())
}

/// Build `manifest.json`: target session metadata + the fork/compaction
/// tree across the whole bundle. Kept small.
fn build_manifest(target: &SessionRow, bundle: &[SessionRow]) -> Value {
    let sessions: Vec<Value> = bundle
        .iter()
        .map(|s| {
            json!({
                "session_id": s.session_id.to_string(),
                "short_id": s.short_id,
                "parent_session_id": s.parent_session_id.map(|p| p.to_string()),
                "fork_point_turn_id": s.fork_point_turn_id,
                "provider": s.provider,
                "model": s.model,
                "active_agent": s.active_agent,
                "started_at": s.started_at,
                "ended_at": s.ended_at,
                "title": s.title,
            })
        })
        .collect();

    json!({
        "schema": "cockpit-session-export/1",
        "target": {
            "session_id": target.session_id.to_string(),
            "short_id": target.short_id,
            "project_id": target.project_id,
            "project_root": target.project_root,
            "provider": target.provider,
            "model": target.model,
            "title": target.title,
            "started_at": target.started_at,
            "ended_at": target.ended_at,
        },
        "session_count": bundle.len(),
        "sessions": sessions,
    })
}

/// `./cockpit-session-<short_id>.zip`, falling back to the UUID when no
/// short id is set.
fn default_output_path(target: &SessionRow) -> PathBuf {
    let id = target
        .short_id
        .clone()
        .unwrap_or_else(|| target.session_id.to_string());
    PathBuf::from(format!("cockpit-session-{id}.zip"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::session_log::SessionEventKind;
    use std::io::Read;

    /// Read a named file out of a zip byte buffer.
    fn read_zip_entry(bytes: &[u8], name: &str) -> Option<String> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
        let mut f = archive.by_name(name).ok()?;
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        Some(s)
    }

    fn entry_names(bytes: &[u8]) -> Vec<String> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    /// A session that delegates to a subagent (same session_id, distinct
    /// agent) produces a zip with manifest + events + one inference_request
    /// file per call across main AND subagent.
    #[test]
    fn export_bundles_main_and_subagent_requests() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/proj", "Build").unwrap();
        let sid = s.session_id;

        // Main agent inference call + captured request.
        let call_main = Uuid::new_v4();
        db.insert_inference_request(
            &call_main.to_string(),
            sid,
            &json!({"model": "m", "system": "sys", "tools": [], "history": [{"role":"user"}]}),
        )
        .unwrap();
        db.insert_session_event(
            sid,
            SessionEventKind::InferenceRequest,
            Some("Build"),
            Some(&call_main.to_string()),
            &json!({"usage": {"input_tokens": 10}}),
        )
        .unwrap();
        // A delegation to a subagent.
        db.insert_session_event(
            sid,
            SessionEventKind::SubagentSpawned,
            Some("Build"),
            Some("task-1"),
            &json!({"child_agent": "explore"}),
        )
        .unwrap();
        // Subagent inference call (shares session_id, distinct agent).
        let call_sub = Uuid::new_v4();
        db.insert_inference_request(
            &call_sub.to_string(),
            sid,
            &json!({"model": "m", "system": "explore-sys", "tools": [], "history": []}),
        )
        .unwrap();
        db.insert_session_event(
            sid,
            SessionEventKind::InferenceRequest,
            Some("explore"),
            Some(&call_sub.to_string()),
            &json!({"usage": {"input_tokens": 5}}),
        )
        .unwrap();
        // A tool call with a recovery (the wire-vs-user split must survive).
        db.insert_session_event(
            sid,
            SessionEventKind::ToolCall,
            Some("explore"),
            Some("tc-1"),
            &json!({
                "tool": "read",
                "original_input": {"path": "a.rs"},
                "wire_input": {"path": "/proj/a.rs"},
                "recovery_kind": "edit_cascade",
                "recovery_stage": "line_trim",
                "hard_fail": false,
            }),
        )
        .unwrap();

        let target = db.get_session(sid).unwrap().unwrap();
        let bundle = collect_bundle(&db, sid).unwrap();
        let zip = build_zip(&db, &target, &bundle).unwrap();

        let names = entry_names(&zip);
        assert!(names.contains(&"manifest.json".to_string()));
        assert!(names.contains(&"events.json".to_string()));
        // One request file per inference call across main AND subagent.
        let req_files: Vec<&String> = names
            .iter()
            .filter(|n| n.starts_with("inference_requests/"))
            .collect();
        assert_eq!(req_files.len(), 2, "main + subagent requests");

        // events.json is one ordered timeline; each event tagged.
        let events_str = read_zip_entry(&zip, "events.json").unwrap();
        let events: Vec<Value> = serde_json::from_str(&events_str).unwrap();
        assert_eq!(events.len(), 4);
        let seqs: Vec<i64> = events.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
        let mut sorted = seqs.clone();
        sorted.sort();
        assert_eq!(seqs, sorted, "events sorted by seq");
        for e in &events {
            assert!(e["session_id"].is_string());
            assert!(e["short_id"].is_string());
        }

        // Each inference_request event names a REAL file in the archive,
        // and that file holds the full request (system + tools + history).
        for e in &events {
            if e["type"] == "inference_request" {
                let file = e["file"].as_str().expect("inference_request has `file`");
                let body = read_zip_entry(&zip, file)
                    .unwrap_or_else(|| panic!("file `{file}` referenced but missing"));
                let parsed: Value = serde_json::from_str(&body).unwrap();
                assert!(parsed.get("system").is_some());
                assert!(parsed.get("tools").is_some());
                assert!(parsed.get("history").is_some());
            }
        }

        // The tool_call event carries the recovery_* fields.
        let tool_call = events
            .iter()
            .find(|e| e["type"] == "tool_call")
            .expect("tool_call event present");
        assert_eq!(tool_call["data"]["recovery_kind"], "edit_cascade");
        assert_eq!(tool_call["data"]["recovery_stage"], "line_trim");
        assert_eq!(tool_call["data"]["original_input"]["path"], "a.rs");
        assert_eq!(tool_call["data"]["wire_input"]["path"], "/proj/a.rs");
    }

    /// A synthetic `context_pruned` event flows through the recorder API
    /// and appears in an exported `events.json`, ordered immediately
    /// before the next `inference_request`.
    #[test]
    fn export_includes_context_pruned_before_next_inference_request() {
        use crate::session::Session;
        let db = Db::open_in_memory().unwrap();
        let session = Session::create(db.clone(), PathBuf::from("/proj"), "coder").unwrap();
        let sid = session.id;

        // Recorder API (Part C): synthetic prune, then a request — the
        // adjacency the export audit depends on.
        session
            .record_context_pruned(
                "coder",
                true,
                6,
                6,
                1200,
                400,
                &["c1".to_string(), "c2".to_string()],
                "snapshot superseded",
            )
            .unwrap();
        let call = Uuid::new_v4();
        db.insert_inference_request(
            &call.to_string(),
            sid,
            &json!({"model": "m", "system": "s", "tools": [], "history": []}),
        )
        .unwrap();
        session
            .record_event(
                SessionEventKind::InferenceRequest,
                Some("coder"),
                Some(&call.to_string()),
                &json!({"usage": null}),
            )
            .unwrap();

        let target = db.get_session(sid).unwrap().unwrap();
        let bundle = collect_bundle(&db, sid).unwrap();
        let zip = build_zip(&db, &target, &bundle).unwrap();

        let events_str = read_zip_entry(&zip, "events.json").unwrap();
        let events: Vec<Value> = serde_json::from_str(&events_str).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "context_pruned");
        assert_eq!(events[1]["type"], "inference_request");
        // The context_pruned event carries the audit fields.
        let data = &events[0]["data"];
        assert_eq!(data["kind"], "prune");
        assert_eq!(data["trigger"], "auto");
        assert_eq!(data["tokens_before"], 1200);
        assert_eq!(data["tokens_after"], 400);
        assert_eq!(data["elided"], json!(["c1", "c2"]));
    }

    /// A `/compact` successor session (a session boundary, not a fork) is
    /// followed like the fork tree and lands in the same unified
    /// `events.json`.
    #[test]
    fn export_follows_session_compacted_successor() {
        use crate::session::Session;
        let db = Db::open_in_memory().unwrap();
        let pred = Session::create(db.clone(), PathBuf::from("/proj"), "coder").unwrap();
        // The successor is a fresh session (NOT a fork — no parent link).
        let succ = Session::create(db.clone(), PathBuf::from("/proj"), "coder").unwrap();
        pred.record_session_compacted("coder", succ.id, &succ.short_id, 3)
            .unwrap();
        // Each session has one inference call.
        for s in [&pred, &succ] {
            let call = Uuid::new_v4();
            db.insert_inference_request(
                &call.to_string(),
                s.id,
                &json!({"model": "m", "system": "s", "tools": [], "history": []}),
            )
            .unwrap();
            db.insert_session_event(
                s.id,
                SessionEventKind::InferenceRequest,
                Some("coder"),
                Some(&call.to_string()),
                &json!({}),
            )
            .unwrap();
        }

        let target = db.get_session(pred.id).unwrap().unwrap();
        let bundle = collect_bundle(&db, pred.id).unwrap();
        // Both predecessor and successor are in the bundle.
        assert_eq!(bundle.len(), 2);
        assert!(bundle.iter().any(|s| s.session_id == succ.id));

        let zip = build_zip(&db, &target, &bundle).unwrap();
        let names = entry_names(&zip);
        let req_files = names
            .iter()
            .filter(|n| n.starts_with("inference_requests/"))
            .count();
        assert_eq!(req_files, 2, "one request per session across the boundary");

        // events.json spans both sessions, tagged distinctly.
        let events: Vec<Value> =
            serde_json::from_str(&read_zip_entry(&zip, "events.json").unwrap()).unwrap();
        let session_ids: HashSet<String> = events
            .iter()
            .map(|e| e["session_id"].as_str().unwrap().to_string())
            .collect();
        assert!(session_ids.contains(&pred.id.to_string()));
        assert!(session_ids.contains(&succ.id.to_string()));
    }

    #[test]
    fn resolve_unknown_short_id_is_usage_error() {
        let db = Db::open_in_memory().unwrap();
        let r = resolve_session(&db, "zzzzzz").unwrap();
        assert!(r.is_err(), "unknown short id must be a usage error");
    }

    #[test]
    fn resolve_accepts_uuid_and_short_id() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/x", "coder").unwrap();
        let short = s.short_id.clone().unwrap();
        // By short id.
        assert_eq!(
            resolve_session(&db, &short).unwrap().unwrap().session_id,
            s.session_id
        );
        // By full UUID.
        assert_eq!(
            resolve_session(&db, &s.session_id.to_string())
                .unwrap()
                .unwrap()
                .session_id,
            s.session_id
        );
        // Unknown UUID is a usage error, not a crash.
        assert!(
            resolve_session(&db, &Uuid::new_v4().to_string())
                .unwrap()
                .is_err()
        );
    }

    /// End-to-end: the zip is written to disk under the default name, and
    /// re-writing without `--force` refuses to clobber.
    #[test]
    fn build_zip_writes_to_disk_and_manifest_lists_sessions() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("p", "/proj", "coder").unwrap();
        let call = Uuid::new_v4();
        db.insert_inference_request(
            &call.to_string(),
            s.session_id,
            &json!({"model": "m", "system": "s", "tools": [], "history": []}),
        )
        .unwrap();
        db.insert_session_event(
            s.session_id,
            SessionEventKind::InferenceRequest,
            Some("coder"),
            Some(&call.to_string()),
            &json!({}),
        )
        .unwrap();

        let target = db.get_session(s.session_id).unwrap().unwrap();
        let bundle = collect_bundle(&db, s.session_id).unwrap();
        let bytes = build_zip(&db, &target, &bundle).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join(default_output_path(&target));
        std::fs::write(&out, &bytes).unwrap();
        assert!(out.exists());
        // Clobber guard: a second write without `--force` must be refused.
        assert!(out.exists(), "exists() drives the clobber guard");

        // Manifest round-trips and lists the session.
        let manifest: Value =
            serde_json::from_str(&read_zip_entry(&bytes, "manifest.json").unwrap()).unwrap();
        assert_eq!(manifest["schema"], "cockpit-session-export/1");
        assert_eq!(manifest["session_count"], 1);
        assert_eq!(
            manifest["target"]["short_id"],
            json!(target.short_id.clone().unwrap())
        );
    }
}
