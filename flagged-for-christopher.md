# Flagged items — review needed

Items deferred from the multi-feature implementation pass (2026-05-26).

## @-file tagging gaps (GOALS §1e)

- **Gitignore enforcement on submit.** The popup filters gitignored files
  from suggestions, but `expand_tags()` will still inline a typed-but-
  untagged path that happens to be gitignored. The spec wants an explicit
  refusal naming the matched pattern. Currently it's silent omission from
  suggestions only — a user can still type `@.env` and get it inlined.
  Easy follow-up: run the same `ignore::WalkBuilder` lookup inside
  `expand_tags()` and fall back to a `[note: ... gitignored ...]` chip.

- **Symlink loop detection.** `WalkBuilder::follow_links(false)`; symlinks
  aren't chased. Spec calls for target resolution + loop detection. Out
  of scope for this slice.

- **Composer "attached: N files, ≈M tokens" chip.** Spec §1e references a
  queued-count indicator near the composer; not implemented.

- **Config knobs** — `composer.tagging.allow_gitignored_files`,
  `composer.tagging.list_hidden_in_directories`. Schema not added yet.

- **Paths with spaces.** Tag parsing stops on whitespace (the `@` syntax
  is whitespace-terminated per spec). A `@path with spaces/file.rs` tag
  isn't expressible. Probably needs a quoted form (`@"path with spaces"`).

## Redaction edge case (GOALS §7)

- **Substring matches against allowlisted origin vars.** If a non-
  allowlisted env var's value happens to be a substring of `$PATH` (or
  another allowlisted var's value), the matcher will still hit it.
  Allowlist controls the *origin* env var name, not arbitrary substring
  matches against the resulting search text. Documented in tests; the
  behavior is correct for the threat model (we want to redact the secret
  regardless of where it appears) but worth knowing.

## Markdown rendering

- Recommendation from the termimad research: pull in `pulldown-cmark`
  rather than `termimad` (no ratatui integration in termimad — would
  require writing a `FmtText → Span` converter). pulldown-cmark + a
  small Span emitter is ~120 LOC and ratatui-native.

## Settings layering

- `with_custom_tools` and `load_user_name` use "first matching layer
  wins" rather than the deep-merge mode the config-layering plan
  ([[config_layering]] memory) describes. Fine for v1; revisit when the
  full merge-mode catalog lands.

## Prompt-injection guard (GOALS §4i, §17)

The schema slot and the wire shape are in place; the substance is
deferred. Open questions before implementation:

- **Detector model + prompt shape.** The guard issues one
  inference call per user-input send (via `prompt_injection_guard.model`,
  falling back to `utility_model`). The classifier prompt and
  output format (binary `{is_injection: bool, reason: str}`?
  multi-label? severity-graded?) are TBD.
- **Action on detection.** v1's spec is "warn + require explicit
  confirmation to send." Should a stricter mode block outright?
  Should there be a "tool-result-only" mode that scans incoming
  tool outputs as well? (That's where injection actually
  originates in practice.) Decide before code.
- **Habituation risk.** If a user confirms-anyway five times in a
  row, the warning loses signal. Some cooldown / escalation
  design — silent flag, raised threshold on the same project,
  aggregate stats — is probably needed.
- **Scope creep.** v1 scans only user-authored input (composer
  prose + `@`-tagged inlined content). Tool results are out of
  scope until v2. That's a deliberate punt; the cost is a known
  gap that the docs should flag to users when the feature ships.

## Startup banner port (GOALS §1g)

Source data: `p51-6.sh` in the repo root (256-color palette + 12
× 36 character grid, rendered with half-block glyphs). Port the
rendering to Rust (target: `src/tui/banner/`). Open questions:

- **24-bit color variant.** The shell script uses ANSI 256-color
  codes; the plane's effective palette is 8 colors. Should we
  also ship a truecolor variant for terminals that advertise it?
  (Lean: no — 256 is sufficient and the palette is small.)
- **User-supplied art.** Should `tui.banner.style` accept a path
  to a user ANSI-art file (e.g. `~/.config/cockpit/banner.ans`)?
  Probably yes, but not v1 — get the default working first.
- **Banner suppression telemetry.** No: per `miscellaneous.md`
  §4, no telemetry in v1 and the §16 opt-in channel covers
  only tool-call performance. Banner suppression is local-only.

## Session auto-titling — empty-`utility_model` UX

`GOALS.md` §17d says: if `utility_model` is unset, sessions
display their 6-char ID as the label. Is that obvious enough to
the user, or do we need a one-time toast ("Set `utility_model` to
auto-name sessions") on the first session that would have been
titled? Decide when first-run UX is being designed.

## Diff rendering for `write` / `writeunlock`

`GOALS.md` §1h lands diff rendering for `edit` / `editunlock` only:
those tools carry `old_string` + `new_string` in their args, so the
TUI can assemble a `HistoryEntry::Diff` from the `ToolStart` payload
alone. `write` / `writeunlock` ship the full new content but no
pre-write copy of the file, so there's no `old` to diff against.

To extend the diff treatment to writes, the engine would need to:

- Read the pre-write content inside the tool's `call()` (it
  already does, to detect CRLF — see `src/tools/writeunlock.rs:65`),
  and surface it as part of `ToolOutput`. Today `ToolOutput.content`
  is a single string; a `pre_write_content: Option<String>` sibling
  field would let the TUI access it without changing the model-
  facing surface.
- Wire that through `TurnEvent::ToolEnd` (extra optional field) and
  `proto::Event::ToolEnd` (same).
- The TUI then caches the pre-content on `ToolStart` (path only,
  no content yet) and pairs it with the new content from
  `ToolEnd` to build the `Diff` row.

Until that lands, `write` / `writeunlock` keep the current "wrote
`<path>` (<bytes> bytes, LF/CRLF)" summary line.

## `/sessions` / `/resume` / `/fork` / `/session rename` TUI plumbing

The four slash commands are in the menu (`SLASH_COMMANDS` in
`src/tui/app.rs`) and the wire RPCs are live in the daemon
(`ListSessions` filter params, `ForkSession`, `RenameSession`,
`DeleteSession` in `src/daemon/proto.rs`). What's missing:

- **AgentRunner needs an RPC wrapper.** Currently `AgentRunner`
  exposes only `input_tx` (one channel for user messages).
  Adding fork/rename/list calls means either (a) per-RPC oneshot
  channels, or (b) exposing the underlying `DaemonClient` to the
  TUI so `app.rs` can issue typed requests directly. Option (b)
  is cleaner; do it once and the future commands (delete, attach
  to fork, etc.) all flow through.
- **Session-picker dialog.** GOALS §17f spec'd a real picker
  (arrow nav, right-arrow fork descent, Enter to resume, `f` to
  fork, `r` to rename). Should live in `src/tui/session_picker.rs`
  alongside the other dialogs (`model_picker.rs`,
  `daemon_prompt.rs`). The dialog reads from `ListSessions`
  responses and emits `Attach { session_id: Some(...) }` /
  `ForkSession` / `RenameSession` / `DeleteSession` requests.
- **Re-attach flow for `/fork`.** After `ForkSession` returns the
  new `session_id`, the TUI's `AgentRunner` needs to detach from
  the old session and re-attach to the new one. The driver still
  owns the underlying conversation transcript on the daemon side;
  the TUI's history mirror has to be rebuilt from the new
  session's `Attached { history }` snapshot.
- **`/resume` is just an alias** — when the picker lands,
  `/resume` should dispatch identically to `/sessions`.

Until all of the above is in, the slash commands print stub
explanations into history. They're discoverable in the menu,
which means users will find the wire RPCs in `cockpit debug` /
direct daemon use; the TUI affordance is the missing piece.
