# `Ctrl+C` interrupt — implementation plan

High-level design for making `Ctrl+C` interrupt an in-flight agent
turn cleanly, with a double-tap-within-500ms fallback that also
exits the TUI. The TUI keybind is trivial; the substantive work is
engine-side cancellation that doesn't currently exist.

This is a follow-up plan deliberately deferred during the T8 work
(see `plan.md`). The wire path (`Request::CancelTurn` →
`SessionWork::Cancel`) already exists end-to-end; the daemon
acknowledges the request but the engine ignores it.

## UX spec

| Agent state | First `Ctrl+C` | Second `Ctrl+C` (within 500ms) |
|-------------|----------------|--------------------------------|
| Idle (no `pending`)        | Exit TUI (current behavior). | — |
| In flight (`pending = Some`) | Interrupt the turn. | Interrupt + exit. |

"Interrupt" means:

1. **Stop the in-flight inference.** Cancel the LLM streaming
   request mid-flight. Whatever assistant text / reasoning has
   streamed so far is **dropped** from the conversation transcript
   so it doesn't pollute context on the next turn.
2. **Discard any in-flight tool call.** If a tool is mid-execution,
   abandon it. Do **not** return its (partial or eventual) result
   to the model — that would corrupt context with a tool-result
   message whose call was never resolved.
3. **Drain the queue.** If the user has messages waiting (GOALS §1c
   queue), the interrupt becomes a clean turn boundary: the next
   inference picks up the queued messages as the user's input.
4. **Empty queue → idle.** No queued messages means the interrupt
   returns the session to the "waiting for user input" state.

UX affordances during interrupt:

- A toast announces the interrupt (`"Interrupted."` or similar).
- The "Thinking…" / streaming indicator clears immediately.
- The dark-grey-while-pending input border flips back to white as
  soon as `pending` clears.

## Current state

- **TUI:** `Ctrl+C` and `Ctrl+D` quit unconditionally
  (`src/tui/app.rs::handle_key`). No double-tap detection, no
  pending-aware branching.
- **Wire path:** `Request::CancelTurn` exists in
  `src/daemon/proto.rs`; the daemon's request handler routes it to
  `SessionWork::Cancel` in `src/daemon/server.rs:268`.
- **Engine:** `SessionWork::Cancel` in
  `src/daemon/session_worker.rs:183` logs and no-ops, with a comment
  acknowledging the deferred work:

  > v1: log only. Cancellation propagation through
  > `Model::complete` lands in a follow-up — it needs a
  > CancellationToken plumbed into rig's streaming future.

So the binding is straightforward to add but does nothing useful
until the engine work lands. The previous turn deferred the entire
feature for that reason — landing the TUI half against the no-op
stub is worse UX than not having the binding at all.

## What needs to change

### 1. Engine: a real cancellation token

Introduce a `CancellationToken` (from `tokio-util::sync::
CancellationToken`) on the engine's `Driver`. Each new turn
creates a fresh child token from the session-level root.

- `Driver` holds: `Option<CancellationToken>` — `Some` during an
  active turn, `None` between turns.
- When `SessionWork::Cancel` arrives:
  - If a turn is active: call `token.cancel()` and drop the slot
    (so a second `Cancel` is a no-op).
  - If no turn is active: log + ignore.

The token threads down into:

a. **The LLM streaming future.** Wrap the `rig` provider's
   streaming call in `tokio::select!` with `token.cancelled()`.
   On cancellation: drop the stream future. Whatever bytes are
   in-flight at the transport layer get discarded.

   Open question: does dropping a `rig` stream cleanly close the
   HTTP connection underneath, or does it leak until the timeout?
   Worth verifying with `kctx ask rig "stream cancellation"`.

b. **In-flight tool calls.** When a tool is executing (bash
   subprocess, file IO, MCP roundtrip), the tool wrapper takes
   the same token and selects against cancellation. Bash is the
   tricky one — see Risks below.

c. **The Driver's outer turn loop.** Between LLM round-trips
   (e.g., after a tool result is appended, before the next
   `Model::complete`), check `token.is_cancelled()` and bail.

### 2. Engine: transcript cleanup on cancel

The session-state mutator must be atomic with respect to
cancellation. Mid-turn, the partial state looks like:

- Assistant message accumulating bytes (text + reasoning).
- Possibly a partial tool call (the model emitted the call but
  the result isn't back yet).
- Optionally one or more completed tool calls earlier in the same
  turn.

On cancellation, the "in-flight" components must **not** persist
to the conversation history:

- **Partial assistant message** → discarded. If we kept it, the
  next turn's prompt would carry a half-finished assistant
  utterance, and the provider would likely complete it in
  unpredictable ways.
- **Unresolved tool call** → also discarded. A tool-use entry
  without a matching tool-result is a hard protocol error for
  most providers (they refuse the next call). The transcript
  must look like the tool call never happened.
- **Already-completed tool calls earlier in the same turn** →
  ambiguous. Either:
  - **Drop them too** — treat the entire turn as if it never
    started. Simpler. Risk: the user may have already observed
    the side-effects (a file got edited, a command ran), and
    "the agent doesn't remember it" can confuse downstream
    reasoning.
  - **Keep them, pair each with a `cancelled-by-user`
    synthetic tool result** — preserves the work but pollutes
    context with a synthetic message. More faithful to what
    actually happened.

  Recommend the **drop everything** path for v1, with a clear
  comment that the alternative is on the table. Cheaper to
  reason about; lines up with "the turn was interrupted, please
  start over."

### 3. Engine: queue-aware re-dispatch

After the cancellation propagates and the transcript is rolled
back:

- If the session's input queue (`SessionWork::UserMessage`
  pending in the channel) is non-empty: immediately dispatch
  the next turn carrying the queued message(s).
  - In code: after the cancel completes, `pop` queued messages
    from the channel and start a fresh turn with them folded as
    the user's input.
- If the queue is empty: the session is now idle. The TUI's
  `pending` slot clears via the natural event flow (no more
  `AssistantText` events arrive, the next event is whatever the
  user types or `SessionEnded`).

### 4. Engine: event emission

The daemon should emit a new event so the TUI knows the
interrupt landed:

```rust
proto::Event::TurnCancelled {
    session_id: Uuid,
    /// `true` if a queued message will start a fresh turn next;
    /// `false` if the session is now idle.
    requeued: bool,
}
```

The TUI translates this into `TurnEvent::TurnCancelled` and
shows a toast.

### 5. TUI: keybind + double-tap

Smallest surface. In `src/tui/app.rs::handle_key`:

```rust
// At top of handle_key, before any other handlers:
let is_ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
    && !key.modifiers.contains(KeyModifiers::SHIFT)
    && matches!(key.code, KeyCode::Char('c'));

if is_ctrl_c {
    let now = Instant::now();
    let is_double_tap = self
        .last_ctrl_c_at
        .is_some_and(|t| now.duration_since(t) < CTRL_C_DOUBLE_TAP);
    self.last_ctrl_c_at = Some(now);

    if self.pending.is_some() {
        self.send_cancel_turn();
        if is_double_tap {
            return true; // exit
        }
        self.show_toast("Interrupting…", ToastKind::Info);
        return false;
    }
    return true; // idle: exit (current behavior)
}
```

New `last_ctrl_c_at: Option<Instant>` on `App`. New constant
`CTRL_C_DOUBLE_TAP: Duration = Duration::from_millis(500)`.

`send_cancel_turn` is a thin wrapper that calls
`agent_runner.input_tx`'s sibling channel — see #6.

The existing `Ctrl+D` quit stays unconditional. Ctrl+D doesn't
participate in the double-tap logic.

### 6. TUI ↔ daemon: cancel channel

Today `AgentRunner` has `input_tx: mpsc::Sender<String>` for
user messages only. Add a parallel `cancel_tx: mpsc::Sender<()>`
that the spawned task drains and forwards as
`Request::CancelTurn`. Kept separate from `input_tx` so a queued
user message doesn't sit behind the cancel signal — cancels
should jump ahead.

Alternative: make `input_tx` carry an enum `{ Send(String),
Cancel }`. Slightly cleaner but introduces a new type at the
boundary. Either is fine; per-channel is the smaller diff.

## Risk and edge cases

- **Cancelling mid-bash-subprocess.** Bash is the spicy tool —
  if the user's command is running, what kills it? Options:
  - Send SIGTERM to the child process group on cancel, wait 100ms,
    SIGKILL if still alive. Clean but adds OS-level work.
  - Just stop awaiting the child's output; let the process keep
    running detached. Simpler but leaks long-running commands.

  Recommend the SIGTERM/SIGKILL path; matches what shells do for
  their own `Ctrl+C`.

- **Race: cancel arrives between turns.** The user submits a
  message, hits `Ctrl+C` before the daemon picks it up. The
  cancel should be a no-op (no active turn to cancel) AND the
  pending user message in the queue should be dropped — otherwise
  the next turn starts with content the user no longer wants.

  This requires the cancel handler to also drain the queue when
  it lands during the idle window. Worth a test.

- **Race: cancel during the cancel.** Second `Ctrl+C` while the
  first cancel is still propagating. The Driver should be
  idempotent here — cancelling an already-cancelled token is a
  no-op in `tokio-util`.

- **Subagent calls.** If the active agent is a subagent
  (`Build` spawned `coder`), should `Ctrl+C` cancel
  just the child, or the whole tree? Recommend **whole tree** —
  the parent is blocked waiting on the child's report, so
  cancelling just the child would leave the parent stuck without
  a meaningful signal. Token tree: parent's token is the child's
  token's source, so `parent.cancel()` cancels everything.

- **DB consistency.** A cancelled turn should be marked in the
  `tool_calls` / `inference_calls` tables as such, so `cockpit
  stats` and session resumption see the truth. Add a `cancelled`
  column or a sentinel state.

- **Cache invalidation.** The provider's prompt cache may be in
  an interesting state after a partial response. If we drop the
  partial response from the transcript, the next prompt has the
  same prefix it would have had before the turn started — so the
  cache should still hit. Worth verifying.

- **OAuth token refresh mid-stream.** If a provider refreshes its
  auth token during the cancelled stream, the refresh shouldn't
  be lost. Auth state lives outside the per-turn token, so this
  should be safe by construction.

## Scope estimate

- **Engine cancellation token** (Driver + tool wrappers + rig
  integration): meaningful work, touches several files. Estimate
  ~300–600 lines.
- **Transcript rollback on cancel**: ~100–200 lines, mostly
  state-machine pruning in the Driver.
- **Bash subprocess SIGTERM/SIGKILL**: ~50 lines in
  `src/tools/bash.rs`.
- **Queue re-dispatch + new event type**: ~100 lines split between
  `session_worker.rs` and `proto.rs`.
- **TUI keybind + cancel channel**: ~50 lines.
- **Tests**: a real cancellation test needs a fake provider that
  streams and respects cancellation. ~150 lines of test scaffold.

Total: realistically a focused multi-day project. Best landed as
its own milestone, not bundled with unrelated TUI polish.

## Open questions to resolve before coding

1. **Drop-everything vs. keep-completed-tools.** Recommended
   drop-everything for v1 — confirm with user before implementing.
2. **Bash kill protocol.** SIGTERM-then-SIGKILL recommended;
   confirm timeout (100ms? 500ms?).
3. **`Ctrl+C` in interactive bash subprocess.** If the user is in
   an `exec_approval` dialog (per
   `TUI-design-philosophy.md` §6), does `Ctrl+C` cancel the
   approval or the agent turn? Suggest: cancel the dialog first
   (closer to user intent), agent turn only if no dialog is open.
4. **What about long-running streaming tool output?** If a tool
   streams output for a long time (e.g., a tail command), the
   tool output stream needs to honor the cancellation token too
   — not just the LLM stream.

## Out of scope

- **Resuming a cancelled turn.** "Continue where I cut off" is
  a v2 feature. v1: cancellation is final.
- **Per-tool cancel granularity.** The user can't say "cancel just
  this bash but keep the inference." It's all-or-nothing.
- **Cancellation from the daemon side without TUI involvement.**
  E.g., a timer that auto-cancels stalled turns. Future work.
