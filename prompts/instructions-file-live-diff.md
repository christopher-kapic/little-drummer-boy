# Live instructions-file diff injection

## Goal

Keep the model's view of the project instructions file current within a
long-running session. Snapshot the resolved instructions file at session
start; on every subsequent outbound LLM request, if the file's content
hash changed, append a system message carrying the diff so the model sees
the up-to-date instructions without busting the cached system prefix.

## Current behavior

- The instructions file is the **single resolved agent-guidance file**:
  `find_agent_guidance(cwd, names)` in `src/engine/builtin/mod.rs` walks up
  from cwd to the git worktree root and returns the first match of the
  configured `agent_guidance_files` list (default `["AGENTS.md"]`, common
  fallback `CLAUDE.md`). It returns `Option<(PathBuf, String)>`.
- That body is baked into the **cached system block** by
  `compose_system_prompt_with(...)` (`src/engine/builtin/mod.rs`, ~lines
  85–137). The system block is held byte-stable for the session so the
  client-side prompt cache hits (GOALS §17g). The instructions content is
  therefore frozen at the value it had when the system prompt was first
  composed — edits to the file mid-session are invisible to the model.
- Outbound requests are assembled in
  `src/engine/model.rs` (`assembled_request(...)`, ~lines 235–243) and sent
  via `build_agent(...).completion(prompt, history).stream()` (~lines
  282–298). Each request body is captured and persisted to the
  `inference_requests` table by `call_id`
  (`session.record_inference_request(...)`, `src/engine/agent.rs:336`).
- Sessions live in the `sessions` table (latest migration
  `0015_rename_build_agent.sql`). There is **no** content-addressed
  hash→contents table today; `inference_requests` stores full request
  bodies keyed by `call_id`, which is unrelated.
- `similar` (`TextDiff::from_lines`) already powers diff rendering in
  `src/tui/diff.rs` (inline unified mode, `-`/`+`/context, 3 context lines).

## Desired behavior

1. **Snapshot at session start.** When a session begins and the system
   prompt is composed, compute a content hash of the resolved guidance file
   body (the exact string that went into the system block). Store:
   - the hash on the session row (new column on `sessions`), and
   - the `hash → contents` mapping in a new content-addressed table.
   If no guidance file resolves at session start, store no baseline hash
   (NULL) and do nothing further for this feature in that session — see
   "Edge cases".

2. **Check on every outbound request.** In the outbound request path
   (`model.rs` assembly, before send), re-resolve the *same* guidance file,
   recompute its content hash, and compare to the session's stored baseline
   hash. The re-resolution must target the same file the baseline came from
   (an in-place edit to that file); switching files is out of scope (see
   "Edge cases").

3. **Inject a diff when the hash changed.** If the hash differs:
   - Persist the new contents into the content-addressed table (keyed by
     its hash; dedup on collision — content-addressed storage is naturally
     idempotent).
   - Build the message body:
     - Default: a **unified diff** (old contents → new contents) via
       `similar`, matching the inline-diff style already in `tui/diff.rs`.
     - **Full-contents fallback** when a diff would be useless: no usable
       baseline contents available, or the change is near-total (≳50% of
       lines changed). In the fallback, inject the full new contents
       instead of a diff.
   - Append it to the conversation as a **synthetic system-role message**
     at the end of history, framed as authoritative — e.g. a short header
     like "Your instructions file (`<path>`) changed since this
     conversation began. Apply the updated version:" followed by the
     diff/contents. Appending (never rewriting the cached prefix) keeps the
     prompt cache intact.
   - **Advance the baseline:** update the session's stored hash to the new
     hash so each distinct change is injected exactly once. The next
     request diffs from the just-injected version. (If the file later
     reverts to an earlier content, that is simply another change and gets
     its own diff.)

4. **Idempotent across turns.** Once injected and the baseline advanced, the
   same change must not re-inject on subsequent requests. Only a further
   content change triggers another injection.

## Edge cases & decisions (settled — do not re-litigate)

- **Track scope:** only the single resolved guidance file body that went
  into the system prompt. One baseline hash per session.
- **Inject format:** unified diff by default; full-contents fallback only
  when there is no usable baseline or the change is near-total (≳50% lines
  changed).
- **Inject form:** appended system-role message, authoritative framing,
  end of history. Must be cache-safe (append only; never mutate the cached
  system prefix).
- **In-place edits only.** This feature handles edits to the *same* resolved
  file. The following are explicitly **out of scope** — take no action
  (no injection) in these cases:
  - the resolved file is deleted mid-session,
  - the resolved file switches (e.g. `AGENTS.md` deleted so `CLAUDE.md` now
    wins),
  - a guidance file appears mid-session where none existed at start.
  Re-resolution that no longer finds the original baseline file (or finds a
  different one) is treated as "no in-place change to track" — leave the
  baseline as-is and do not inject. Do not error.
- **Subagents:** subagents recompose a fresh system prompt on spawn and so
  already pick up the latest file — no injection needed for them. This
  feature targets the long-lived conversation that reuses a frozen system
  block across multiple requests.
- **Single-shot / non-interactive runs:** harmless. A run that makes one
  outbound request never observes a change; long-running loops/jobs benefit
  the same as interactive sessions.

## Storage

- **New `sessions` column** for the baseline instructions hash (nullable;
  NULL when no guidance file resolved at start).
- **New content-addressed table** mapping `hash → contents` (plus whatever
  minimal metadata is genuinely needed). Keyed by the content hash; inserts
  are idempotent (ignore on existing hash). This stores both the start-of-
  session baseline contents and each subsequent version, so diffs can be
  computed from the prior stored contents.
- Add a new numbered migration (next after `0015`). Follow the existing
  migration conventions in `src/db/migrations/`.

## Acceptance

- Start a session in a repo with an `AGENTS.md` (or `CLAUDE.md`); confirm
  the `sessions` row has the baseline hash and the contents table has the
  body.
- Edit the file mid-session; on the next outbound request, the captured
  request body (`inference_requests`) contains a trailing system message
  with the unified diff of the change.
- The diff does **not** appear again on the following request (baseline
  advanced); a further edit produces a new diff.
- A near-total rewrite injects full contents, not a noisy diff.
- Deleting the file mid-session injects nothing and does not error.
- Prompt-cache behavior is unaffected: the cached system prefix is
  byte-identical to before; the new content is appended only.

## Suggested packages

None new. Reuse `similar` (already a dependency) for the unified diff, and
the existing hashing approach used by the codebase-intelligence index
(mtime+size with hash tiebreaker) as a model for content hashing — pick the
hash already used there rather than introducing a new hash crate.

## Constraints (always)

- Implement without incurring tech debt: no shortcuts, no TODO-for-later,
  no half-finished paths.
- For any new package, use the latest stable release unless this prompt says
  otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in. (No new package is
  expected here.)
- Honor token economy (GOALS §10): the injected diff is the only added
  context — no preamble bloat; keep the framing header to one line.
- Redaction is non-bypassable: the injected message goes through the same
  outbound path, so it must pass through `redact::scrub()` like any other
  content — do not route around it.
