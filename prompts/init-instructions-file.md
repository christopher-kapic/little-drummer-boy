# Add `/init` — agentically generate the project instructions file

## Goal

Implement `/init [path]`: run an agent that explores the project and
writes (or updates) the project's instructions/guidance file.

## Current behavior

- `cockpit init` is a **stub** that bails: "cockpit init is not
  implemented yet (planned: run an agent to write AGENTS.md; do NOT
  write extended-config.json here)" (`src/commands/init.rs`).
- The guidance-file concept is live: `agent_guidance_files` config list
  (default `["AGENTS.md", "CLAUDE.md"]`), resolved by
  `find_agent_guidance` / `load_agent_guidance`
  (`src/engine/builtin/mod.rs:155-200`) walking cwd → git root and
  taking the first basename match; the body is injected into the system
  prompt.
- There is no TUI `/init` slash command.

## Desired behavior

- Provide `/init [path]` in the TUI (and make the `cockpit init` CLI
  command do the same underlying work — drop the stub bail).
- **Target file resolution:**
  - With an explicit arg — `/init MADE_UP_INSTRUCTIONS_FILE.md` or any
    arbitrary relative path — that path is the target.
  - With no arg — target the **first configured instructions file**,
    i.e. `agent_guidance_files[0]` (default `AGENTS.md`). (Use the first
    *configured* name, not "first that happens to exist".)
- **The work:** spawn an agent that explores the project (structure,
  build/test commands, conventions) and writes a concise, useful
  instructions file at the target path — the same intent as opencode's
  `/init`. Token economy applies to the generated file: it should be
  genuinely useful guidance, not padded.
- **Must NOT** write or modify `extended-config.json` (per the existing
  stub's note — config files are created lazily elsewhere).

## Edge cases & UX decisions

- **Target file already exists:** ask at runtime. Present a prompt with
  three choices — *update in place* (agent revises/extends the existing
  content, preserving what's there), *overwrite from scratch*, or
  *cancel*. Honor the choice. (For the headless `cockpit init` CLI path
  where there's no interactive prompt, default to refusing with a clear
  message that the file exists and how to override — do not silently
  overwrite.)
- Show progress while the agent runs and report the final path written.
- If the agent fails, report it and leave any existing file untouched.

## Which agent

Use the existing built-in agent infrastructure to do the exploration +
write (the project's normal coder/Build delegation path) rather than a
new bespoke agent. Picking the concrete agent and the exploration
strategy is left to the implementing agent — but it must produce a real
file write through the normal tool path, not a canned template.

## Acceptance

- `/init` with no arg writes `AGENTS.md` (or the first configured
  guidance filename) from an agent's exploration of the project; `/init
  some/other.md` targets that path; an existing target triggers the
  update/overwrite/cancel prompt; `extended-config.json` is never
  touched.

## Constraints

Implement without incurring tech debt — no shortcuts, no TODO-for-later,
no half-finished paths. For any new package use the latest stable
release unless this prompt says otherwise, and verify correct
API/dependency usage with `kcl ask <package> "<question>"` before wiring
it in. Slash-command descriptions are one sentence (token economy,
CLAUDE.md).
