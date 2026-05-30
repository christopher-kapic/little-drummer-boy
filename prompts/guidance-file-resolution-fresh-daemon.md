# Fix: project guidance file fails to resolve until a daemon already exists

## Goal

The project guidance / instructions file (e.g. `CLAUDE.md`, `AGENTS.md`)
must resolve correctly in the fresh-chat context indicator in **every**
launch state — daemonless, the first launch that spawns a daemon, and
subsequent launches against an already-running daemon. Today it only
resolves when a daemon was already running at launch.

## Observed behavior (the bug)

On `cockpit` 0.1.8, in this project (which has `CLAUDE.md`, no
`AGENTS.md`, and a global `~/.config/cockpit/extended-config.json` whose
`agent_guidance_files` is `["CLAUDE.md", "AGENTS.md"]`):

- **Daemonless mode** (chose "Continue without daemon"): the indicator
  shows **no guidance file**.
- **First launch that spawns a daemon**: also shows **no guidance file**.
- **Exit and relaunch while that daemon is still running**: now it
  **resolves correctly**.

So the discriminator is *"was a daemon already running at launch?"* — not
daemon-vs-daemonless per se. The same persistent daemon serves both the
failing first launch and the succeeding relaunch.

## What's already known (do not re-derive — verify, then go past it)

The launch-time estimate is computed once and never refreshed:

- `src/tui/app/mod.rs:~936` (`App::run`) calls
  `agent_runner::fetch_guidance_estimate(&self.launch.cwd, …)` **once**,
  before the event loop — and in the TUI the daemon is spawned lazily
  (on first message via `ensure_agent_runner` →
  `probe_or_spawn`, `src/tui/agent_runner.rs:~100`), so at estimate time
  there is usually **no daemon yet**.
- `fetch_guidance_estimate` (`src/tui/agent_runner.rs:~263`) tries
  `daemon_guidance_estimate` first (only succeeds if a daemon is
  **already running**), otherwise falls back to `local_guidance_estimate`
  (`src/tui/agent_runner.rs:~316`).
- Both the daemon handler (`src/daemon/server.rs:~619`,
  `Request::GuidanceEstimate`) and the local fallback call the **same**
  `crate::engine::builtin::load_agent_guidance(cwd)`
  (`src/engine/builtin/mod.rs:~155`), which resolves config via
  `load_extended_config` → `discover_config_dirs`
  (`src/config/dirs.rs:38`) and walks `cwd`→git-root in
  `find_agent_guidance` (`src/engine/builtin/mod.rs:~177`).

Two facts that make this subtle and **must be explained by your root
cause**:

1. The shipped default `agent_guidance_files` is `["AGENTS.md"]` only
   (`src/config/extended.rs:~575`); this project resolves `CLAUDE.md`
   solely because the global config overrides that list.
2. Calling `local_guidance_estimate(cwd)` / `load_agent_guidance(cwd)`
   directly with `cwd = <this project root>` **does resolve `CLAUDE.md`
   correctly** in isolation. So the failure is **not** pure logic in the
   walk — it is environment- or sequencing-dependent at TUI launch. A
   fix that can't explain why the isolated call succeeds while the live
   launch fails is incomplete.

## Required investigation

Find the **true** root cause before changing anything. The two facts
above mean the likely culprits are one or more of:

- The estimate is computed **once at startup via the local fallback and
  never recomputed** after a daemon connects mid-session — so the first
  launch is permanently stuck on whatever the local fallback produced,
  while a relaunch (daemon already up) takes the daemon path. Confirm
  whether the indicator is refreshed on daemon connect.
- The local fallback at launch runs with a **different effective cwd,
  `HOME`/`XDG_CONFIG_HOME`, or `PATH`** than the isolated call —
  e.g. `discover_config_dirs` not finding `~/.config/cockpit`, so the
  list collapses to the `["AGENTS.md"]` default and this project (no
  `AGENTS.md`) resolves nothing. Note `find_agent_guidance` calls
  `crate::git::find_worktree_root` which shells out to `git`; check its
  behavior when `git` resolution differs between client and daemon
  processes.
- A race where `daemon_guidance_estimate` is attempted against a
  not-yet-ready daemon and silently falls back.

Instrument or trace as needed **during investigation only**; the
delivered change must contain no leftover debug logging.

## Required fix (root-cause + full)

1. The guidance file must resolve correctly in **all three** launch
   states: daemonless, first-launch-that-spawns-a-daemon, and
   already-running-daemon.
2. **Refresh the estimate when the daemon connects.** Once a daemon
   becomes available mid-session (lazy spawn or later attach), the
   indicator must update to the daemon's calibrated estimate rather than
   remaining stuck on the launch-time local fallback. The user should
   never have to exit and relaunch to get correct resolution.
3. Keep the local fallback correct on its own (daemonless sessions never
   get a daemon, so their estimate must still be right — raw cl100k
   token count is acceptable there; only detection/filename must match
   what the engine will actually inject).
4. The displayed estimate must stay consistent with what the engine
   actually injects into the agent's system prompt
   (`compose_system_prompt_with`, `src/engine/builtin/mod.rs:~95`) — do
   not let the indicator and the real injection diverge.

## Out of scope

Do **not** change the shipped default `agent_guidance_files` list
(the `["AGENTS.md"]` → add-`CLAUDE.md` question) — that is a separate
decision tracked elsewhere.

## Acceptance

- Launch with no daemon running, in a project whose only guidance file
  is `CLAUDE.md` (with the global config that lists it): the indicator
  shows the correct file and a sane token count — on the **first**
  launch, with no exit/relaunch needed.
- Launch that lazily spawns a daemon: after the daemon connects, the
  indicator reflects the daemon's calibrated estimate, still showing the
  correct file.
- Launch against an already-running daemon: unchanged (still correct).
- A regression test pins daemonless/local resolution and (where
  feasible) the refresh-on-daemon-connect behavior, so this can't
  silently regress.

## Constraints

- Implement without incurring tech debt: no shortcuts, no TODO-for-later,
  no half-finished paths. Leave no debug logging or instrumentation
  behind.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Respect the existing daemon/TUI split and token-economy rules
  (`CLAUDE.md`); the indicator refresh must not bust the prompt cache or
  add per-launch daemon round-trips beyond what's needed.
- Gates: `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check`.
