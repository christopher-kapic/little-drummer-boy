# Graceful daemon shutdown: drain in-flight work, refuse new

## Goal

Give the daemon a **graceful, drain-then-die shutdown**: when it's asked
to stop, it immediately stops issuing new LLM provider requests and
refuses new user work, lets in-flight inference and tool calls finish,
and only then exits — with a bounded grace period after which it
force-exits.

## Background — current behavior

- The SIGINT/SIGTERM handler (`src/daemon/mod.rs:316-335`) flips a
  shutdown flag; the accept loop watches it and **closes immediately**
  (`src/daemon/mod.rs:141-166`).
- `registry.shutdown_all()` (`src/daemon/registry.rs:150-165`) sends
  `SessionWork::Shutdown` to each worker but does **not await** them
  ("we don't have a join handle for them here"); it relies on the caller
  giving "a moment." There is no in-flight tracking, no gate on new
  provider requests, and no deadline/force fallback.
- A session worker handling `SessionWork::Shutdown`
  (`src/daemon/session_worker.rs:524-546`) drops its driver input
  channel and awaits the current turn, but nothing coordinates this
  across workers or blocks new provider calls during the wait.
- `StopDaemon` (explicit) lands via the server and currently triggers
  the same abrupt path.

## Desired behavior

A single graceful-shutdown path, used by SIGINT, SIGTERM, explicit
`StopDaemon`, and the ephemeral last-client/owner-exit teardown:

1. **Gate new provider requests immediately.** Once shutdown begins, no
   new request may be dispatched to any LLM provider. Implement this as
   a real chokepoint at the inference-dispatch site (where requests
   actually go out to providers) — not an advisory flag each call site
   must remember to check. In-flight inference/tool calls already
   running continue to completion.
2. **Refuse new user work.** New user messages / new turns are rejected
   while draining (see Drain UX). Don't silently drop or queue them.
3. **Await in-flight work.** The teardown must actually wait for all
   in-flight inference + tool calls across all session workers to finish
   (give the registry real join handles / a completion signal — today
   `shutdown_all()` fires and forgets).
4. **Bounded grace, then force.** Wait up to a fixed grace period for
   in-flight work to drain, then force-exit even if work remains
   (abort outstanding inference/tool calls). Use **30s** for the grace,
   reusing/aligning with the existing `EPHEMERAL_IDLE_GRACE` constant
   convention in `src/daemon/mod.rs`; define it as a named constant.
5. **Cleanup on every path.** Socket + pid files are removed whether the
   daemon drained cleanly or hit the force deadline (today
   `src/daemon/mod.rs:352-368`).

### Drain UX

- When drain begins, surface a user-facing notice in the attached
  TUI(s) — e.g. "finishing in-flight work, shutting down…".
- While draining, **reject** new user input/messages with a short
  notice rather than dropping or queuing them.
- If the force deadline is hit with work still outstanding, note that
  the shutdown was forced (so a truncated turn isn't mistaken for a
  clean finish).

## Edge cases

- `StopDaemon` (or a second signal) arriving while already draining must
  not start a second drain, reset the deadline, or deadlock. A second
  SIGINT/SIGTERM during drain may **shorten** to an immediate force-exit
  — pick that behavior and document it.
- Last client of an ephemeral daemon disconnects mid-inference: drain
  the in-flight work (subject to the same grace/force bound), then reap
  — don't abandon a running tool call just because the UI detached.
  Reconcile with the 30s idle watchdog: the watchdog reaps an **idle**
  daemon; an in-flight daemon drains first.
- A provider request that hangs must not block shutdown past the grace
  deadline — the force path aborts it.
- Multiple session workers draining concurrently: the daemon exits only
  after all have finished (or the shared deadline fires), not after the
  first.

## Expected UX / acceptance

- With an inference/tool call in flight, SIGINT/SIGTERM/`StopDaemon`
  lets the call finish (within grace), then the daemon exits cleanly;
  the TUI shows the drain notice and refuses new input meanwhile.
- No new provider request is dispatched after drain begins — assert at
  the dispatch chokepoint.
- A hung in-flight call is force-killed at the 30s deadline and the
  daemon still exits with socket/pid files cleaned up.
- Idle daemon (no in-flight work) shuts down promptly, as today.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Respect the single-async-job / single-in-daemon-authority design rules
  (CLAUDE.md): the drain state, the new-request gate, and in-flight
  tracking live on the daemon's central authority, not scattered
  per-call.
- Cross-platform: Linux, macOS, Windows. Windows has no SIGTERM — use
  the platform's Ctrl-C / console-close equivalents, consistent with the
  existing signal handling.
- Add tests for: drain-awaits-in-flight, new-request-gate-after-drain,
  force-at-deadline, and idle-fast-path.

## Notes / decisions baked in (from the requester)

- Drain bound: **bounded grace (30s) then force** — NOT wait-forever.
- Drain UX: show a notice and **reject** new input while draining.
- This is the shutdown-mechanics half; which daemon a daemonless TUI
  binds to and reliably triggering its teardown is the separate
  `daemonless-tui-ephemeral-lifecycle.md` prompt. This drain path is the
  shared shutdown route both the persistent and ephemeral daemons use.
