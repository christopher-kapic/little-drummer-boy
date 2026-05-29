# Miscellaneous design notes

Considerations that don't have a doc of their own. Each item is small enough
to live as a short section here; if any one of them grows, split it out
into a dedicated doc and link from `GOALS.md`.

---

## 1. Windows compatibility

`cockpit` must work on Windows. The hard parts:

### 1a. Shells

opencode's `bash` tool, shell-substitution slash commands, and many
formatters (`prettier`, `ruff`, `gofmt`) assume a POSIX-ish shell. Native
Windows `cmd.exe` and `powershell.exe` don't satisfy that.

How other CLIs handle this:

- **git for Windows** ships its own MSYS2 environment with `bash.exe`
  installed at `C:\Program Files\Git\bin\bash.exe`. ~80% of Windows
  developers who do anything terminal-related already have it.
- **scoop / chocolatey / winget** can install gitbash declaratively.
- **WSL** is an alternative but is heavyweight and not always available
  (corporate machines, ARM64 edge cases).

**Recommended approach:**

1. **Detect** an existing gitbash install on Windows in this order:
   `where.exe bash.exe` → `C:\Program Files\Git\bin\bash.exe` →
   `C:\Program Files (x86)\Git\bin\bash.exe` → `%LOCALAPPDATA%\Programs\Git\bin\bash.exe`.
2. If found, set the default `shell` to the discovered `bash.exe`.
3. If not found, on first launch print:
   > Windows requires a POSIX shell for the `bash` tool and most formatters.
   > Install Git for Windows (`winget install Git.Git`) or set
   > `shell` in your `config.json` to a shell of your choice. Continuing
   > without a POSIX shell will disable the `bash` tool.
4. **Do not bundle gitbash inside `cockpit`'s installer.** Bundling adds
   ~250 MB and licensing complexity (gitbash is a curated MSYS2
   distribution; redistribution is allowed but sets the expectation that
   we own its security updates). Better: detect, and direct users to
   `winget`/`scoop` for the install.

### 1b. Paths

- Always use `std::path::PathBuf` and `Path::join`, never string
  concatenation with `/`.
- Honor `XDG_CONFIG_HOME` etc. on every platform (kctx and ralph already
  do this — match them). On Windows the defaults map to:
  `%APPDATA%\cockpit` for config and `%LOCALAPPDATA%\cockpit`
  for data. We deliberately do **not** use `%PROGRAMDATA%`
  (system-wide).
- When invoking subprocess harnesses, pass paths exactly as the user
  wrote them (no canonicalization) — Windows users sometimes mix
  forward and backward slashes intentionally.

### 1c. Process groups & signals

`ralph-rs` uses `process_group(0)` + SIGKILL-to-group on Unix to clean
up child processes when a harness invocation is cancelled. On Windows:

- Use `CREATE_NEW_PROCESS_GROUP` when spawning.
- Cancel via `GenerateConsoleCtrlEvent` on the group, then fall back to
  `TerminateProcess` after a grace period.
- Existing crate options: `tokio` exposes the right knobs through
  `std::os::windows::process::CommandExt`. We can copy ralph's `signal`
  module structure and add a `#[cfg(windows)]` branch.

### 1d. Terminal & TUI

- `crossterm` (which both ratatui and codex use) abstracts most Windows
  terminal differences. The supported terminals are Windows Terminal
  (recommended), ConEmu, and the modern conhost. Older `cmd.exe` is
  *not* supported.
- ANSI escape support: enabled via `ENABLE_VIRTUAL_TERMINAL_PROCESSING`
  in `crossterm::terminal::enable_raw_mode()`.
- Bracketed paste: works on Windows Terminal; falls back to crossterm's
  paste-burst detection on legacy conhost.
- Mouse support: works on Windows Terminal.

### 1e. Default keymap

Codex notes:
> Windows terminals have modified defaults for undo (`ctrl+z` added) and
> suspend (forced to `none`) due to POSIX compatibility limitations.

Apply the same overrides. `Ctrl+Z` in a Unix shell SIGTSTPs; on Windows
it inserts a literal substitute character. We rebind it to undo.

### 1f. Filesystem case-sensitivity

NTFS is case-insensitive by default; macOS HFS+/APFS can be either. The
agent search path (`~/.config/cockpit/agents/CLAUDE.md` vs `claude.md`)
must not assume case sensitivity. When listing agents, lowercase the
filename for the agent-name lookup table; when reading the file, use the
on-disk casing.

### 1g. Line endings

JSON config files round-trip CRLF → LF on Windows write-back, which
is ugly. Use `serde_json` with the platform's native line ending only
when writing a file we created; preserve original line endings when we
edit in-place. Cheap heuristic: count `\r\n` vs `\n` in the first 1 KB
of the file we're about to overwrite.

---

## 1.5. Shell-tool availability detection

cockpit's `bash` tool surface depends on what the user has
installed locally — `rg`, `fd`, GNU `sed`. The rule across all
of them is the **same**: **detect availability, advertise it in
the `bash` tool description, do not silently alias** *unless* the
substitution is genuinely drop-in compatible.

### 1.5a. `rg` and `fd` — advertise, don't alias

`rg` ≠ `grep` and `fd` ≠ `find`. Their flag semantics differ in
ways that cause **silent wrong behavior** rather than loud
failures:

- `rg -r` means "replace," not "recursive" (`rg` is recursive
  by default).
- `find . -name '*.rs' -exec ...` has no clean fd equivalent;
  the model would write `find` syntax and get fd's positional-
  regex interpretation.
- `grep -P` (Perl regex) vs rg's regex flavor diverges on
  lookbehinds and a few other constructs.

Silent aliasing would corrupt the agent's mental model of which
tool it's using. Instead, on startup cockpit probes for `rg` and
`fd` (`which rg`, `which fd` or `which fdfind` on Debian) and
includes a one-line note in the `bash` tool description:

> `rg` (ripgrep) is available — preferred over `grep -r` for
> recursive search. `fd` is available — preferred over `find`
> for filename discovery. Use their native flag semantics, not
> `grep`/`find` flags.

When `rg`/`fd` are absent, the note is omitted (no false
suggestion that they're there). The structured `grep` and `glob`
tools (per `GOALS.md` §10's v1 tool surface) are always
ripgrep-backed internally regardless — this advertisement
exists only for the case where the model invokes a search via
the `bash` tool.

### 1.5b. `sed` → `gsed` on macOS — alias, because it's drop-in

macOS ships **BSD sed**, which differs from GNU sed in several
load-bearing ways (`-i` semantics, `-r`/`-E` for extended
regex, in-place backup-suffix handling). The friction is
constant — agents trained on Linux examples write `sed -i 's/…/…/' file`
and get a `command i expects \ followed by text` error.

GNU sed *is* available on macOS via Homebrew as `gsed`. It is a
drop-in replacement for GNU sed (because it *is* GNU sed). So
on macOS, when `gsed` is detected, cockpit transparently aliases
`sed` → `gsed` for the `bash` tool's execution environment
(e.g., by prepending a shim directory to `PATH` in the
spawned-subprocess environment, or via the equivalent shell-
function injection). The model writes `sed` and gets GNU sed
under the hood.

Regardless of whether the alias is active, the `bash` tool
description on macOS includes a single sentence telling the
model what `sed` actually is in its execution environment:

- If `gsed` was detected (alias active):
  > `sed` is GNU sed (`gsed`); standard GNU flags work.
- If only BSD sed is available:
  > `sed` is BSD sed (macOS native). `-i` requires an explicit
  > suffix (`-i ''` for no backup); `-E` for extended regex;
  > no `-r`.

On Linux/Windows, no `sed` note is added — GNU sed is the
default and the model can assume it.

### 1.5c. Detection runs once, at daemon startup

These probes run when the daemon starts and the results are
cached in `~/.local/state/cockpit/tool-env.json`. The `bash`
tool description is assembled from the cache at daemon-process
startup. Users who install `rg` later can re-probe with
`cockpit daemon restart` (cheap) or `cockpit tools rescan`
(no restart). The token cost of the advertised notes is
counted against the §10 budget like any other tool description.

---

## 2. CI matrix

Even before v1, the GitHub Actions matrix should be:

- `ubuntu-latest` × stable Rust
- `macos-latest` × stable Rust
- `windows-latest` × stable Rust + gitbash (pre-installed on the runner)
- `ubuntu-latest` × MSRV (set to current stable - 2 minor versions)

Compare ralph-rs's `.github/workflows/` for the existing pattern.

---

## 3. Distribution

- **Cargo:** `cargo install --locked cockpit-cli` (crate name);
  installs the `cockpit` binary. The crate name needs to be reserved
  early. Always document `--locked`: without it `cargo install`
  re-resolves past the published `Cargo.lock` and can pull a newer
  `bitflags` that overflows `dispatch2`'s `recursion_limit` (x86_64
  macOS, via the `arboard`/`keepawake` → `objc2` chain).
- **Homebrew:** `brew install cockpit-cli` via a tap (`brew tap
  christopher-kapic/tap`). ralph-rs and kctx-local both publish this
  way; copy their `scripts/` workflow.
- **Windows:** `scoop install cockpit-cli`, `winget install cockpit-cli`.
  Both expect a release-tag-with-binary workflow; `cargo dist`
  automates this.
- **No npm.** opencode is `npm install -g`; we are explicitly Rust-native.
- **No curl-pipe-bash installer** — script-based installers are a known
  attack surface.

### 3a. Optional `cock` shortcut prompt

The Homebrew, scoop, and winget post-install scripts (and a
first-run check for `cargo install` users, since cargo can't run
post-install hooks) prompt the user once:

> Install the `cock` shortcut command? It's an alias for
> `cockpit` that triggers an ASCII-rooster splash on launch.
> Purely cosmetic; you can install or remove it later with
> `cockpit shortcut install` / `cockpit shortcut remove`.
>
> [Y/n]

If yes: write a tiny shim into the same install prefix as the
main binary. On Unix the shim is a shell script:

```sh
#!/bin/sh
COCKPIT_ROOSTER=1 exec cockpit "$@"
```

On Windows: a `cock.cmd` that sets `COCKPIT_ROOSTER=1` and execs
`cockpit.exe %*`. Mark executable; done.

If no: skip silently. Re-prompt only when the user explicitly
runs `cockpit shortcut install`. Don't nag.

The `cockpit shortcut {install,remove,status}` subcommand
handles the lifecycle regardless of the original install path —
it just writes/removes the shim next to the user's resolved
`cockpit` binary (`which cockpit`).

---

## 4. Telemetry

**Status: none in v1.** We deliberately ship with no opt-in or opt-out
telemetry. If we later want anonymous usage stats (e.g. for `cockpit
connect`'s billing), it must be:

- Opt-in only.
- Documented in `GOALS.md` before code lands.
- Sourced from the WebSocket relay's already-authenticated session, not
  from a sidechannel.

---

## 5. Logging

- Use `tracing` (industry default; supports structured logs and span
  context).
- Default sink: nothing in interactive mode. With `--print-logs`, write
  to stderr. Always write to `~/.local/state/cockpit/logs/cockpit-YYYY-MM-DD.log`
  rotated daily, capped at 100 MB total.
- **Never log redacted values, even in debug mode.** The redaction layer
  (per `GOALS.md` §7) sits between the prompt builder and the network
  client; any `tracing::debug!` of the prompt body must use the same
  redaction filter.
- **Structured repair events.** The tool-input repair layer (`GOALS.md`
  §12) emits `tracing` events at `INFO` with a stable field set:
  `event = "tool_input_repair"`, `tool`, `model`, `kind` (catalog name
  | `relational_default` | `markdown_link_unwrap`), `outcome`
  (`repaired` | `invalid`), and a redacted excerpt of the offending
  input. These records are what `cockpit debug repair` (and an
  agent piped its JSONL output) reads to track which models break
  which contracts. The field set is stable — renaming any of
  `tool`, `model`, `kind`, `outcome` is a breaking change for the
  repair-summary tooling. Redaction runs over the excerpt before it
  hits the log, same as any other prompt body.

---

## 6. Error reporting

- `anyhow::Error` for user-facing errors; `thiserror` for typed library
  errors. Match ralph-rs and mcp2cli-rs's pattern.
- Exit codes (matching kctx's discipline):
  - `0` success
  - `1` cockpit error (bad config, no provider, etc.)
  - `2` harness terminated abnormally (signal, spawn failure, timeout)
  - `3` harness ran but exited non-zero
  - `4` redaction failure (refused to send a request because secret
    scanning errored out)
  - `64` usage error (clap's default, kept verbatim).

---

## 7. Multi-context primitives: subagent vs fork

Per `GOALS.md` §4c, cockpit exposes two complementary primitives for
running an agent against more than one conversation state. **Neither
is a subprocess concept.** Both run inside the cockpit **daemon
process** (see `GOALS.md` §8); "process forking" with separate
cockpit subprocesses is not part of v1.

### `subagent` — fresh, scoped child context

- A `task` tool call spawns a child agent whose conversation starts
  empty save for the task brief (`TaskPacket`).
- The child sees: the brief, the tool registry, agent-file prompt
  variants for its model.
- The child does **not** see: the parent's user prompts, the
  parent's tool calls, the parent's reasoning, prior subagent
  reports.
- The parent sees: the child's final structured report (per
  `reporting_contract`). Never the transcript.
- Used for "delegate this scoped piece of work; report back."

This is the standard delegation primitive. It's the default for the
model-facing `task` tool.

### `fork` — branch the conversation thread

- Branches the parent's session at a turn boundary (the codex
  `ForkSnapshot` model: explicit `TurnId` or a synthesized mid-turn
  snapshot).
- The branch **inherits the parent's full conversation up to the
  fork point** and diverges from there.
- Both branches are first-class sessions in cockpit's session DB and
  share a parent pointer. The user (or the model) can switch
  between them.
- Used for "explore an alternative direction from here." Pairs with
  oh-my-pi's branch summaries (reconstituted-on-return).

This is what the user / model invokes when the *setup is the
value* — e.g., "ask the same hard question of Opus and Sonnet from
the same context" or "try fix A vs fix B from this turn."

### Why both, not one

The two primitives optimize for opposite things:

| Question                                  | Subagent | Fork |
|-------------------------------------------|----------|------|
| Is the parent's setup useful to the child?| no       | yes  |
| Does the parent need the child's reasoning?| no      | n/a — same thread |
| Token cost of spawning                    | low (just brief) | high (inherits history) |
| Use case                                  | "do this small thing" | "what if from here?" |

### Concurrency: file-lock manager runs in the daemon

Because both primitives run inside the single cockpit daemon
process (`GOALS.md` §8), the file-lock manager (`plan.md` §4.1)
is a straightforward in-memory data structure with a SQLite-
persistence layer for crash-recovery. No inter-process locking,
no `.delivering-<uuid>.json` TTL files for crashed-process
reclaim. The lock manager protects parallel work inside the
daemon from stepping on itself, including parallel `coder`
instances spawned by the ralph executor (background plan
execution; see `GOALS.md` §3b).

If a node ever needs **filesystem** isolation (a separate working
tree to compile in), that's a per-node `worktree: true` declaration
on the graph node — it triggers a `git worktree add` for that node
only, not a session-wide concurrency mode.

### Per-call selection

The choice is **per-`task`-call**, not session-wide. The model
specifies `task({ ..., mode: "fork" })` or `task({ ..., mode:
"subagent" })`; the default (when omitted) comes from
`default_delegation` in `config.json`.

---

## 8. Editor integration (out of scope but worth noting)

opencode has IDE extensions. `cockpit` will not ship any in v1.

That said, the JSON-event stream emitted by `cockpit run --format json`
must be **stable** — when an IDE extension is eventually built, it
will tail that stream. Document the schema in `docs/json-events.md`
once defined.

---

## 9. Naming

- **Binary name: `cockpit`.**
- **Crate name: `cockpit-cli`** on crates.io. The `-cli` suffix
  exists because the unsuffixed crate name may already be
  claimed; the binary the crate installs is just `cockpit`.
- **Optional shortcut: `cock`** — installed only if the user
  opts in at install time (see §3). It's a one-line shim that
  sets `COCKPIT_ROOSTER=1` and execs `cockpit`. The binary
  detects the env var and renders an ASCII rooster on launch
  (art TBD; designed elsewhere). Pure easter egg — `cock`
  is identical to `cockpit` in every other respect.
- **Branding:** lowercase `cockpit` everywhere in user-facing
  strings, except in the README header where "Cockpit" is fine.

### 9a. Known binary-name conflict: cockpit-project.org

The Linux server-admin web UI [cockpit-project.org](https://cockpit-project.org/)
ships a `cockpit` binary (and `cockpit-bridge`, `cockpit-ws`).
On Fedora / RHEL / Debian systems with the cockpit-project
package installed, `which cockpit` may resolve to their binary
instead of ours. Mitigations, in order of preference:

1. **PATH precedence.** Our installer guidance (`cargo install`,
   `brew install`, `scoop install`) places the binary in a
   user-controlled `~/.cargo/bin` or Homebrew prefix that
   normally precedes `/usr/bin` in `PATH`. Most users will get
   the right one by default.
2. **Disambiguate via the `cock` shortcut.** If the user
   installed `cock`, it's an alias they own and won't collide.
3. **Detection on first launch.** If our binary detects that a
   different `cockpit` is on PATH ahead of it (via `which -a
   cockpit` comparison), print a one-time warning suggesting
   the user alias us or use the `cock` shortcut. Do not silently
   take over — the cockpit-project users got there first.

This is a known and accepted cost of the naming choice. The
warning + opt-in shortcut keeps it manageable.

### 9b. The `COCKPIT_ROOSTER` env var

Defined for one purpose: trigger the ASCII-rooster splash on
launch. Set by the `cock` shim; never set by the main `cockpit`
binary. Users can also set it manually in their shell rc if
they want the rooster on every launch without installing `cock`.

The art is rendered once at startup, after the TUI initializes
and before the first frame. If the terminal can't render it
(stdout not a TTY, `NO_COLOR=1` and the art relies on color,
window too narrow, etc.), the rooster is silently skipped — no
error, no fallback prose, just no rooster. Cosmetic only.

**Precedence over the default banner.** `GOALS.md` §1g introduces
a default startup banner (a P-51 Mustang) that renders on every
TUI launch unless suppressed. When `COCKPIT_ROOSTER=1` is set, the
rooster **preempts** the default banner — only one banner ever
appears, the rooster wins.

---

## 10. License

MIT, matching ralph-rs / kctx-local / mcp2cli-rs. Include the same
`LICENSE` file structure.

---

## 10a. Provider-auth posture: sanctioned vs passthrough

See **GOALS §20** for the full policy. Cross-cutting summary:

- **Sanctioned flows** (every API-key provider; GitHub Copilot's
  documented device-code flow) are the default in the Add-Provider
  wizard and require no special UX warning.
- **Passthrough flows** re-use a first-party client's OAuth client id
  to spend that vendor's subscription quota from cockpit. They are
  not officially supported by the vendor and may stop working
  without notice. Currently in this category:
  - **Codex / ChatGPT Plus/Pro** (`src/auth/codex.rs`, re-using the
    Codex CLI's `CLIENT_ID = app_EMoamEEZ73f0CkXaXp7hrann`).
  - **Anthropic Pro/Max** — *if* added; under discussion in
    `design-need-to-discuss-or-test.md`.

Passthrough flows must:
1. Not be the default in the Add-Provider wizard.
2. Display a one-line warning in the wizard and in `cockpit
   providers status`: *"Uses your <vendor> quota via <first-party
   tool>'s OAuth client. Not officially supported by <vendor>; may
   stop working without notice."*
3. Each live under its own `src/auth/<vendor>.rs`; no client-id
   sharing across modules.

---

## 10b. Reasoning / thinking-block preservation across providers

**The bug, in one sentence.** Anthropic's native Messages API signs
every `thinking` / `redacted_thinking` block it returns; when you
replay that assistant turn back to the API, the thinking blocks on
the *latest* assistant message must come back **byte-for-byte
identical**, or the request 400s:

```
messages.N.content.M: `thinking` or `redacted_thinking` blocks in the
latest assistant message cannot be modified. These block must remain
as they were in the original response.
```

This is a signature-integrity mechanism (thinking can't be forged,
edited, or reordered and replayed), not a competitive lockout — every
harness that drives the native Anthropic API hits it identically. The
high `content.M` index is the tell: it fires on turns with thinking
*interleaved with several tool-use blocks*, which is exactly the shape
a multi-tool planning turn produces.

**The two failure modes** (both bind only the *most recent* assistant
turn; older turns may be freely stripped):

1. **Modifying** a thinking block on the latest turn — re-encoding,
   trimming, reordering the block sequence.
2. **Modifying a sibling block** in the same turn when thinking +
   `tool_use` are combined — the safe reading is *don't touch the
   latest assistant turn at all* while thinking is live. When thinking
   and tool calls coexist, the thinking block must also be *present*
   (not stripped) on the turn you attach the `tool_result` to.

**Why cockpit is immune today — and where it stops being immune.**
v0 ships only the OpenAI-compatible Chat Completions variant
(`Model::OpenAi`, `src/engine/model.rs`); the Anthropic native variant
is still a stub. Chat Completions does not sign or require replay of
reasoning blocks, and `Model::complete` runs `strip_reasoning` over
the **entire** history before every request
(`src/engine/model.rs` — `history.iter().map(strip_reasoning)`), so no
thinking block is ever replayed. That blanket strip is safe for Chat
Completions but is **the wrong default the moment the native Anthropic
variant is wired**: stripping the latest turn's thinking when that
turn also carries a `tool_use` is itself a 400. So the rule below is a
precondition for building the Anthropic variant, not a live bug.

**Rules for the native Anthropic path (when built):**

- **Never strip reasoning from the latest assistant turn.** Make
  `strip_reasoning` position-aware: scrub thinking from turns `< N-1`
  for token economy ([[pruning_policy]]), preserve the most recent
  assistant turn's content vector verbatim — signature included.
- **Replay thinking blocks exactly as received**, including the
  opaque signature; store the raw content blocks, don't reconstruct
  them from the captured `ReasoningDelta` text (that text is a
  display projection and has no signature).
- **`rewrite_assistant_tool_call` is the in-house tripwire.** The
  §13c edit-cascade canonical rewrite (`src/engine/agent.rs`) mutates
  the *most recent* assistant message's tool-call args in place. On
  Chat Completions that's fine. On native Anthropic, mutating any
  block of a thinking-bearing latest turn risks the failure mode (2)
  400. Before enabling native Anthropic, either (a) suppress the
  in-history rewrite when the turn carries a thinking block (keep the
  canonical form only in the audit row's `wire_input`, which GOALS §14
  already separates from what the model sees), or (b) confirm against
  a live Anthropic endpoint that sibling-`tool_use` edits don't
  invalidate the thinking signature. GOALS §14 already commits to
  *never rewriting reasoning prose* — extend that invariant to "never
  mutate any block of a signed latest assistant turn."
- **Pruning interacts with this** ([[pruning_policy]]): a prune that
  drops the latest turn's thinking to save tokens trades a few hundred
  tokens for a hard 400. The position-aware strip above is the same
  predicate — older turns only.

---

## 11. What we explicitly will not do

A list to point at when feature requests come in:

- ~~No MCP support. Use `mcp2cli-rs`.~~ **Reversed 2026-05-27** —
  cockpit ships first-class MCP via lazy discovery (GOALS §18).
  mcp2cli remains supported as an alternative shell-wrap path.
- No bundled JS runtime / npm plugins.
- No hosted session sharing.
- No self-update.
- No telemetry in v1.
- No headless server in v1 (planned for `cockpit connect`).
- No GUI (web/desktop).
- No first-party LSP integration in v1.
- No `cockpit github` agent.
- No bundled gitbash on Windows — detect and direct.
