# Show the session short id at startup, before the first message

## Goal

When the TUI opens with a daemon reachable, the session short id must
appear in the welcome box immediately — before the user sends any message.
Today it only appears after the first message.

## Current behavior

The lazy-persist machinery already exists and is intended to show the id
at startup: `ensure_session_for_display()` runs each event-loop tick and
attaches a **deferred** (un-persisted) session so the welcome box can show
its short id; the DB row is only written on the first user message. See
`src/tui/app/mod.rs` (`ensure_session_for_display`, `ensure_agent_runner`)
and `src/tui/banner_box.rs` (renders `LaunchInfo::session_short_id`).

But in practice the short id appears only **after** the first message.

## Root-cause leads (verify by reproducing — do not assume)

There is an asymmetry between the two attach paths:

- **First-message path** (`src/tui/app/input.rs` ~line 1202) calls
  `ensure_agent_runner()` **unconditionally**.
- **Display path** (`ensure_session_for_display`, `src/tui/app/mod.rs`)
  refuses to attach unless `!self.daemon_connected` is false — i.e. it
  requires `daemon_connected == true`.

`daemon_connected` is set true in only two narrow spots: the constructor
when the startup probe returns `Running` (`src/tui/app/mod.rs` ~763), and
the "Start and connect" daemon-prompt choice (`src/tui/app/input.rs`
~184). It is not set after "continue without daemon", and may not reflect
a daemon that becomes reachable slightly later (e.g. just-spawned socket
not yet bound). So whenever `daemon_connected` is false at display time,
the eager attach is suppressed while the first message attaches anyway.

Also check the latch hazard: `ensure_agent_runner` stores the result as
`agent_runner = Some(runner)` even on `Err`, and both entry points
early-return on `self.agent_runner.is_some()`. A single transient
`try_spawn` failure (daemon socket not ready right after spawn) could
therefore poison the runner to `Some(Err(..))` and permanently disable the
eager display attach until something replaces it. Confirm whether this is
part of the observed behavior.

**Reproduce the bug first** (daemon already running; and the
start-and-connect flow) and confirm the actual root cause before fixing.
Don't ship a fix against a guessed cause.

## Desired behavior

- Opening the TUI with a daemon reachable — whether it was already running
  or was just started via "Start and connect" — shows the session short id
  in the welcome box immediately, before any message is sent.
- The eager display attach must be reliable: a single transient attach
  failure must not permanently disable it (retry on a later tick rather
  than latching forever on a poisoned `Some(Err)` runner), OR match
  whatever retry behavior the first-message path already relies on so the
  two paths behave consistently.

## Deliberate non-goals / keep these intact

- Do **not** attach or spawn a daemon while the "daemon not running"
  prompt is still open — that gate (`daemon_prompt.is_some()`) is
  intentional (the doc comment: "don't spawn a daemon out from under the
  user's choice").
- Do **not** auto-spawn a daemon purely to show the id when the user chose
  "continue without daemon". In that mode there is no daemon, so there is
  no deferred session and no short id until a daemon comes up (e.g. on the
  first message). That is acceptable and expected — just don't regress it.

## Acceptance

- Daemon running → open TUI → short id visible in the welcome box with no
  message sent.
- "Start and connect" → after the daemon starts, the short id appears
  without needing a first message.
- "Continue without daemon" → no short id until a daemon starts; no crash.
- The short id shown before the first message is the **same session** that
  gets persisted on first message — the id does not change between the
  welcome box and the post-first-message state.

## Constraints

- Implement without incurring tech debt: no shortcuts, no TODO-for-later,
  no half-finished paths.
- No new dependency is expected. If you do add one, use its latest stable
  release and verify API usage with `kcl ask <package> "<question>"`.
- Gates must pass: `cargo build`, `cargo test`, `cargo clippy -- -D
  warnings`, `cargo fmt --check`.
