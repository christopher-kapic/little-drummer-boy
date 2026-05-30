# Add `/ps` and `/stop` — current-session job list & stop

## Goal

Add two current-session-scoped conveniences over the existing job
infrastructure: `/ps` lists the running background/loop/timer jobs for
the current session, and `/stop` stops them.

## Current behavior

- `/jobs` is **fully implemented** (`src/tui/app/mod.rs`,
  `handle_jobs_command()` ~1554): bare `/jobs` lists active jobs across
  **all** sessions with elapsed time + per-session id; `/jobs cancel
  <job-id>` cancels one via `Request::CancelJob`.
- Job tracking: `app.active_jobs: HashMap<String, ActiveJob>` where
  `ActiveJob { label, kind ("loop"/"timer"/"background"), iteration,
  last_activity }` (`app/mod.rs:666`); daemon emits `JobStarted` /
  `JobProgress` / `JobNote` / `JobCompleted`.

## Desired behavior

`/ps` and `/stop` are **thin, current-session-scoped wrappers** over the
existing job infra. **`/jobs` stays exactly as-is** (the all-sessions
view); do not change or remove it.

- **`/ps`** — list only the jobs belonging to the **current** session
  (filter `active_jobs` to the current session id), showing the same
  per-job info `/jobs` shows (label, kind, elapsed/iteration). Empty
  state: "No background jobs in this session."
- **`/stop`** — stop current-session jobs:
  - `/stop <job-id>` stops that one job immediately (reuse the
    `CancelJob` path `/jobs cancel` uses).
  - **Bare `/stop`** stops **all** current-session jobs, but **only
    after a confirmation** — prompt "Stop N job(s) in this session?
    [y/N]" and proceed only on yes. N is the count of current-session
    jobs; if zero, say so and don't prompt.

## Edge cases & UX decisions

- All filtering is by current session id — `/ps` and `/stop` never touch
  other sessions' jobs (that's what `/jobs` is for).
- `/stop <job-id>` for an id that isn't in the current session: refuse
  with a message (don't reach across sessions).

## Acceptance

- `/ps` lists only this session's jobs; `/stop <id>` cancels one;
  bare `/stop` cancels all this-session jobs after a `[y/N]`
  confirmation; `/jobs` behaves unchanged.

## Constraints

Implement without incurring tech debt — no shortcuts, no TODO-for-later,
no half-finished paths. For any new package use the latest stable
release unless this prompt says otherwise, and verify correct
API/dependency usage with `kcl ask <package> "<question>"` before wiring
it in. Slash-command descriptions are one sentence (token economy,
CLAUDE.md).
