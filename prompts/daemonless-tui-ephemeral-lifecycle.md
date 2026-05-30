# Daemonless TUI = own ephemeral daemon per instance; fix lingering daemon

## Goal

Make the TUI's "daemonless" mode actually run against a **per-instance
ephemeral daemon** that is isolated from other TUIs and torn down when
that TUI exits, and fix a bug where the daemon lingers after the TUI
exits if a message was sent.

## Background — current behavior

- The TUI always connects via `LifecycleMode::AttachOrAutoPromote`
  (`src/tui/agent_runner.rs:100`): attach to the canonical daemon if one
  is running, else **auto-promote a persistent daemon** at the shared
  canonical socket with `owns_daemon = false`. The TUI never sends
  `StopDaemon` on exit.
- The "Continue without daemon" choice in `src/tui/daemon_prompt.rs`
  (`DaemonChoice::ContinueWithout`, handled in
  `src/tui/app/input.rs:195`) is misleading: it still reaches
  `probe_or_spawn(AttachOrAutoPromote)` and ends up bound to a
  **persistent** daemon. There is no real daemonless path today.
- True ephemeral daemons already exist for `cockpit run`
  (`LifecycleMode::AttachOrEphemeral` / `--ephemeral` =
  `AlwaysEphemeral`, see `src/daemon/client.rs:219-307` and
  `src/commands/run.rs`). They are **per-pid**
  (`cockpit-eph-<pid>.sock`, `DaemonPaths::resolve_ephemeral(pid)`),
  `owns_daemon = true` (the spawner sends `StopDaemon` on exit via the
  RAII `EphemeralDaemonGuard`, which also covers panic/unwind and
  SIGINT/SIGTERM), and carry a self-reaping idle watchdog (30s grace
  after the last client disconnects, `EPHEMERAL_IDLE_GRACE` in
  `src/daemon/mod.rs`). Persistent daemons never arm that watchdog.

## Desired behavior

### 1. Daemonless mode = each TUI owns its own ephemeral daemon

- When a TUI runs in daemonless mode it spawns its **own per-pid
  ephemeral daemon** (the existing `AlwaysEphemeral` per-pid path), with
  `owns_daemon = true`. It does **not** attach to, or auto-promote, the
  canonical persistent daemon.
- A **second** daemonless TUI gets its **own** separate ephemeral
  daemon — instances are fully isolated. There is no sharing and no
  ownership transfer between TUIs. Two daemonless TUIs ⇒ two distinct
  ephemeral sockets/processes.
- Wire the existing "Continue without daemon" choice
  (`DaemonChoice::ContinueWithout`) to this path so it does what its
  label says, instead of silently auto-promoting a persistent daemon.
  The misleading wording/notice in `daemon_prompt.rs` /
  `src/tui/app/input.rs` should be corrected to reflect the real
  behavior (own ephemeral daemon, goes away on exit).
- On TUI exit the owned ephemeral daemon must be shut down on **every**
  exit path (clean quit, error, panic/unwind, SIGINT/SIGTERM) — reuse
  the same ownership contract `cockpit run` already has
  (`EphemeralDaemonGuard` + signal handler in `src/commands/run.rs`)
  rather than inventing a parallel mechanism. The self-reaping idle
  watchdog stays as the backstop for an uncatchable death.

### 2. Fix: daemon lingers after exit when a message was sent

- Repro: launch a daemonless TUI, send **no** message, exit → daemon
  tears down as expected. Launch, **send a message**, exit → the daemon
  stays alive indefinitely.
- The likely cause is that the first user message lazily persists the
  `sessions` row (`src/daemon/session_worker.rs:420-433`) and the
  teardown path then fails to actually stop the daemon once a persisted
  / in-flight session exists, whereas the no-message case has nothing
  holding it open. Root-cause it for real rather than papering over it.
- After the fix, a persisted session must **not** by itself keep an
  owned ephemeral daemon alive past its owner's exit: send-message-then-
  exit must reap the daemon just like the no-message case.

## Edge cases

- TUI killed with an uncatchable signal (SIGKILL): the per-pid
  self-reaping idle watchdog must still reap the orphan after the idle
  grace — preserve the existing
  `ephemeral_self_reaps_persistent_does_not` invariant in
  `src/daemon/mod.rs`.
- Daemonless TUI started while a persistent canonical daemon is also
  running: the two must coexist (ephemeral uses a per-pid socket that
  `daemon stop`/`status` never touch); the daemonless TUI must bind its
  own ephemeral daemon, not attach to the canonical one.
- Stale ephemeral socket/pid files from a prior crashed run must not
  block a new daemonless TUI from starting.
- Exit cleanup must remove the ephemeral socket + pid files (today done
  at `src/daemon/mod.rs:352-368`).

## Expected UX / acceptance

- Two daemonless TUIs ⇒ two distinct ephemeral daemon processes/sockets
  (verify both exist and are independent).
- Exiting a daemonless TUI reaps its ephemeral daemon (no orphan
  process; socket + pid files removed) — whether or not a message was
  sent.
- The "Continue without daemon" choice produces an owned ephemeral
  daemon (not a persistent one); its on-screen wording matches.
- Closing one daemonless TUI does not affect another daemonless TUI's
  daemon.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths. The lingering-daemon fix must
  be a real root-cause fix, not a workaround.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Reuse the existing ephemeral ownership machinery
  (`EphemeralDaemonGuard`, signal shutdown, per-pid `DaemonPaths`, the
  idle watchdog) — do not introduce a second, parallel lifecycle path.
- Cross-platform: Linux, macOS, Windows (the `cockpit run` ephemeral
  path already handles the non-unix signal case — stay consistent).
- Preserve existing daemon-lifecycle tests; add tests for daemonless =
  own-ephemeral, two-isolated-instances, and the lingering-daemon fix.

## Notes / decisions baked in (from the requester)

- Multi-TUI model: **own ephemeral daemon per instance**, fully
  isolated — NOT a shared daemon, so no ownership-transfer logic.
- The graceful drain-on-shutdown behavior is a **separate** prompt
  (`daemon-graceful-drain-shutdown.md`); this prompt is only about which
  daemon a daemonless TUI binds to and ensuring it is reliably torn
  down. The two are complementary: this one triggers shutdown correctly,
  the other governs how shutdown drains.
