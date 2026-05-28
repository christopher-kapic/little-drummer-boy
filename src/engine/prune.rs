//! Deterministic context pruning — snapshot dedup (`plan.md` T6.b/T6.d).
//!
//! The single rule that both the live "% prunable" projection
//! ([`dedup_plan`]) and the actual `/prune` execution ([`apply_plan`])
//! consume. Because they share one function, the figure the status line
//! shows always equals what `/prune` then removes — the stable-contract
//! property GOALS §1a / `plan.md` T6.d require.
//!
//! ## What it does
//!
//! For every snapshot-class tool call of *exact identity* (same
//! canonical path + identical args JSON), all but the most recent
//! result **body** is redundant given the newer one. We replace the
//! superseded body with a [`Part::Elided`] marker, keeping the
//! `tool_use`/`tool_result` **call shape** intact:
//!
//! - the assistant `ToolCall` is never touched;
//! - the `ToolResult` keeps its `id` + `call_id` (so the provider's
//!   tool_use↔tool_result pairing stays valid, and reasoning blocks
//!   that reference the earlier read still parse);
//! - only the `ToolResultContent::Text` body is rewritten to the
//!   marker string.
//!
//! ## Wire-only (GOALS §14)
//!
//! Elision touches the **model-bound** `Vec<Message>` history only. The
//! on-disk `tool_calls` rows and the TUI scrollback are driven by a
//! separate event stream and keep full fidelity, so the original body
//! is always recoverable (`cockpit session show`). The marker carries
//! the originating `call_id` as `original_event_id` to point a reader
//! at the full body.
//!
//! ## Snapshot-class tools
//!
//! `read` and the non-mutating codebase-intelligence tools
//! (`outline`, `symbol_find`, `word`, `deps`, `circular`, `tree`,
//! `search`). Deliberately excluded this pass (see `plan.md` T6.d):
//! `bash` (the command is interpretive context; classifying which
//! commands are snapshots is the hard problem), `edit`/`write` (their
//! args carry semantic content), and `hot` (a ranking, not a snapshot
//! of a single addressable resource).

use crate::config::providers::{CacheConfig, CacheMode};
use crate::engine::message::{AssistantContent, Message};
use rig::OneOrMany;
use rig::message::{ToolResultContent, UserContent};

/// Tools whose repeated identical calls produce a redundant snapshot
/// body. `read` plus the non-mutating intel tools. `hot`, `bash`,
/// `edit`, `write` are intentionally absent (see module docs).
pub const SNAPSHOT_TOOLS: &[&str] = &[
    "read",
    "outline",
    "symbol_find",
    "word",
    "deps",
    "circular",
    "tree",
    "search",
];

fn is_snapshot_tool(name: &str) -> bool {
    SNAPSHOT_TOOLS.contains(&name)
}

/// A reasoning-block / superseded snapshot body that has been removed
/// from the wire history. The single mechanism for body removal: it
/// rewrites a tool-result body, never a call's shape.
///
/// `original_event_id` is the originating tool call's `id` (the same
/// value the `tool_calls` row keys on), so a reader can recover the
/// full body from the on-disk transcript. `reason` is a terse,
/// human-readable explanation rendered into the marker text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elision {
    pub original_event_id: String,
    pub reason: &'static str,
}

impl Elision {
    /// The marker body the model sees in place of the elided snapshot.
    /// One line; terse (token economy §10). The newest identical call's
    /// full body is still in context, so the model can read it there.
    pub fn marker_text(&self) -> String {
        format!(
            "[elided: {} — superseded by a later identical call; full body in transcript event {}]",
            self.reason, self.original_event_id
        )
    }

    /// True when a tool-result body is already an elision marker (so we
    /// never double-count or re-elide it). Matches the `[elided: ` prefix
    /// `marker_text` emits.
    pub fn is_marker(body: &str) -> bool {
        body.starts_with("[elided:")
    }
}

/// One body to elide: its index in the history `Vec<Message>` plus the
/// marker to write there. Produced by [`dedup_plan`]; consumed by
/// [`apply_plan`] and the token-savings projection.
#[derive(Debug, Clone)]
pub struct ElisionTarget {
    /// Index into the `history` slice of the `Message::User` carrying the
    /// `ToolResult` to elide.
    pub history_index: usize,
    /// The current (full) body text at that index — used to compute the
    /// token savings without re-walking history.
    pub current_body: String,
    pub elision: Elision,
}

/// The deterministic plan: every superseded snapshot body that `/prune`
/// would elide, in history order. Empty when nothing is prunable.
#[derive(Debug, Clone, Default)]
pub struct DedupPlan {
    pub targets: Vec<ElisionTarget>,
}

impl DedupPlan {
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// cl100k_base token count that would be dropped from the wire by
    /// applying this plan. Each target trades its full body for the
    /// (small) marker, so the saving is `count(body) - count(marker)`,
    /// floored at zero.
    pub fn tokens_saved(&self) -> usize {
        self.targets
            .iter()
            .map(|t| {
                let before = crate::tokens::count(&t.current_body);
                let after = crate::tokens::count(&t.elision.marker_text());
                before.saturating_sub(after)
            })
            .sum()
    }
}

/// Walk `history` and build the dedup plan. The identity key is
/// `(tool_name, canonical_args)` where `canonical_args` is the
/// tool-call's argument JSON serialized canonically (serde_json's
/// `Value` ordering is stable for objects via `BTreeMap`-like sorting in
/// `to_string` only for `Map` insertion order, so we normalize through a
/// round-trip — see [`canonical_args`]). For each identity group we keep
/// the **last** body and mark every earlier one for elision.
///
/// Bodies already elided (marker text) are skipped — they neither get
/// re-elided nor count as "the surviving body" for a group. If the only
/// surviving (newest) body of a group is already elided, the older
/// bodies are left full: a marker pointing at a body no longer in
/// context would be a lie (`plan.md` T6.d edge case).
pub fn dedup_plan(history: &[Message]) -> DedupPlan {
    // First pass: map every assistant tool-call id → its identity key,
    // for the snapshot tools only.
    let mut call_identity: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for msg in history {
        if let Message::Assistant { content, .. } = msg {
            for c in content.iter() {
                if let AssistantContent::ToolCall(tc) = c
                    && is_snapshot_tool(&tc.function.name)
                {
                    let key = format!(
                        "{}\u{0}{}",
                        tc.function.name,
                        canonical_args(&tc.function.arguments)
                    );
                    call_identity.insert(tc.id.clone(), key);
                }
            }
        }
    }

    // Second pass: collect, per identity group, the history indices of
    // the (non-elided) tool-result bodies in order, plus their call id.
    struct ResultLoc {
        history_index: usize,
        call_id: String,
        body: String,
        elided: bool,
    }
    let mut groups: std::collections::HashMap<String, Vec<ResultLoc>> =
        std::collections::HashMap::new();

    for (idx, msg) in history.iter().enumerate() {
        if let Message::User { content } = msg {
            for c in content.iter() {
                if let UserContent::ToolResult(tr) = c {
                    let Some(key) = call_identity.get(&tr.id) else {
                        continue;
                    };
                    let body = tool_result_body(&tr.content);
                    let elided = Elision::is_marker(&body);
                    groups.entry(key.clone()).or_default().push(ResultLoc {
                        history_index: idx,
                        call_id: tr.id.clone(),
                        body,
                        elided,
                    });
                }
            }
        }
    }

    // Third pass: for each group with >1 result, keep the newest body
    // and elide the older non-elided ones — but only if the newest body
    // is still full (not already elided).
    let mut targets = Vec::new();
    for locs in groups.values() {
        if locs.len() < 2 {
            continue;
        }
        let newest = locs.last().expect("len >= 2");
        if newest.elided {
            // The surviving body is gone; a marker would point at
            // nothing. Leave the older bodies intact.
            continue;
        }
        for loc in &locs[..locs.len() - 1] {
            if loc.elided {
                continue;
            }
            targets.push(ElisionTarget {
                history_index: loc.history_index,
                current_body: loc.body.clone(),
                elision: Elision {
                    original_event_id: loc.call_id.clone(),
                    reason: "snapshot superseded",
                },
            });
        }
    }

    // Stable order: by history index so application + display agree.
    targets.sort_by_key(|t| t.history_index);
    DedupPlan { targets }
}

/// Apply the plan to `history` in place, replacing each targeted
/// tool-result body with its elision marker while preserving the
/// `ToolResult`'s `id`/`call_id` (the call shape). Returns the number of
/// bodies elided. Safe to call with a plan computed against the same
/// history; indices are validated defensively.
pub fn apply_plan(history: &mut [Message], plan: &DedupPlan) -> usize {
    let mut n = 0;
    for target in &plan.targets {
        let Some(msg) = history.get_mut(target.history_index) else {
            continue;
        };
        if let Message::User { content } = msg {
            for c in content.iter_mut() {
                if let UserContent::ToolResult(tr) = c
                    && tr.id == target.elision.original_event_id
                {
                    // Rewrite the body only; keep id/call_id intact so
                    // the tool_use↔tool_result pairing stays valid.
                    tr.content =
                        OneOrMany::one(ToolResultContent::text(target.elision.marker_text()));
                    n += 1;
                }
            }
        }
    }
    n
}

/// Convenience: compute and apply in one shot. Returns the plan that was
/// applied (so callers can report token savings / count).
pub fn prune_history(history: &mut [Message]) -> DedupPlan {
    let plan = dedup_plan(history);
    apply_plan(history, &plan);
    plan
}

/// The set of `original_event_id`s whose tool-result body is **currently**
/// an elision marker in the wire history. This is the cumulative live set
/// — every body that has been elided so far and not since restored —
/// derived by walking history rather than tracking deltas, so it tracks
/// the true wire state exactly even across multiple prunes and the
/// engine-fallback "keep full content" edge case (an un-elided body simply
/// isn't a marker, so it's absent here).
///
/// The TUI consumes this to dim the matching scrollback tool-result
/// bodies: a `ToolResult`'s `id` equals the originating tool call's `id`
/// (`apply_plan` preserves it), which is the same `call_id` the TUI keys
/// its rendered tool-call entries on. Render-time lookup, not a persisted
/// flag (GOALS §14: dimming is a wire-state view; scrollback stays
/// full-fidelity).
pub fn current_elided_ids(history: &[Message]) -> Vec<String> {
    let mut ids = Vec::new();
    for msg in history {
        if let Message::User { content } = msg {
            for c in content.iter() {
                if let UserContent::ToolResult(tr) = c
                    && Elision::is_marker(&tool_result_body(&tr.content))
                {
                    ids.push(tr.id.clone());
                }
            }
        }
    }
    ids
}

/// The cache-cold predicate (GOALS §10 / `plan.md` T6.f): "expected
/// cache-hit on the next call is zero." When this is true, pruning costs
/// no cache bust, so auto-prune may fire for free. Three cases, unified.
///
/// This is the clean public API other features reuse (auto-prune,
/// `/compact`'s prune-first step, the `/prune` confirm copy's hot/cold
/// label). Pure over its inputs so it's trivially testable.
///
/// Inputs:
/// - `cache`: the resolved per-(provider, model) cache config.
/// - `secs_since_last_send`: `None` ⇒ no warm prefix yet (cold).
/// - `upstream_bust`: the next call already invalidates the cache anchor
///   for an unrelated reason (a tool-result edit before the breakpoint,
///   a redaction/system-block mutation). Caller computes this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheState {
    /// A warm prefix is expected on the next call; pruning would bust it.
    Hot,
    /// No cache hit expected; pruning is free. Carries which case fired.
    Cold(ColdReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdReason {
    /// Provider has no prompt cache (`cache.mode = none`).
    NoCacheProvider,
    /// The cache TTL has elapsed since the last send (or no send yet).
    TtlElapsed,
    /// The next call already busts the cache upstream this turn.
    UpstreamBust,
}

impl CacheState {
    pub fn is_cold(self) -> bool {
        matches!(self, CacheState::Cold(_))
    }
}

/// Evaluate the cache-cold predicate. Order matters only for the
/// `ColdReason` attribution, not the boolean outcome.
pub fn cache_state(
    cache: &CacheConfig,
    secs_since_last_send: Option<u64>,
    upstream_bust: bool,
) -> CacheState {
    // Case (a): provider has no cache support at all.
    if cache.mode == CacheMode::None {
        return CacheState::Cold(ColdReason::NoCacheProvider);
    }
    // Case (c): the next call busts the cache upstream regardless of TTL.
    if upstream_bust {
        return CacheState::Cold(ColdReason::UpstreamBust);
    }
    // Case (b): TTL elapsed (or never sent → no warm prefix).
    match secs_since_last_send {
        None => CacheState::Cold(ColdReason::TtlElapsed),
        Some(secs) if secs >= cache.ttl_secs => CacheState::Cold(ColdReason::TtlElapsed),
        Some(_) => CacheState::Hot,
    }
}

/// Concatenate a tool-result's text content into one body string.
/// Images contribute nothing to the textual body (snapshot tools never
/// emit images anyway).
fn tool_result_body(content: &OneOrMany<ToolResultContent>) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Canonicalize a tool call's argument JSON so two structurally-equal
/// arg objects hash to the same identity key regardless of key order.
/// Round-trips through `serde_json::Value` with sorted object keys.
fn canonical_args(args: &serde_json::Value) -> String {
    fn sort_value(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => {
                let mut sorted = serde_json::Map::new();
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for k in keys {
                    sorted.insert(k.clone(), sort_value(&map[k]));
                }
                serde_json::Value::Object(sorted)
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(sort_value).collect())
            }
            other => other.clone(),
        }
    }
    sort_value(args).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::message::ToolCall;
    use rig::OneOrMany;
    use rig::message::{AssistantContent, ToolResult};
    use serde_json::json;

    /// Build an assistant message carrying one snapshot tool call.
    fn assistant_call(call_id: &str, tool: &str, args: serde_json::Value) -> Message {
        let tc = ToolCall {
            id: call_id.to_string(),
            call_id: None,
            function: rig::message::ToolFunction {
                name: tool.to_string(),
                arguments: args,
            },
            signature: None,
            additional_params: None,
        };
        Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::ToolCall(tc)),
        }
    }

    /// Build a user message carrying one tool result body.
    fn tool_result(call_id: &str, body: &str) -> Message {
        Message::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: call_id.to_string(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text(body)),
            })),
        }
    }

    fn body_at(history: &[Message], idx: usize) -> String {
        match &history[idx] {
            Message::User { content } => tool_result_body(match content.first_ref() {
                UserContent::ToolResult(tr) => &tr.content,
                _ => panic!("not a tool result"),
            }),
            _ => panic!("not a user message"),
        }
    }

    /// Two identical reads of the same file: the older body is elided,
    /// the newest survives, call shapes (the assistant turns) untouched.
    #[test]
    fn dedups_repeated_identical_reads() {
        let args = json!({ "path": "/abs/foo.rs" });
        let mut history = vec![
            assistant_call("c1", "read", args.clone()),
            tool_result("c1", "FULL BODY ONE with lots of content here"),
            assistant_call("c2", "read", args.clone()),
            tool_result("c2", "FULL BODY TWO with lots of content here"),
        ];

        let plan = dedup_plan(&history);
        assert_eq!(plan.targets.len(), 1, "older read elided, newer kept");
        assert_eq!(plan.targets[0].history_index, 1);
        assert_eq!(plan.targets[0].elision.original_event_id, "c1");

        let n = apply_plan(&mut history, &plan);
        assert_eq!(n, 1);
        // Older body became the marker; newer body intact.
        assert!(Elision::is_marker(&body_at(&history, 1)));
        assert_eq!(
            body_at(&history, 3),
            "FULL BODY TWO with lots of content here"
        );
        // Call shapes (assistant turns) are unchanged — still 4 messages,
        // assistant turns at 0 and 2.
        assert_eq!(history.len(), 4);
        assert!(matches!(history[0], Message::Assistant { .. }));
        assert!(matches!(history[2], Message::Assistant { .. }));
    }

    /// PROJECTION == EXECUTION: the same `dedup_plan` drives both the
    /// "% prunable" figure and the actual prune, so tokens_saved before
    /// applying equals the wire bytes that actually disappear.
    #[test]
    fn projection_equals_execution() {
        let args = json!({ "path": "/abs/big.rs" });
        let big = "x".repeat(4000);
        let mut history = vec![
            assistant_call("c1", "read", args.clone()),
            tool_result("c1", &big),
            assistant_call("c2", "read", args.clone()),
            tool_result("c2", &big),
        ];
        // The projection the status line would show.
        let projected = dedup_plan(&history);
        let projected_saving = projected.tokens_saved();
        assert!(projected_saving > 0);

        // Measure wire tokens before/after the ACTUAL prune.
        let before: usize = history.iter().map(wire_tokens).sum();
        let applied = prune_history(&mut history);
        let after: usize = history.iter().map(wire_tokens).sum();
        let actual_saving = before - after;

        // The plan used for projection and the plan applied are identical
        // (same function), so the saving the user was promised is the
        // saving they got.
        assert_eq!(applied.targets.len(), projected.targets.len());
        assert_eq!(projected_saving, actual_saving);
    }

    /// Different args (different offset) are NOT the same identity — no
    /// dedup.
    #[test]
    fn distinct_args_not_deduped() {
        let mut history = vec![
            assistant_call("c1", "read", json!({ "path": "/f", "offset": 1 })),
            tool_result("c1", "page one body padding padding"),
            assistant_call("c2", "read", json!({ "path": "/f", "offset": 200 })),
            tool_result("c2", "page two body padding padding"),
        ];
        let plan = dedup_plan(&history);
        assert!(plan.is_empty(), "different offsets are different snapshots");
        assert_eq!(apply_plan(&mut history, &plan), 0);
    }

    /// Key-order differences in args don't defeat identity matching.
    #[test]
    fn arg_key_order_is_canonicalized() {
        let mut history = vec![
            assistant_call("c1", "read", json!({ "path": "/f", "limit": 50 })),
            tool_result("c1", "body alpha padding padding padding"),
            assistant_call("c2", "read", json!({ "limit": 50, "path": "/f" })),
            tool_result("c2", "body beta padding padding padding"),
        ];
        let plan = dedup_plan(&history);
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(apply_plan(&mut history, &plan), 1);
    }

    /// bash / edit / write are not snapshot tools; repeated identical
    /// calls are never deduped.
    #[test]
    fn non_snapshot_tools_untouched() {
        let history = vec![
            assistant_call("c1", "bash", json!({ "command": "ls" })),
            tool_result("c1", "file listing body padding"),
            assistant_call("c2", "bash", json!({ "command": "ls" })),
            tool_result("c2", "file listing body padding"),
        ];
        let plan = dedup_plan(&history);
        assert!(plan.is_empty(), "bash is not a snapshot tool this pass");
    }

    /// Already-elided newest body → leave older bodies full (no marker
    /// pointing at nothing).
    #[test]
    fn newest_already_elided_keeps_older_full() {
        let args = json!({ "path": "/f" });
        let marker = Elision {
            original_event_id: "c2".into(),
            reason: "snapshot superseded",
        }
        .marker_text();
        let history = vec![
            assistant_call("c1", "read", args.clone()),
            tool_result("c1", "older full body padding padding"),
            assistant_call("c2", "read", args),
            tool_result("c2", &marker),
        ];
        let plan = dedup_plan(&history);
        assert!(
            plan.is_empty(),
            "surviving body is elided; older must stay full"
        );
    }

    /// Three identical reads: the two older bodies elide, the newest
    /// survives.
    #[test]
    fn three_reads_elides_two() {
        let args = json!({ "path": "/f" });
        let mut history = vec![
            assistant_call("c1", "read", args.clone()),
            tool_result("c1", "body one padding padding padding"),
            assistant_call("c2", "read", args.clone()),
            tool_result("c2", "body two padding padding padding"),
            assistant_call("c3", "read", args.clone()),
            tool_result("c3", "body three padding padding padding"),
        ];
        let plan = dedup_plan(&history);
        assert_eq!(plan.targets.len(), 2);
        assert_eq!(apply_plan(&mut history, &plan), 2);
        assert!(Elision::is_marker(&body_at(&history, 1)));
        assert!(Elision::is_marker(&body_at(&history, 3)));
        assert!(!Elision::is_marker(&body_at(&history, 5)));
    }

    /// `current_elided_ids` reflects the live wire state exactly: after a
    /// prune it returns the elided body's id; the kept newest body is
    /// absent; an un-pruned history yields nothing.
    #[test]
    fn current_elided_ids_tracks_wire_state() {
        let args = json!({ "path": "/abs/foo.rs" });
        let mut history = vec![
            assistant_call("c1", "read", args.clone()),
            tool_result("c1", "FULL BODY ONE with lots of content here"),
            assistant_call("c2", "read", args.clone()),
            tool_result("c2", "FULL BODY TWO with lots of content here"),
        ];
        // Nothing elided yet.
        assert!(current_elided_ids(&history).is_empty());

        prune_history(&mut history);
        let elided = current_elided_ids(&history);
        // Only the older body's id is elided; the kept newest is not.
        assert_eq!(elided, vec!["c1".to_string()]);
        assert!(!elided.contains(&"c2".to_string()));
    }

    #[test]
    fn cache_cold_three_cases() {
        let none = CacheConfig {
            mode: CacheMode::None,
            ttl_secs: 300,
        };
        let ephemeral = CacheConfig {
            mode: CacheMode::Ephemeral,
            ttl_secs: 300,
        };

        // (a) no-cache provider → cold regardless of timing.
        assert_eq!(
            cache_state(&none, Some(0), false),
            CacheState::Cold(ColdReason::NoCacheProvider)
        );
        // (c) upstream bust → cold even when the prefix would be warm.
        assert_eq!(
            cache_state(&ephemeral, Some(1), true),
            CacheState::Cold(ColdReason::UpstreamBust)
        );
        // (b) TTL elapsed → cold.
        assert_eq!(
            cache_state(&ephemeral, Some(301), false),
            CacheState::Cold(ColdReason::TtlElapsed)
        );
        // No send yet → cold (no warm prefix to lose).
        assert_eq!(
            cache_state(&ephemeral, None, false),
            CacheState::Cold(ColdReason::TtlElapsed)
        );
        // Warm: ephemeral, within TTL, no bust.
        assert_eq!(cache_state(&ephemeral, Some(10), false), CacheState::Hot);
        assert!(!cache_state(&ephemeral, Some(10), false).is_cold());
    }

    /// Helper: approximate the wire tokens of one message via the same
    /// tokenizer the projection uses, over its tool-result body (the only
    /// thing prune touches).
    fn wire_tokens(msg: &Message) -> usize {
        match msg {
            Message::User { content } => content
                .iter()
                .map(|c| match c {
                    UserContent::ToolResult(tr) => {
                        crate::tokens::count(&tool_result_body(&tr.content))
                    }
                    _ => 0,
                })
                .sum(),
            _ => 0,
        }
    }
}
