# cockpit-cli (cockpit) — Goals

`cockpit` is an AI coding harness written in Rust. Its design is informed
by [opencode](https://opencode.ai), [Claude Code](https://www.anthropic.com/claude-code),
and [codex](https://github.com/openai/codex) — but it is **not** a
drop-in for any of them. It has its own config files, its own session
DB, and its own opinions about file locking, context pruning, and
multi-harness orchestration.

This document is the authoritative statement of *what* `cockpit` is for. Design
trade-offs and feature-by-feature decisions live in the companion docs:

- [`opencode-features-review.md`](./opencode-features-review.md) — a
  design comparison with opencode (what to copy, what to deliberate,
  what to skip). Not a compatibility map.
- [`miscellaneous.md`](./miscellaneous.md) — Windows packaging, shell handling,
  and other cross-cutting concerns that don't have a doc of their own.

---

## Strategic vision

The feature set below is shaped by four load-bearing claims about
who `cockpit` is for and how it competes:

1. **Primary target: open-source models with ~120k context
   windows.** The harness is deliberately optimized for OS models,
   not for frontier models. Frontier models will of course work,
   but optimizing for them is not the differentiator. This is
   what drives the token-economy obsession (§10) and the
   tool-input repair layer (§12) — they earn their keep
   precisely because OS models bite harder on contract failures.
2. **24/7 plan execution with human-in-the-loop.** The endgame is
   a user who runs `cockpit` as a daemon, kicks off ralph plans,
   and resolves questions from a phone or browser over the
   course of a day. The system labor-multiplies the user, not
   the user's context window. This is what drives the daemon-in-v1
   (§8), the leaf-terminated invocation tree (§3a), the
   needs-attention queue (§3b), and the caller-based `coder` mode
   (§3b).
3. **Remote control via dashboard (v2).** `cockpit connect` (§8d)
   is a hosted WebSocket relay so users can monitor plans,
   resolve needs-attention queue entries, and steer agents from
   anywhere. The v1 daemon architecture is shaped to make this
   layering trivial — same wire protocol, different transport
   (§8c).
4. **Data-efficient interaction over SSH and remote links.** The
   harness should remain comfortable to use over SSH, mosh, and the
   future relay/mobile surfaces. That means we optimize not just for
   model-context economy but also for **wire economy**: summaries,
   deltas, citations, and on-demand expansion beat full transcripts,
   bulky live panes, and repeated large payloads. If a proposed
   feature is meaningfully data-heavy by default, the design must
   justify the cost explicitly rather than smuggling it in as
   convenience.

When a design choice trades convenience for OS-model viability,
OS-model viability wins. When a v1 decision constrains v2 (the
remote dashboard), prefer the v1 decision that makes v2 easier
even if it's marginally more work now.
When a design choice would increase routine SSH/remote bandwidth
without a proportional payoff, the data-efficient design wins by
default.

---

## 1. Codex-style TUI

The user-facing interaction loop is modeled on the **codex** TUI (built with
`ratatui`). Concretely:

- Full-screen TUI with a chat surface, composer at the bottom, and a status
  line / footer.
- Slash commands (`/model`, `/agent`, `/skills`, `/help`, etc.) discoverable
  from a leader-less slash menu.
- Streaming markdown rendering with syntax-highlighted code blocks.
- Approval dialogs for sensitive actions (including `Shift+Tab` mode
  cycling for bash `exec_approval` flows; see `plan.md` §3e and
  `TUI-design-philosophy.md` §6 — will grow more sophisticated).
- Configurable keymap.

Use `kcl ask codex "<question>"` whenever you need to inspect codex's
behavior in detail — codex's own source is the reference implementation
for "what does this look like."

### 1a. TUI status line: cwd + git branch + context (always)

The TUI must **always** show the current working directory, the git
branch (when the cwd is inside a git repo), and a live **context usage
indicator**. These are not opt-in via `/statusline`; they are part of
the chrome.

- cwd: shown abbreviated (`~/p/d/cockpit-cli`) when it would otherwise
  overflow.
- git branch: shown with a leading `` symbol; absent (no slot, no
  placeholder) when the cwd is not a git repo.
- context: `ctx 65% → 42% prunable` — current fraction of the active
  model's window, plus the projected fraction after `/prune` would run.
  Ambient at all times; promoted (color shift, bolder treatment) when
  current usage crosses a configurable threshold (default 80%) so the
  user notices when the decision matters but isn't trained to ignore a
  constantly-screaming number.
- active agent: the name of the agent currently driving the
  conversation (e.g. `orchestrator-build`, `coder`, `explore`).
  Required because interactive subagents become the primary while
  they work (§3b) — the user must always know who they're talking
  to. Visually distinct from the cwd / branch slot.

### 1b. Vim keybinds in the composer

Editing the prompt with arrow keys is unacceptable. The composer **must**
support a Vim-mode editor with the standard Vim keybinds:

- Normal mode: `h j k l w b e 0 $ gg G x D Y p P i a I A o O d{motion}
  y{motion}`.
- Insert mode: `Esc` returns to normal mode; everything else passes through
  to the standard editor.
- Toggle is a **default-on** preference (not opt-in like codex's `/vim`),
  but can be disabled in `config.json` or via a slash command for users
  who want raw editor mode.

Implementation may borrow heavily from codex's `textarea.rs` Vim
state machine.

### 1c. Composer behavior: queued-message editing (must-have)

The user must be able to **type and send while the model is busy**.
Messages sent during a model turn are added to a per-session
**queue**, and the queue is delivered with the next inference
request the runtime makes for that session — not the next *user
turn*, but the next *inference call*. So if the model is mid-tool-
loop, the queued message rides along on the next tool-result →
inference round-trip. The agent sees the user's input as soon as
the runtime is going to talk to the model anyway.

Three behaviors are required, and they are non-negotiable:

1. **Up-arrow in an empty composer pops the most recently queued
   message back into the composer for editing — and removes it
   from the queue.** opencode loads the message into the composer
   but leaves the original in the queue, so the same message gets
   sent twice. That bug is the reason this is called out as a
   must-have. cockpit's rule: pop is destructive. Edit, then either
   re-send (queues at the tail again) or `Esc` / `Ctrl+C` to
   discard. Repeated up-arrows pop progressively older queued
   messages, then fall through to the standard send-history
   recall once the queue is empty.
2. **Multiple queued messages are folded into one message** before
   being sent. Two consecutive queued items `A` and `B` become a
   single user message `A\n\nB`. The user composed them as
   separate thoughts; the model sees one coherent message. No
   special framing or numbering — the user added structure if
   they wanted it.
3. **The queue is delivered at the next inference boundary**, not
   buffered until the model finishes its turn. Mid-tool-loop:
   the next tool result going back to the model has the queued
   message attached as the next user content. End-of-turn: the
   queue is delivered as the first content of the next request.
   Empty: no queue, no behavior change.

Affordances:

- The chrome shows a queued-count indicator (`queued: 2`) so
  the user knows what's pending.
- `/queue` (no args) shows the queue stack and lets the user
  drop or reorder; `/queue clear` empties it.
- The active-agent name in the chrome (§1a) tells the user *who*
  the queued messages will be delivered to — important when an
  interactive subagent has been swapped into the foreground
  (§3b).

Interaction with interrupt: `Enter` sends/queues; the explicit
interrupt path (`Ctrl+C` or `Esc Esc`, TBD) cancels the in-flight
model call without affecting the queue.

### 1d. Exit leaves the transcript tail in the terminal

When the user runs `/exit` (or `Ctrl+D` from an empty composer),
cockpit tears down the TUI but **writes the tail of the conversation
to the primary terminal buffer** before quitting. The user can
scroll up in their terminal and copy commands, paths, or any text
the agent produced in the last few turns.

This is a deliberate divergence from the typical
full-screen-TUI-in-alt-screen pattern, where exiting wipes
everything the user saw during the session. Claude Code does
this right (it renders directly in the primary buffer the whole
time); opencode and codex use the alt screen and the session is
gone on exit. cockpit splits the difference: alt screen *during*
the session for the clean full-screen experience (status line,
floating slash menu, approval dialogs), then on exit it leaves
the alt screen and prints the last N turns to stdout before
returning control to the shell.

- **Default:** the last 100 rendered lines (or everything since the
  last `/clear`, whichever is fewer).
- **Configurable:** `tui.exit_tail_lines` in `config.json`. Set to
  `0` to disable (clean exit, no tail); set to `-1` for the whole
  session. Line-counted rather than turn-counted because turn sizes
  vary by orders of magnitude (a one-line "yes" vs a 2000-line tool
  dump), and the user's interest is in "a screenful or two of
  recent context I can copy from," which lines track better than
  turns.
- **Formatting:** transcript is rendered in a copy-friendly form —
  no box characters, no color codes (or stripped via `NO_COLOR`
  semantics), agent names prefixed (`orchestrator-build:`,
  `coder:`) so multi-agent sessions stay readable. Code blocks
  preserved verbatim. The point is: commands the agent gave you
  should paste cleanly.

### 1e. Composer `@`-tagging: inline files and directories

The user can `@`-tag files and directories in the composer. When
the message is sent, cockpit inlines the referenced content into
the request **before** it reaches the model. This is a composer
feature (the TUI does the inlining); the model sees a normal
user message with the file content attached, not a special
syntax.

Syntax:

- `@path/to/file.rs` — inline the file. Path is resolved
  relative to the cwd; absolute paths work too. Tab-completion
  in the composer offers file/directory completions.
- `@path/to/file.rs:10-80` — inline lines 10 through 80
  inclusive (1-indexed, half-open or closed range is the same
  syntax Claude Code uses — closed range here). For a single
  line: `@file.rs:42`.
- `@path/to/dir/` (trailing slash, or detected directory) —
  inline a directory listing (name + type + size, sorted),
  not its contents recursively.

Behavior:

- **The inliner reuses the `read` tool's formatter, but is an
  *automatic* tool the model cannot invoke.** `@`-tag expansion
  is a composer-side helper, not a model-facing tool: the model
  never calls it, and (per the Anthropic API, which forbids
  `tool_use` blocks inside a user turn) the fetched content rides
  *inline* in the user message between fenced markers
  (`<file path="...">...</file>`) rather than as a real tool turn.
  Files route through the shared `read_slice` formatter, so the
  inlined content is **line-numbered** with the same 2000-line /
  ~8 KB caps, the same truncation marker, and the same redaction
  as the model-invoked `read`. (If the model wants a directory
  listing *its* way, it runs `bash ls` — that's the model-facing
  route; the `@`-tag inliner is not.)
- **Each expansion shows in the chat as a harness-automatic
  tool-call entry.** When a tagged message is sent, every `@`-tag
  renders a one-line tool-call entry in the transcript
  (`→ read(path) ✓ 142 lines`, `→ list(src/) ✓ 23 entries`,
  `→ read(big.rs) ✗ 9001 lines — referenced, not inlined`) in the
  same idiom the agent's own tools use — so the user sees exactly
  what the composer fetched and what it cost. The agent didn't
  invoke them; the composer did.
- **Directory listings are produced internally (no shell-out),**
  cap 100 entries, sorted (directories first, then alphabetical).
  Beyond the cap the footer is `... N more entries; @-tag a
  subdirectory or ask explore for a search`. An internal
  `std::fs::read_dir` listing is portable across Linux/macOS/
  Windows and gives deterministic name + type + size output —
  preferred over invoking `ls` (which would need a per-OS command
  map and a working shell). All entries are listed, including
  hidden (`.`-prefixed) ones.
- **Over-cap files are referenced, not inlined.** A full-file tag
  whose content exceeds the `read` cap (2000 lines or ~8 KB) is
  **not** inlined — the `@path` survives verbatim with a one-line
  note (`[note: @big.rs is N lines — not inlined; ask read with
  offset/limit]`) so the model knows it exists and can pull what
  it needs on demand. Auto-dumping a multi-thousand-line file the
  user may not need is exactly the context bloat the token economy
  (§10) avoids. A tag with an explicit range (`@file:10-80`,
  `@"my file.rs":10-80`) is always inlined — the slice is bounded.
- **Redaction runs over inlined content.** A `.env` file
  tagged with `@.env` is scanned for secret-shaped values
  before the request is built (§7). Same chokepoint, no
  bypass — `@`-tagging is one more input source for the
  redaction layer to handle.
- **Inlining cost is visible.** The composer's queued-count
  indicator (§1c) gains an "attached: N files, ≈M tokens"
  affordance so the user sees how much they're spending on
  this turn before they hit Enter.
- **Suggestion popup: deepening search + scroll window.** Typing
  `@` opens a popup of cwd entries (gitignore- and hidden-aware).
  As the query narrows, if fewer than 6 matches remain at the
  current depth the search widens **breadth-first one directory at
  a time** (matching basename prefixes) until it reaches 6 or
  exhausts the subtree — so the popup stays useful in sparse
  directories. All matches are kept (not just the visible 6); the
  popup shows a 6-row window and scrolls with a one-row margin so
  the next / previous candidate is always visible except at the
  true ends of the list. The `/model` picker uses the identical
  scroll window.
- **Tab vs Enter.** `Enter` commits the highlighted candidate
  (file or directory), appends a trailing space, and closes the
  popup. `Tab` does the same for a file, but on a **directory**
  descends into it (no trailing space; the popup stays open and
  re-queries inside the directory) so the user can keep navigating.
- **Spaces are quoted automatically; the composer stays clean.** A
  path with spaces is shown unquoted in the composer; on submit
  the runtime wraps autocompleted spaced paths in quotes
  (`@"my file.rs"`) on a throwaway copy so the tokenizer reads them
  as one tag — the visible buffer never shows the quotes. To
  hand-type a spaced path the user types the quotes themselves
  (`@"path with spaces"`); the popup keeps narrowing inside an open
  quote, and a bare typed space otherwise ends the tag.
- **Backspace over a completed tag deletes the whole tag.** Once a
  tag is completed (autocompleted, or typed and terminated),
  Backspace at its right edge removes the entire `@tag` atomically
  rather than one character; a tag still being composed (popup
  open) deletes character-by-character. The trailing space is two
  keystrokes (space, then the tag). Forward-`Delete` from the
  tag's left edge mirrors this.

- **Gitignored files cannot be `@`-tagged by default.** If
  the cwd is inside a git repo, cockpit parses the active
  `.gitignore`s (`<repo>/.gitignore`, ancestor
  `.gitignore`s, `.git/info/exclude`, and the user's
  `core.excludesfile`) and **refuses** to inline files that
  match. The refusal message names the matched pattern and
  the rationale: *"`.env` matches `.gitignore` pattern
  `.env`; gitignored files are blocked from @-tagging
  because they frequently contain secrets. Enable
  `composer.tagging.allow_gitignored_files` to override
  (see §4)."* The redaction layer (§7) is the last line of
  defense, but blocking at the input boundary is cheaper
  and surfaces the choice to the user. Outside a git repo,
  no `.gitignore` semantics apply — all readable files are
  tag-able.

  > **Implementation status (2026-05-28):** the *suggestion
  > popup* already excludes gitignored + hidden entries (the
  > `ignore`-crate walk), so autocomplete never offers them.
  > Blocking a *manually typed* gitignored path at the inline
  > boundary is **not yet wired** (it needs the gitignore
  > matcher + the `allow_gitignored_files` config threaded into
  > expansion). Until then redaction (§7) is the backstop for a
  > hand-typed `@.env`. Tracked in `flagged-for-christopher.md`.

Failure modes:

- `@nonexistent.rs` — composer shows a warning inline and
  refuses to send until the user fixes or removes the tag.
  Silently dropping a tag would be confusing.
- `@huge-binary.bin` (matches a binary heuristic: NUL bytes in
  the first 1KB, or extension blacklist) — refused with a
  message suggesting `head` via the bash tool if the user
  really meant it.
- Symlinks resolve to their target; loops are detected and
  refused. The gitignore check runs against the **target**
  path, not the link path — a symlink into a gitignored
  directory is still blocked.

### 1f. External `$EDITOR` handoff for long prompts

Long prompts are painful to edit inside even a vim-mode textarea.
When the user starts typing (or the buffer grows long), the composer
surfaces a small hint in the input chrome or footer:
``press ctrl+g to edit in <editor>`` (showing the resolved name, e.g.
"lvim", "nvim", or "code --wait").

- Default binding: `Ctrl+G` (Claude Code convention; also used by
  many other TUIs).
- Resolves `$VISUAL`, then `$EDITOR`. If neither is set, a red toast
  explains the requirement and points at docs — no silent fallback to
  a built-in editor.
- The handoff primitive (leave alternate screen + disable raw mode,
  write buffer to a cockpit-namespaced tempfile, spawn, read back on
  clean exit) is the one already sketched in
  `TUI-design-philosophy.md` §8. Non-zero editor exit or no change on
  disk = cancel (original composer text is preserved).
- Always available; not gated behind a `tui.vim_mode`-style toggle.
  The binding may later live under a configurable `tui.keymap` block.
- Applies to new prompts, history recall (Up arrow), and queued
  messages popped for editing.
- The `?` help overlay and `/help` document the binding.

This is a **COPY from Claude Code**. It directly fulfills the
"composer overflow" / "prompt editing" case called out for `$EDITOR`
handoff in the design philosophy. The same mechanism is reused for
in-TUI editing of agent files, custom slash commands, and other
multi-line text the user owns.

### 1h. Diff rendering for edit/write tool calls

`edit` / `editunlock` tool calls render as a styled diff in the
history pane rather than the raw "edited file X" line every other
harness uses. Three modes, configured via `tui.diff_style`:

- **`side-by-side`** (default) — two columns, old on the left, new
  on the right, separated by a vertical rule. Degrades to `inline`
  dynamically when the terminal is narrower than 80 columns.
- **`inline`** — unified diff. Removed lines prefixed `-` in red;
  added lines prefixed `+` in green; ±3 context lines around hunks,
  with `…` separators between collapsed regions.
- **`hidden`** — one-line summary (`✓ edit: path/to/file.rs
  (+N −M)`). Useful when the user wants to see *that* edits
  happened but doesn't want the diff churn in the transcript.

Diffing uses the [`similar`](https://docs.rs/similar) crate (line-
granular `TextDiff`, ±3 context). Tool-call args (`old_string`,
`new_string`) are captured at `ToolStart` and consumed at
`ToolEnd` to assemble the `HistoryEntry::Diff` row.

`write` / `writeunlock` are **not yet** diff-rendered — the engine
doesn't currently surface the pre-write file content to the TUI,
so we have no `old` to diff against. Follow-up tracked in
`flagged-for-christopher.md`.

### 1g. Startup banner

cockpit renders a small pixel banner on TUI startup, after raw mode
is enabled and before the first frame. The default art is a P-51
Mustang in 256-color ANSI half-blocks; the reference rendering
lives in `p51-6.sh` in the repo root (color palette + character
grid). The banner is **on by default**.

Suppression — any of:

- `NO_COLOR=1` in the environment.
- stdout is not a TTY (piped, non-interactive).
- Terminal narrower than the art (~36 columns).
- `tui.banner.enabled = false` in `config.json` (§4j).
- `--no-banner` CLI flag on the launching command.
- Terminal does not advertise 256-color support (degrade-to-nothing
  rather than degrade-to-monochrome — a four-color plane isn't
  worth the special case).

Interaction with the `cock` shortcut (`miscellaneous.md` §3a): when
`COCKPIT_ROOSTER=1` is set — which the `cock` shim does — the
rooster splash **preempts** this banner. Only one banner ever
renders.

Implementation: port the data structure in `p51-6.sh` (palette +
cell grid) into Rust. Rendering uses crossterm's 256-color ANSI
sequences. The banner is one-shot; no animation.

### 1i. Embedded `$EDITOR` pane (`/editor`)

A live `$EDITOR` running **inside** a ratatui pane — not the
suspend-the-whole-TUI handoff (that's §1f / Ctrl+G, which edits the
*composer text*; this edits the *project*). The editor child runs in
a PTY and is rendered into a pane carved out of the chat body region.
This is what makes splits and live resize possible.

- The command only exists in the slash menu when `$EDITOR` is set;
  hidden otherwise.
- `/editor` (no arg) opens the editor **fullscreen** — it fills the
  history+composer body region. `/editor right`, `/editor left`,
  `/editor bottom` (alias `down`), `/editor top` (alias `up`) open a
  split: the named side is the **editor** pane, the chat occupies the
  remainder. Default split ratio 50/50, persisted for the session.
- The child launches with the TUI's cwd as its working directory and
  **no file argument** — the editor does its own thing (empty buffer
  or its own file browser). `$EDITOR` may carry args (`code -w`); it
  is shell-word-split, not treated as a bare program name.
- If a pane is already open, **any** `/editor …` (or `/lazygit`) is a
  no-op — one embedded pane at a time.
- The PTY is resized (pty resize + SIGWINCH) whenever the pane rect
  changes, so the child reflows on splits, divider drags, and
  terminal resizes.

**Chrome stays.** "Fullscreen" fills the body region only; the
always-on chrome (cwd + git branch + context indicator + active
agent, §1a) remains visible, and the composer stays below the pane so
the user can keep talking to the agent. This honors the documented
always-on invariant — the pane never hides chrome.

**Focus & close.** A pane auto-closes when its child exits (`:q`).
`Ctrl+O` toggles focus between the pane and the composer; clicking a
pane focuses it. `Ctrl+X` force-closes a pane even if the child is
still running (terminates and reaps it). Closing returns focus to the
composer. These two binds are reserved by cockpit while a pane is
open and are not delivered to the child (the unavoidable cost of an
embedded terminal — documented, and chosen to not collide with the
*composer's* vim mode or existing TUI handlers).

This is a **COPY of the embedded-terminal pattern** from editors like
helix/zellij, adapted to cockpit's chat-body layout.

### 1j. Embedded `lazygit` pane (`/lazygit`)

Same embedded-PTY machinery as §1i, for `lazygit`.

- The command only exists when `lazygit` is on `PATH`; hidden
  otherwise.
- `/lazygit` opens **fullscreen only** (no split args).
- Same focus / force-close / auto-close behavior as the editor pane.
- lazygit drives its own mouse: when the pane is focused and the
  child has requested a mouse-tracking mode, mouse events are
  forwarded to the child PTY (SGR-encoded). See §1k's cross-cutting
  mouse note in plan T9.

### 1k. `!` one-shot shell mode (local-only)

A leading `!` in the composer puts the input in **shell mode** — a
one-shot command runner, not an interactive shell pane.

- While the composer buffer starts with `!`, the input box swaps its
  top border for a **"shell mode" label** (reusing the existing
  top-border-swap hook) and tints the border. Shell mode ends the
  moment the leading `!` is gone.
- On submit: strip the leading `!`, run the rest via the shell
  (`$SHELL -c`, fallback `/bin/sh`; Windows `cmd /C`) with cwd = the
  TUI's cwd. One-shot capture of stdout+stderr.
- The command and its (capped) output render as a chat entry so the
  user sees it; output is truncated in the display with a note to
  re-run in a real terminal for the full text.
- **Never sent to the agent.** The output is local-only: it is not
  added to the wire, is excluded from any future message, and does
  **not** count toward the context-token estimate (it uses a history
  variant the estimator ignores).

### 1l. `/git` — share command output with the agent

`/git <args>` runs `git <args>` locally (pager disabled, ANSI
stripped) with cwd = the TUI's cwd, **immediately**, and renders the
result in chat now — but it does **not** trigger a request.

- The agent-bound copy is packaged as
  `<git cmd="status">…output…</git>`, matching the existing
  `<file>` / `<dir>` wire convention (§1e), and **buffered**: it is
  silently attached to the **next** user message's wire text.
  - Agent idle → the block waits until the user actually sends a
    message; `/git` alone never starts a turn.
  - Agent busy → it still attaches to the next user message (which
    rides the existing queue path).
  - Multiple `/git` calls accumulate in order and all ride the next
    message.
- Because the block becomes outbound prompt content, it flows through
  `redact::scrub()` like any wire text — it is not bypassed.
- The agent-bound copy is capped (~2k tokens, §10) with a truncation
  marker; the chat display is capped separately with a re-run note.
- Buffered-but-unsent `<git>` blocks are surfaced in the context
  indicator's pre-first-response estimate so their token cost is
  visible before the user commits to sending.

---

## 2. cockpit-native config

`cockpit` has its own config files, in its own locations. It does **not**
parse opencode's `opencode.json` / `.opencode/` directories, and it
does not write to opencode's locations. An earlier draft of this doc
promised a drop-in opencode replacement; that goal was dropped — the
implementation complexity and the constant pull toward opencode's
schema were a poor trade for the convenience of "no migration step."

Layout — **layered, with walk-up discovery.** cockpit's config is
not a single global file with a single project overlay; it's a chain
of `.cockpit/` directories the resolver walks up from cwd, with
more-specific (deeper) layers overriding less-specific (shallower)
ones.

Discovery algorithm:

1. Start at cwd. For each ancestor directory, check for a
   `.cockpit/` directory. Collect every hit, deepest-first.
2. **Stop at the first ancestor that is one of `$HOME`, `/srv`, or
   `/opt`** — *inclusive*: that directory's own `.cockpit/` is
   read before the walk halts. This keeps the search from leaking
   past `$HOME` into `/`, and prevents service trees under
   `/srv/<orgname>/<project>/...` or `/opt/<vendor>/<project>/...`
   from reading siblings they shouldn't.
3. If cwd is not under any of those three roots (e.g.
   `/tmp/scratch`), skip the upward walk and use only
   `<cwd>/.cockpit/` (if present) plus the home-level configs
   in step 4.
4. Home-level configs are always considered: `~/.cockpit/`
   (least specific — broadest user-level default) and
   `~/.config/cockpit/` (XDG-canonical user-level). When both
   exist, `~/.config/cockpit/` overrides `~/.cockpit/` on
   conflict (matching the "more-specific wins" rule and the
   intent of the ordering above).
5. `COCKPIT_CONFIG` env var overrides discovery entirely — when set,
   it points at one specific config file used in place of the walk.

Example: cwd `~/projects/orgname/projectparent/project` reads (in
order from most-specific to least-specific):

1. `~/projects/orgname/projectparent/project/.cockpit/`
2. `~/projects/orgname/projectparent/.cockpit/`
3. `~/projects/orgname/.cockpit/`
4. `~/projects/.cockpit/`
5. `~/.config/cockpit/`
6. `~/.cockpit/`  (inclusive stop at `$HOME`)

Each discovered `.cockpit/` directory may contain `config.json`,
plus optional `agents/`, `commands/`, and `skills/` subdirectories.
Agent, command, and skill discovery walks the same chain (§3, §5).

The motivation is that one user often has meaningfully different
defaults across `~/projects/orgname-a/` and `~/projects/orgname-b/`
(different models, different agents, different permission rules)
and a flat global-plus-project model forces them to re-declare
org-level decisions in every project. Layering with a per-directory
override gives "set the rule once at the level it applies to."

The config schema is cockpit's own. We are free to borrow opencode's
schema shape where it's good (the `permission` block's
allow/ask/deny model, the agent-frontmatter format, the
provider-block structure), but the file names, directory layout,
and exact key set are ours. Migration from opencode is a one-shot
`cockpit config import-from-opencode` command, not an ongoing dual-read.

### 2a. The `extended-config.json` collapse

The original design split cockpit's config into two files:
`opencode.json` (compat layer) plus `extended-config.json`
(cockpit-only). With opencode-compat dropped, there's no longer a
reason for two files — everything goes into `config.json`. The
keys formerly described in §4 below now live under top-level
namespaces in the single config file. Migration: trivial; we
weren't shipping `extended-config.json` separately yet.

### 2b. Merge modes

When more than one layer is discovered (per §2), the resolved
config is the merge of every layer from least-specific to
most-specific. The merge isn't uniform — each config field is
**tagged in the schema** with one of four merge modes:

- **`replace`** — smaller-scope value wins outright; parent is
  dropped. Default for scalars.
- **`concat`** — values from all scopes are combined. May be paired
  with a negative counterpart list (e.g. `redact.allowlist` +
  `redact.denylist`; the resolved set is `(∪ allowlists) − (∪
  denylists)`).
- **`key-merge`** — concat + smaller-scope wins per key; the list
  is treated as a map keyed by an identifier field. The schema
  must specify the key identifier and any normalization rules
  (whitespace, glob equivalence, etc.).
- **`deep-merge`** — for maps; recurse into nested keys. Default
  for map-shaped fields.

A single global merge rule does not work for every field: `agents`
wants replace-on-collision (a project re-declaring an agent should
drop the parent version), `redact.allowlist` wants concat (a global
list of test-fixture strings should not be lost when a project adds
project-specific allowlist entries), and `providers.*` wants
deep-merge (a project setting `providers.anthropic.base_url` must
not nuke the global `providers.anthropic.api_key`). The four-mode
taxonomy is the smallest set that covers every case seen so far.

Per-field tags live alongside each schema field in §4.

### 2c. `/config` TUI

The TUI `/config` slash command opens a tabbed window — one tab per
discovered config layer plus an implicit "merged" view — for
inspecting and editing each layer in isolation. Each tab includes:

- The on-disk path of that layer's `config.json` (or "not yet
  created at this level" with a **create-file affordance** so users
  can introduce a new layer at, e.g., `~/projects/orgname/.cockpit/`
  without leaving the TUI).
- A form over a curated subset of high-traffic settings (model,
  default agent, vim mode, redaction toggle, theme).
- An "open in `$EDITOR`" escape hatch for everything the form
  doesn't cover.

Curated form over auto-reflected schema is deliberate: full
reflection would make every new config field a UI task and the
editor would drift out of sync with the schema. Curated means some
settings still require hand-editing the file — the open-in-editor
affordance is the escape hatch for everything not in the form.

The resolver runs in the daemon (§8): the TUI never walks the
filesystem itself. It receives a structured `{layers: [...],
merged: {...}}` payload over the daemon wire schema and renders
tabs from it. The same payload is what the v2 websocket relay
will deliver to remote clients (§8d) — one config surface, two
transports.

---

## 3. Arbitrary agent definition files

Most coding harnesses require agent files to live in a fixed config
directory. That's limiting: many users have agent definitions that
live elsewhere — in shared dotfiles repos, in another tool's
config, or checked into a project at a path other than the default.

`cockpit` allows arbitrary agent paths. Three mechanisms:

1. **Per-invocation flag**: `cockpit --agent-file ./path/to/agent.md run "…"`
   — load this single file as the active agent for this invocation,
   regardless of where it lives.
2. **Directory inclusion**: in `config.json`, an `agent_dirs: []`
   array adds extra directories to the agent search path. Files in
   these directories are merged with the cockpit-native locations
   (`<project>/.cockpit/agents/`, `~/.config/cockpit/agents/`).
3. **Symlink-friendly**: the standard pattern of symlinking external
   directories into the agents folder continues to work (e.g.
   `~/.config/cockpit/agents/ralph2 -> ~/.ralph2/agents`). This
   is supported by file-system semantics; cockpit just doesn't fight it.

Agent files use a frontmatter shape compatible with
opencode/Claude-Code-style agent definitions (`description`, `mode`,
`model`, `temperature`, `tools`, `permission`, `prompt`, etc.) — we
borrow the format because it's well-designed, not because we promise
file-level compatibility. An agent file written for opencode will
parse cleanly in cockpit; the reverse is not guaranteed if the cockpit
agent uses keys that opencode doesn't recognize.

### 3a. Bundled agent cast (v1)

cockpit ships five built-in agents (two orchestrator variants + three
specialists). The cast is deliberately minimal — these compose into
"plan ↔ build → look at project → look at deps → write." Anything
else is a user-authored agent.

| Agent                 | Mode     | Cwd | Purpose |
|-----------------------|----------|-----|---------|
| `orchestrator-build`  | primary  | project root | The traditional coding-harness experience. Owns the user's conversation when the focus is *making the change*. Tools: `read` (shallow inspection of files the user references; not for searching), `task` (delegation), `skill`. May invoke `explore` interactively (one at a time) or in the background (multiple in parallel). May invoke `coder` interactively (one at a time). Does not directly `write`/`edit` and does not hold file locks. |
| `orchestrator-plan`   | primary  | project root | Ralph-style planner. Owns the user's conversation when the focus is *deciding what to do*. Tools: `read` (shallow inspection), `task`, `skill`, plus plan-graph tools (create / append / update / delete / trigger). Sees in-progress and not-yet-implemented plans (completed plans are hidden by default; see "plan visibility" below). Triggering hands the plan off to the ralph executor (see §3b "Background agents") — `orchestrator-plan` does **not** hold the user's conversation while the plan runs; the user keeps talking to `orchestrator-plan` about other plans. Delegates to `explore` (interactive one-at-a-time, or background multi-parallel). Does not write code. |
| `explore`             | subagent | project root | Read-only investigator over the **current project**. Tools: `read`, `bash` (raw `rg`/`fd`), and the read-only codebase-intelligence tools (§21). Cannot invoke subagents. Returns `file:line` citations, not prose summaries. |
| `coder`               | subagent | project root | The only agent that holds file locks and writes/edits. Tools: `read`/`readlock`/`write`/`writeunlock`/`unlock`/`edit`/`bash`/`task` plus the codebase-intelligence tools (§21). The `task` permission is scoped to `docs` only (noninteractive, may run multiple in parallel). Receives a scoped task from its caller, makes the changes, returns a structured report. Mode is set by caller: interactive when invoked by `orchestrator-build`, noninteractive when invoked by the ralph executor (see §3b). |
| `docs`                | subagent | (pipeline; see below) | A **fixed two-stage, fully-noninteractive pipeline** that answers a caller's question about how to use a third-party dependency, by reading that dependency's *actual source code*. Not a single read/bash investigator and not general delegation — the driver routes it (`engine::docs_pipeline`). **Docs.1 (resolver)** runs in the caller's cwd with `list-packages` / `add-package` / `bash` / `webfetch` / `websearch`; it confirms or shallow-clones the dependency into cockpit's package registry (§4d-bis) and sees **only** the package name (the question never enters its context — token economy §10). **Docs.2 (answerer)** then runs in the resolved package directory with `read` + the sandboxed `grep` / `glob` only — no bash, no network, no write — and produces `file:line`-cited output from the dependency source. Cannot invoke subagents. |

**Why two orchestrators.** "Plan" and "build" are different
cognitive modes, not different priorities of the same mode. A
planning conversation talks about the *graph* (nodes, edges,
dependencies, what to do next); a building conversation talks
about the *code* (this file, this function, this diff). Bundling
both into one agent forces the model to context-switch every turn
and produces worse output in both modes. `/plan` and `/build`
slash commands swap which orchestrator owns the conversation.

**Structural payoff of the specialist split.** Only `coder`
writes. The file-lock manager (`plan.md` §4.1) therefore has a
single writer per delegation tree — much simpler concurrency
reasoning than "any agent might write at any time." Parallel
`coder` instances (under graph plans driven by `orchestrator-plan`)
are arbitrated by the lock manager as designed.

**Invocation tree (who can spawn whom).** The hierarchy is
deliberately shallow and leaf-terminated:

```
orchestrator-build → explore, coder
orchestrator-plan  → explore   (+ triggers plans via the ralph executor)
coder              → docs
explore            → (leaf, cannot spawn)
docs               → (leaf, cannot spawn)
```

The leaf-termination rule (explore and docs cannot spawn) is what
keeps context aggregation tractable: every delegation tree has a
bounded depth and a single writer. `docs` is itself a fixed
two-stage internal pipeline (Docs.1 resolver → Docs.2 answerer; see
the cast table and §4d-bis), but to its caller it is a single
noninteractive leaf — the two stages are an implementation detail of
the `docs` unit, not additional delegation, so leaf-termination holds.

**Multi-task decomposition on `orchestrator-build` (sequential, fresh
context per task).** A single user prompt often contains several
distinct tasks. Rather than carry all of them in one ever-growing
context, `orchestrator-build` **orders them and the runtime runs one
fresh `orchestrator-build` episode per task, sequentially** — each task
starts with clean context for maximum reasoning quality (priority #1,
the weak-model target). This is **runtime-owned episode sequencing, not
self-spawn**: `orchestrator-build` does *not* appear in its own
invocation-tree children, the scheduler owns the task queue, and the
"one interactive agent at a time / interactive agents don't spawn
interactive agents" rules (§3b) are preserved unchanged.

- **Single small task** is the common case: no decomposition, no
  episode machinery — `orchestrator-build` briefs `coder` directly. The
  `coder` hop is kept *cheap* (fast-path spawn, minimal ceremony) so a
  one-line change isn't penalized for not being a write-capable
  orchestrator. (`orchestrator-build` stays delegation-only in v1;
  direct-write for small tasks is deferred — see
  `design-need-to-discuss-or-test.md` D17, contingent on per-agent tool
  descriptions.)
- **TUI invariant.** Episode sequencing must be indistinguishable from
  a normal session: the chrome keeps showing `orchestrator-build` across
  episode boundaries, there is no visible "subagent returned / new agent
  started" seam, and the next task's episode simply begins the way a
  fresh turn does today. The user is always "talking to
  `orchestrator-build`."
- **Mid-flight task add.** When the user adds a task while an episode is
  running, the active episode decides *now vs later* and uses
  `task_request` (§3b; urgency `now` / `after_current`) to either fold
  it into the current episode or append it to the runtime queue at the
  right position.
- **No parallel fan-out from `orchestrator-build`.** Tasks run one at a
  time. A user who wants tasks executed in parallel uses
  `orchestrator-plan` + the ralph executor (§3b) — that is the only
  parallel-execution path; `orchestrator-build` is not made ralph-like.

**Orchestrators may read, but should delegate searches.** Both
orchestrators get `read` so the user can `@`-tag a file (see §1e)
or ask "what does foo.rs say on line 42?" without a subagent
round-trip. The `read` tool's description, when surfaced to
orchestrators, includes a one-line nudge: *"For multi-file
investigation, codebase searches, or anything you'd describe
as 'figuring out where X lives,' spawn an `explore` subagent
instead — it returns `file:line` citations under a token cap."*
Orchestrators do **not** get `bash` or the codebase-intelligence
search tools directly — those are the routes that earn the explore
round-trip.

**Plan visibility on `orchestrator-plan`.** To keep the context
manageable on long-lived projects, `orchestrator-plan` sees only
**in-progress** and **not-yet-implemented** plans by default.
Completed plans are hidden from the default view but remain
accessible on request (`/plans completed`, or by name). A config
knob `plans.hide_completed_after_days` (default `30`) sets how
quickly completed plans drop out of the agent's working set.
Additionally, **on completion** each plan's transcript is
summarized into a one-paragraph result so the graph view stays
roughly fixed-size regardless of project age.

**Triggering plans is explicit, not automatic.** Plans never
run on their own — the user (or the user-controlled agent)
must trigger each run. This is so a workflow like "implement
plan A, review the PR, pull main, then start plan B" can avoid
merge conflicts the planner couldn't have predicted.

Users who want named personas, or who want a reviewer / committer /
researcher role, author their own agent files (see mechanisms 1-3
above). The bundled cast does not grow opportunistically.

### 3b. Subagents vs background agents

cockpit distinguishes two delegation models that share machinery
but serve very different purposes:

#### Subagents

A subagent is spawned by an orchestrator (or by another
non-leaf agent like `coder` → `docs`) for a *scoped piece of work*.
Subagents run in two modes; **the mode is set by the caller, not
by the subagent**.

- **Noninteractive.** The subagent runs to completion without
  user interaction; the caller receives only the final structured
  report. Reports are token-capped (see §10). This is the
  classic delegate-and-report pattern (`plan.md` §3d). Multiple
  noninteractive subagents may run in parallel under a single
  caller — this is how `orchestrator-build` / `orchestrator-plan`
  fan out `explore`, and how `coder` fans out `docs`.

- **Interactive.** The subagent **becomes the primary agent**
  while it works. The user sees the subagent's outputs in the
  composer, talks to it directly, and approves its actions. The
  caller is paused — its conversation is preserved, just not
  active. When the subagent completes (or the user explicitly
  returns control), the caller resumes and receives the
  subagent's report. Interactive subagents are strictly
  one-at-a-time per caller (only one agent can hold the user's
  conversation at a time). Interactive subagents do **not**
  directly spawn other interactive subagents; the interactive
  ownership model stays scheduler-owned so pause/resume semantics,
  event routing, and active-agent identity remain obvious.

  In interactive mode, the user sometimes asks the subagent for
  things that are *outside its assigned task* — "while you're at
  it, also check X" or "what was the orchestrator going to do
  next?". The subagent doesn't try to do them itself (scope
  discipline is what makes the report contract work). Instead it
  uses `task_request(...)` to hand the new work back to the
  caller/runtime with an urgency of `now` or `after_current`.
  `now` means "pause me and switch the user to a fresh-context
  sibling task immediately"; `after_current` means "queue this for
  the caller to schedule once I finish." The active subagent may
  attach a small set of **seed artifacts** (specific file reads,
  concise findings, open questions) so the follow-up starts with
  the right context without inheriting the full interactive stack.
  When the caller resumes, it reads its subagent's report plus the
  queued task requests and decides what to do about each entry
  (spawn a follow-up subagent, answer the user directly, ask a
  clarifying question, etc.).

  The TUI must make active-agent identity unambiguous —
  current-agent name shown in the chrome (alongside cwd / branch /
  ctx%) so the user always knows who they're talking to.

**Caller-based mode selection for `coder`.** `coder` is a special
case because it has two callers with opposite needs:

- Invoked by `orchestrator-build` (the user is hands-on): `coder`
  runs **interactive** (the user can intervene mid-edit, see what's
  about to be written, approve actions). One coder at a time per
  user session.
- Invoked by the **ralph executor** (background plan execution;
  see "Background agents" below): `coder` runs **noninteractive**.
  Multiple coders may run in parallel across plan nodes, arbitrated
  by the file-lock manager. Each `coder` can `raise_interrupt(
  description, question?)` to pause itself and push an item onto
  the daemon's **needs-attention queue** (see §8).

##### Interrupt payload schema

Every `raise_interrupt` call has a `description` (always) and an
optional `question`. The description is free-text — what
happened, why the agent paused, what state the work is in.
**If no question is provided, the user resolves with a free-text
reply** (often empty, meaning "you can continue"):

> Description: *"I could not modify `/etc/hosts` because I
> don't have OS-level write permission for it. Please `chmod`
> the file (or `sudo` me into it) and let me know when it's
> good to go."*
>
> User resolves with: *(empty)* — agent reads as "continue"; or
> *"I modified the file for you, continue."* — agent reads as
> context for its next action.

When the agent does have a specific question, it sets
`question.kind` to one of three shapes:

- **`single`** — mutually-exclusive options. Used for yes/no
  questions and "which one?" decisions.

  ```jsonc
  {
    "kind": "single",
    "prompt": "The migration adds a NOT NULL column. Backfill strategy?",
    "options": [
      { "id": "default_now",    "label": "Backfill with NOW()" },
      { "id": "default_null",   "label": "Allow NULL, backfill later" },
      { "id": "block_writes",   "label": "Take a brief write lock and backfill atomically" }
    ],
    "allow_freetext": true   // user may also reply with a free-text alternative
  }
  ```

- **`multi`** — non-mutually-exclusive options. Used when the
  agent is enumerating things to do/skip.

  ```jsonc
  {
    "kind": "multi",
    "prompt": "Which test files should I update?",
    "options": [
      { "id": "auth",     "label": "tests/auth.rs" },
      { "id": "session",  "label": "tests/session.rs" },
      { "id": "redact",   "label": "tests/redact.rs" }
    ],
    "allow_freetext": true   // user may add an additional option
  }
  ```

- **`freetext`** — no options; just a question. The user types
  the answer.

  ```jsonc
  {
    "kind": "freetext",
    "prompt": "What error message did the CI run show? (paste relevant section)"
  }
  ```

`allow_freetext` on `single` and `multi` gives the user a
permanent escape hatch — agents can't always predict the right
option set, and the cost of being wrong is the user can't reply
at all. Defaulting `allow_freetext: true` is the safer choice;
agents may set it `false` only when the option list is truly
exhaustive (rare).

The daemon's needs-attention queue (§8) stores these payloads
verbatim; the TUI client and the future web/mobile client both
render the same schema. Resolution writes a typed answer back
to the paused agent, which resumes from its tool call.

#### Background agents = ralph plan executions

Background agents are **not** a separate kind of agent — they are
**plan executions run by the ralph executor**, decoupled from
the user's interactive conversation.

Concretely:

- When `orchestrator-plan` (or the user directly, via slash
  command) triggers a plan, control passes to the **ralph
  executor**, a daemon-resident process (see §8) that walks the
  plan graph and spawns `coder` / `explore` / `docs` subagents to
  execute each node.
- The ralph executor is the *caller* for all subagents it spawns,
  so subagent reports flow back to it (not to `orchestrator-plan`,
  which has moved on to the user's next request).
- These subagents run **noninteractive** by default — they
  produce reports, not real-time conversation. When a `coder` in
  this mode needs human input, it uses `raise_interrupt` to push
  a typed question onto the needs-attention queue. The
  user resolves the question from the TUI (or, later, a remote
  client per §8), the answer is delivered to the paused agent,
  and execution continues.
- Status, progress, deferred questions, and final reports for
  every running plan are observable via the daemon's event
  stream — this is what powers the future dashboard surface.

**Why "background agent" is just a perspective, not a kind.**
The same `coder` binary running the same prompt has different
*caller semantics* depending on whether `orchestrator-build` or
the ralph executor spawned it. Modeling them as one primitive
keeps the file-lock manager, redaction layer, and tool registry
single-implementation. The difference is purely about which
process holds the report-back end of the channel.

---

## 4. Config schema (`config.json`)

> **Note:** Prior drafts split this into a separate
> `extended-config.json` layered on top of opencode's config (see
> §2a on the collapse). With opencode-compat dropped, the schema
> below lives as the *top level* of cockpit's own `config.json`, under
> the namespaces shown. Schema field names are otherwise unchanged.

**Merge modes (per §2b).** Each field below is tagged with the
merge mode the resolver uses when combining layers:

| Field                                                 | Merge mode |
|-------------------------------------------------------|------------|
| `harnesses`                                           | `deep-merge` |
| `agent_guidance_files`                                | `replace` |
| `default_delegation`                                  | `replace` |
| `agent_dirs`                                          | `concat` |
| `packages_directory`                                  | `replace` |
| `redact.{enabled, scan_environment, scan_dotenv, min_secret_length, placeholder}` | `replace` |
| `redact.extra_dotenv_paths`                           | `concat` |
| `redact.allowlist`                                    | `concat`; subtracted by `redact.denylist` |
| `redact.denylist`                                     | `concat`; subtracts from `redact.allowlist` |
| `tui.*`                                               | `replace` per scalar |
| `composer.tagging.*`                                  | `replace` per scalar |
| `utility_model`                                       | `replace` |
| `prompt_injection_guard.*`                            | `replace` per scalar |
| `system_prompt.*`                                     | `replace` per scalar |
| `permission` (added per §6a)                          | `key-merge` by `tool` pattern, smaller-scope ordered first — *pending confirmation* |
| Discovered agent set (`.cockpit/agents/` per §3)  | `key-merge` by agent name |

The `redact.allowlist` / `redact.denylist` pair is the canonical
example of paired concat: the resolved set is `(∪ allowlists) −
(∪ denylists)`. All denylists subtract from the union, not just
the smallest-scope one. That lets a project add a one-off
redaction the global config doesn't know about *and* punch a hole
in a global allowlist when needed.

Initial schema:

```jsonc
{
  "$schema": "https://app.flycockpit.dev/schema/config.json",

  // 4a. Other harnesses on the device.
  // The key is the harness name; the value describes how to invoke it.
  // See ralph-rs and kctx-local for the same shape.
  "harnesses": {
    "claude": {
      "command": "claude",
      "args": ["-p", "{prompt}"],
      "prompt_mode": "arg",            // "arg" | "stdin"
      "model_args": ["--model", "{model}"],
      "default_model": null,
      "supports_skills": true,
      "supports_agent_file": true,
      "agent_file_args": ["--agent-file", "{agent_file}"]
    },
    "codex": {
      "command": "codex",
      "args": ["exec", "{prompt}"],
      "prompt_mode": "arg",
      "model_args": ["-m", "{model}"]
    },
    "opencode": {
      "command": "opencode",
      "args": ["run", "{prompt}"],
      "prompt_mode": "arg",
      "model_args": ["-m", "{model}"]
    }
  },

  // 4b. Agent guidance file resolution.
  // The first existing file in the cwd (or its ancestors up to the git root)
  // wins. If none match, no guidance file is loaded. The order matters.
  "agent_guidance_files": [
    "AGENTS.md",
    "CLAUDE.md",
    ".github/copilot-instructions.md",
    ".cursorrules"
  ],

  // 4c. Multi-context primitives.
  // Both run in-process; this isn't about subprocess isolation.
  //
  // - subagent: child agent with a FRESH, scoped context (just a task
  //   brief — no inherited conversation). Returns a structured report;
  //   parent never sees the transcript. Pattern: "delegate this scoped
  //   piece of work; report back."
  //
  // - fork: branch the parent's conversation thread at a turn boundary.
  //   The branch INHERITS the parent's history up to the fork point and
  //   diverges from there. Both branches continue independently; the
  //   user (or agent) can switch between them. Pattern: "explore an
  //   alternative direction from here." (Codex's ForkSnapshot model,
  //   oh-my-pi's branch summaries.)
  //
  // The two are complementary, not mutually exclusive — a session uses
  // whichever fits the moment. The config knob below sets the DEFAULT
  // for the `task` tool when neither the model nor the user picks.
  "default_delegation": "subagent",    // "subagent" | "fork"

  // 4d. Agent search paths beyond cockpit's built-in locations (see §3).
  "agent_dirs": [
    "~/dotfiles/agents",
    "/srv/team-agents"
  ],

  // 4d-bis. Package registry — cockpit's own user-global registry of
  // dependency source clones the `docs` answerer (Docs.2, §3a) reads
  // from. The registry is a `packages` table in the global cockpit DB
  // (same store as `intel_*`; NOT project-scoped). Git clones land in
  // `packages_directory` (default `~/src/cockpit-packages/`) under a
  // percent-encoded identifier; identifiers are ecosystem-prefixed for
  // autonomous adds (`cargo:tokio`, `npm:@tanstack/query`, `pip:requests`)
  // to dodge cross-ecosystem collisions. Population is autonomous (Docs.1
  // shallow-clones from registry-declared repos — never a guessed URL),
  // or manual: `cockpit packages add <id> [--git <url>] [--path <dir>]
  // [--branch] [--shallow]`, `cockpit packages list`, and the one-way
  // `cockpit kcl import` (copies a local kcl install's `packages` rows
  // cockpit lacks, referencing kcl's on-disk clone paths as-is; never
  // writes back to kcl). v1 does not auto-pull on every query (cost);
  // `source_url`/`branch` are recorded so a future `cockpit packages
  // update` could.
  "packages_directory": "~/src/cockpit-packages",

  // 4e. Secret redaction (see §7) — toggles and additional sources.
  "redact": {
    "enabled": true,
    "scan_environment": true,
    "scan_dotenv": true,
    "extra_dotenv_paths": [],
    "min_secret_length": 8,           // skip short env values that would false-positive
    "placeholder": "***redacted-by-cockpit***",
    "allowlist": [],                  // values to NEVER redact (test fixtures, etc); concat across layers (§2b)
    "denylist": []                    // values to ALWAYS redact (project-specific); concat across layers; subtracts from allowlist (§2b)
  },

  // 4f. TUI preferences.
  "tui": {
    "vim_mode": true,
    "show_cwd": true,
    "show_branch": true,
    "banner": { "enabled": true },   // §1g — pixel banner on TUI startup
    // §1h — diff rendering for edit/write tool calls. Three modes:
    //   side-by-side (default) — two columns; degrades to inline at
    //     terminal widths below 80 cells.
    //   inline                  — unified diff with -/+ prefixes.
    //   hidden                  — one-line summary (path + churn).
    "diff_style": "side-by-side"
  },

  // 4g. Composer @-tagging (see §1e).
  "composer": {
    "tagging": {
      // DANGEROUS. When true, @-tagging is allowed on files matching
      // .gitignore patterns. Defaults to false because gitignored
      // files commonly contain secrets (`.env`, key material, build
      // outputs) and the redaction layer (§7) is a last line of
      // defense, not a first one. Enable per-project, not globally,
      // unless you trust every project's `.gitignore` discipline.
      "allow_gitignored_files": false,

      // When true (default), @dir/ listings include dot-prefixed
      // entries and gitignored entries — knowing a file exists is
      // much lower-risk than sharing its contents, and a complete
      // listing is what users usually want. Set false to omit
      // dot-prefixed entries from listings (gitignored entries are
      // still shown — set `.gitignore` entries themselves to hide
      // them at the source).
      "list_hidden_in_directories": true
    }
  },

  // 4h. Utility model.
  // A small/cheap model used for background work that doesn't need
  // the user's primary model: session auto-titling (§17d), the
  // prompt-injection guard when enabled (§4i), and future similar
  // background tasks. Identifier format mirrors the primary model
  // selector (e.g. "anthropic:claude-haiku-4-5-20251001"). Unset
  // disables every utility-model-dependent feature — auto-titling
  // is skipped and sessions display their session-ID as the label.
  "utility_model": null,

  // 4i. Prompt-injection guard.
  // Scans user-authored input (composer prose + `@`-tagged inlined
  // content per §1e) for prompt-injection patterns before the
  // request reaches the model. Off by default. v1 scope is
  // user input only; scanning incoming tool results is out of
  // scope and tracked in flagged-for-christopher.md. On detection,
  // the daemon warns the user and pauses the send for explicit
  // confirmation — it does not strip, block, or silently rewrite.
  // `model` falls back to `utility_model` (§4h) when null; if both
  // are unset and `enabled = true`, the guard logs a one-time
  // warning and behaves as disabled.
  "prompt_injection_guard": {
    "enabled": false,
    "model": null
  },

  // 4j. (banner) — see §4f "tui.banner" above.

  // 4k. System-prompt injection (§17g).
  // Volatile context (current time) is appended to user messages
  // rather than baked into the cached system prompt, to avoid
  // invalidating the provider's prompt cache on every send. The
  // first user message of a session always carries a timestamp
  // prelude; subsequent messages carry one only when the gap since
  // the last prelude exceeds this many minutes.
  "system_prompt": {
    "time_injection_interval_minutes": 5
  }
}
```

`cockpit` mutates this file via its own commands (e.g. `cockpit harness
add`, `cockpit redact disable`). No other tool reads or writes it.

---

## 5. Claude skills support

`cockpit` supports Claude-Code-style skills. Skill discovery walks
both cockpit-native locations and the cross-tool sharing locations
that other harnesses also read (so a user's `~/.claude/skills/`
investment doesn't have to be duplicated):

- `<cwd>/.cockpit/skills/*/SKILL.md` and ancestors up to the git
  worktree root.
- `<cwd>/.claude/skills/*/SKILL.md`, `<cwd>/.agents/skills/*/SKILL.md`
  — cross-tool sharing locations.
- `~/.config/cockpit/skills/` — cockpit's per-user location.
- `~/.claude/skills/`, `~/.agents/skills/` — cross-tool sharing
  locations.

We read the cross-tool locations because skills are *content* the
user authored, not config, and demanding duplication into a
cockpit-only directory would be hostile. We do **not** read opencode's
config-tree skill locations (`.opencode/skills/`,
`~/.config/opencode/skills/`) — those are inside opencode's config
directory, and reading them would imply the broader opencode-config
compatibility we dropped (§2).

Skills are loaded on-demand via a native `skill` tool exposed to
the model. The frontmatter shape is the Claude-compatible one
(`name`, `description`, plus an optional `model`-gated trigger
block).

`cockpit` does **not** require a separate `cockpit init` to enable skills
— they are auto-discovered.

---

## 6. `cockpit meta` — meta-harness

`cockpit meta` is a top-level subcommand that turns `cockpit` into an
orchestrator over **other** harnesses on the device. From inside `cockpit
meta`, the agent can:

- Invoke any harness declared in `config.json`'s `harnesses` block,
  including `cockpit` itself recursively.
- Read and manage `ralph-rs` plans and runs (`cockpit meta ralph status`,
  `cockpit meta ralph run …`). `ralph` loops and `cockpit meta` are designed to
  cooperate: a meta-agent can launch a ralph plan, watch it, and resume on
  failure.
- Inspect `kctx`-style codebase Q&A across local clones.

The meta-harness is itself just a `cockpit` agent (the agent file ships with
`cockpit`) plus a small set of built-in tools:

- `harness_invoke(name, prompt, agent_file?, model?)` — non-interactive
  call into another harness; returns stdout/stderr/exit.
- `ralph_*` family — list/show/run/resume/cancel plans.
- `cockpit_subagent(prompt, agent?)` — recursive `cockpit` invocation.

The intent is that **`cockpit meta` is the primary entry point for users
who do not have a single preferred harness** — it is the harness that
picks the harness.

---

## 7. Environment-variable redaction

Before any prompt — system, user, tool result, retry, anything — is sent
to a model provider, `cockpit` scans it for environment variable values and
substitutes them with a placeholder.

Scope:

- **OS environment** (`std::env::vars()`).
- **Project `.env`** files: `<cwd>/.env`, `<cwd>/.env.local`, plus any
  paths in `redact.extra_dotenv_paths`. Walks up to the git root.
- Future: other secret sources (1Password, op, vault) are out of scope
  for v1 but the pluggable design must allow adding them later.

Algorithm (v1):

1. On startup, build a redaction table: `value -> name` for every env var
   whose value is at least `min_secret_length` characters long and is
   not in a small allowlist (e.g. `PATH`, `HOME`, `SHELL`, `TERM`, `LANG`).
2. Before each provider request, replace every occurrence of `value` in
   the request body with `***redacted-by-cockpit***` (configurable
   placeholder).
3. Replacement is case-sensitive and substring-aware (so a token embedded
   in a longer URL is still redacted).
4. The redaction table is rebuilt when `.env` files change on disk
   (debounced).
5. Redaction failures (e.g. unreadable `.env`) emit a TUI warning **and
   block the request** by default — this is a security feature, not a
   convenience. Users can opt out via `redact.enabled = false`.

The placeholder is intentionally distinctive (`***redacted-by-cockpit***`)
so leaks into provider logs are easy to grep for.

---

## 8. Daemon architecture and remote control

cockpit runs as a **long-lived daemon process** that owns the
session DB, the file-lock manager, the ralph executor, the
provider clients, the redaction layer, and the config-hierarchy
resolver (§2). The TUI is a *client* of the daemon — not the
process that does the work. This is
true in v1 (locally, over a Unix socket) and forward-compatible
with `cockpit connect` later (remotely, over a WebSocket relay).

### 8a. Why a daemon in v1

The agent/subagent design in §3 only earns its keep if
long-running work outlasts any single terminal window:

- The ralph executor (background plan executions, §3b) needs to
  keep running across `cockpit` invocations and across the user
  closing their terminal.
- The file-lock manager must be a single in-process authority
  across all parallel subagents; running it inside whichever
  TUI happens to be open is fragile.
- The session DB and ongoing inference calls need crash recovery
  to survive a closed terminal without losing in-flight work.

Making the daemon part of v1 also lets `cockpit connect` (§8d)
layer cleanly on top — the daemon's wire protocol is the same;
only the transport changes.

### 8b. Lifecycle and the "first invocation becomes the daemon" UX

The first `cockpit` invocation on a machine **auto-promotes to a
detached background daemon** and the foreground terminal
becomes a TUI client attached to it. Subsequent invocations
detect the running daemon and attach as additional clients.

- Promotion uses `setsid` + a double-fork on Unix (similar
  pattern on Windows via `DETACHED_PROCESS`) so the daemon
  outlives the original terminal.
- The TUI client shows a **one-time toast** the first time it
  promotes a daemon, of the form:

  > `cockpit` daemon started (pid 12345). Closing this terminal
  > will not stop the daemon. Manage with `cockpit daemon
  > {status,stop}`.

  Dismissible; never shown again on that machine (recorded in
  `~/.local/state/cockpit/state.json`). This is what keeps
  users from being confused by a background process they didn't
  knowingly start.
- Daemon lifecycle commands: `cockpit daemon start|stop|status|restart`.
- **Auto-start is not a system service by default.** Install
  scripts may *suggest* installing a `systemd --user` unit or a
  launchd plist for users who want the daemon running across
  reboots, but the default is "starts when you first run
  `cockpit`, stops when you tell it to." This matches the
  user's expectation: not everyone wants a background process
  living through restarts.
- If the daemon crashes, ongoing model calls die but the
  session DB and file-lock manager state are durable
  (`rusqlite`-backed). The next `cockpit` invocation starts a
  fresh daemon that reconnects to in-progress sessions and
  resumes plan executions from the last completed node.

**Ephemeral run daemons.** `cockpit run` (and `cockpit run
--ephemeral`) do **not** promote a persistent daemon. They spawn an
*ephemeral* daemon scoped to the single run, on a **unique per-pid
path** — `cockpit-eph-<pid>.sock` + `cockpit-eph-<pid>.pid` in the same
directory as the canonical socket/pid. Because the ephemeral daemon
never touches the canonical path, it coexists with a persistent daemon,
and `cockpit daemon {stop,status}` (canonical-only) never sees it. The
TUI's `--ephemeral`-free default still auto-promotes a *persistent*
daemon at the canonical path. Three independent layers guarantee an
ephemeral daemon never outlives its run:

  - **A — foreground guard.** The `run` process holds an RAII guard
    (tied to `owns_daemon`) that sends `StopDaemon` to the daemon it
    spawned on *every* exit path: normal completion, early `?` error,
    panic/unwind, and SIGINT/SIGTERM. A run that attached to a
    pre-existing persistent daemon owns no guard and shuts nothing down.
  - **B — path isolation.** The unique per-pid socket/pid scheme above.
  - **C — self-reaping watchdog.** The ephemeral daemon exits on its own
    when it has had no connected client for a ~30s idle grace
    (`EPHEMERAL_IDLE_GRACE`); a reconnect inside the window cancels the
    countdown. This catches uncatchable foreground deaths (SIGKILL,
    power loss) that Layer A cannot. The persistent daemon never arms
    this watchdog.

### 8c. IPC: same wire protocol, different transports

cockpit defines **one** message schema for client↔daemon
communication. The schema is transport-agnostic; the choice of
transport is per-link:

| Link                          | v1 transport      | Notes |
|-------------------------------|-------------------|-------|
| Local TUI ↔ local daemon       | **Unix socket** at `~/.local/state/cockpit/daemon.sock` (Windows: named pipe at `\\.\pipe\cockpit-<user>`) | Filesystem perms gate access. Simple, no port management. |
| Local daemon ↔ relay (later)   | **WebSocket** outbound to a hosted relay | Daemon initiates; relay does not initiate to the daemon. |
| Relay ↔ remote browser/mobile  | **WebSocket** | Same schema as the daemon↔relay leg. |

The wire schema (event types, request/response envelopes) is
the contract; transports are just framing. This is the
explicit promise that lets `cockpit connect` ship later without
a protocol rewrite.

### 8d. `cockpit connect` — remote control (v2)

(Planned for a later milestone, scoped here so v1 doesn't paint
us into a corner.)

Users will be able to pay a monthly subscription to control their
`cockpit` instance from anywhere — typically a phone — by:

1. Running `cockpit connect` on the device that hosts the daemon.
2. The daemon opens an outbound WebSocket to a hosted relay
   (operated by us).
3. The user's phone or browser connects to the same relay
   (auth'd by their account) and gets a thin web UI that mirrors
   the TUI and adds a **plans dashboard** (in-flight plan runs,
   needs-attention queue from §3b, per-plan progress).

Implications already captured in v1 architecture:

- The session/event log is already decoupled from the TUI (it
  lives in the daemon).
- All secret material (API keys, env vars) stays on the device.
  The relay sees only the redacted event stream (per §7) plus
  user input.
- The TUI and the future web client are peers of the same
  daemon; the daemon doesn't know or care which type of client
  is attached.

### 8e. Multiple TUI clients (deferred)

The architecture allows multiple TUI clients to attach to one
daemon (e.g., one per terminal window, viewing different
sessions). v1 ships single-client; the daemon's session model
is designed to support multi-client later without protocol
changes.

---

## 9. Cross-platform (incl. Windows)

`cockpit` must work on Linux, macOS, and Windows. Windows in particular has
caveats around shell semantics (no POSIX `sh`), path handling, and
process-group signals. See `miscellaneous.md` for the bundled-gitbash
discussion and the full Windows compatibility plan.

---

## 10. Token economy — keep cockpit's overhead off the model

Every byte `cockpit` puts into the model's context is a byte the model
can't use to reason. We treat the LLM context window as a **scarce
shared resource** and aggressively minimize cockpit's footprint in it.

This is not a soft preference; it is a load-bearing design constraint
that touches every subsystem.

### Concrete commitments

- **Tool descriptions are terse.** A tool's `description` field is one
  sentence. Parameter `description` fields are short noun-phrases, not
  paragraphs. No examples, no rationale, no "use this when…" prose —
  the model is smart enough to figure it out from the name + one line.
  If a tool genuinely needs a long explanation, that's a sign the tool
  should be split or renamed.
- **System prompt is minimal.** The base system prompt is under
  ~400 tokens. AGENTS.md and skills are layered on top *only when
  applicable* (skills lazy-load, AGENTS.md only loads if the file
  resolves).
- **Cached system block stays cache-stable across sends.** Stable
  per-session metadata (OS + version, session ID, per §17g) goes
  inside the cached system prompt and counts against the
  ~400-token budget above. Volatile per-message context (current
  time) rides on user messages with an interval-based suppression
  (default 5 min), so the provider's prompt cache is not
  invalidated every turn. Full rule in §17g.
- **Skills are lazy.** Discovery returns `(name, one-line description)`
  pairs only — never the full skill body. The model invokes
  `skill <name>` to load the body on demand. This is opencode's design
  too; we keep it and never regress to "load all skills' frontmatter
  into the system prompt."
- **AGENTS.md walk-up is one file, not a chain.** We load the **first**
  matching guidance file (per `agent_guidance_files` in `config.json`) and
  stop. We do not concatenate AGENTS.md from every ancestor directory.
- **Tool results are bounded.** Every tool result clips at a
  configurable limit (default ~8 KB), always on a UTF-8 char
  boundary. Paginated tools (`read`, and `@`-tag inlining which
  shares its formatter) clip with a `... [truncated, ask read with
  offset N to see more]` trailer that tells the model how to page
  through. Non-paginated tools (`bash`, custom shell tools like
  `webfetch`) clip **head + tail** with a `... [truncated N bytes]
  ...` marker in the middle, so the failure signal — which usually
  surfaces at the tail (stderr, a non-zero `exit:` line, a panic) —
  is never lost to head-only truncation.
- **Two complementary context-reduction commands.** `/prune` is
  deterministic, mechanical, and reviewable. Current shipped scope is
  **snapshot dedup only**: it collapses all-but-the-most-recent result
  body for snapshot-class calls of exact identity (same canonical path
  + identical args) for `read` and the read-only intel tools
  (`outline` / `symbol_find` / `word` / `deps` / `circular` / `tree` /
  `search`) into a `Part::Elided` marker, with no LLM in the loop.
  Elision is wire-only (§14): the on-disk transcript and TUI scrollback
  stay full-fidelity; only the model-bound message list shrinks. Older
  read-only `bash` snapshot dedup, bash-result truncation, and the
  interactive picker are deferred (see `plan.md` T6.d). `/compact` is
  the heavyweight option: it asks the model
  to draft a handoff prompt summarizing the work so far, then starts a
  fresh thread seeded with that prompt (the old thread is preserved on
  disk and recoverable). Automatic background staleness/dedup (T6.a/b
  in `plan.md`) runs continuously; the slash commands are the
  user-facing escape hatches when the budget gets tight. opencode's
  inline summarization-style compaction is **replaced** by the
  fresh-thread handoff model — it avoids compaction sediment (no
  summarizing summaries) and is friendlier to provider caches (the new
  thread starts with a clean cache rather than mutating the old one).
- **Auto-prune is cache-aware.** The auto-prune predicate fires
  whenever the expected cache-hit on the next call is zero. Three
  cases unified under one rule: (a) provider with no cache support
  (`provider.cache.mode = "none"`); (b) cache TTL has elapsed since
  the last send (default 5 min, overridable per-provider and
  per-model); (c) the next call already busts the cache upstream
  (e.g., a tool-result edit before the cache breakpoint). When the
  predicate evaluates true, `/prune` runs before the inference call
  with no user prompt — cache cost is already zero, so the savings
  are pure. The per-session "last prune watermark" short-circuits the
  walk when nothing new is prunable. Threshold-based auto-prune
  (`prune.auto_threshold` in `plan.md` T6.f) remains in force as a
  secondary trigger.
- **`/compact` always prunes first.** Pruning is lossless and cheap;
  pruning before compaction means the compaction summarizer sees a
  smaller, denser input and produces a tighter handoff. The order is
  fixed in the engine — there is no `--no-prune` flag.
- **Seed-tool handoff** (`plan.md` T6.e, §3d). When the parent
  invokes a subagent (`task`) or fires `/compact`, it may attach a
  list of `seed_tools: [{name, args}, ...]` that are dispatched
  **before** the new conversation starts; the results land in the new
  agent's initial context as if it had just run them. Restricted to
  **read-only, idempotent** tools (`read` and the read-only
  codebase-intelligence tools — `tree`, `outline`, `symbol_find`,
  `word`, `deps`, `hot`, `circular`, `search`; §21). No `bash`, no
  `write`, no `edit`. Re-execute, never
  replay cached output — the seed exists to save the new agent a
  round-trip, not to hand off stale snapshots. The TUI surfaces
  seed-tool token cost on the receiving agent's first turn so an
  over-eager parent is debuggable.
- **Built-in tool surface is small.** v1 ships `read, readlock, write,
  writeunlock, edit, bash, task, skill, webfetch, mcp_invoke`, the
  codebase-intelligence tools (`tree, outline, symbol_find, word, deps,
  hot, circular, search`; §21), the `jobs` meta-tool (§22), and the
  sandboxed `grep`/`glob` tools (`docs`-answerer-only). The
  lock-aware tool set (`readlock` / `write` / `writeunlock`) is
  required for the multi-agent file-locking model (see `plan.md`
  §4.1); plain `read` is the unlocked snapshot variant for exploration
  that doesn't intend to modify. **No `grep`/`glob` tool for general
  agents** — raw search is `bash` + `rg`/`fd`; the budgeted/structured
  path is the `search` intel tool, which post-processes into
  token-capped results (a raw `bash rg` dump has no budget awareness).
  `grep`/`glob` exist *only* on the `docs` answerer (Docs.2, §3a),
  which is denied `bash`: they are Rust-native (ripgrep libraries +
  `globset`, never shelling to `rg`/`fd`) and hard-confine every path
  to the answerer's package-root cwd, so Docs.2 can explore an
  untrusted cloned dependency without shell access. No `websearch`
  on general agents (provider-side search exists; if a user wants
  `cockpit`-side, they pipe `curl` through `bash`). No tool we
  couldn't justify removing.
- **MCP via lazy discovery** (§18 — reversed from the prior "no MCP"
  policy). The original §10 objection to MCP was token cost: a typical
  MCP server's per-tool schemas inject thousands of tokens into every
  system prompt. The lazy-discovery design (catalog of `(server.tool,
  one-line description)` only; schemas load on `mcp_invoke`)
  neutralizes that objection by construction — the §10 budget holds.
  See §18 for the full design.
- **Subagent reports, not transcripts.** When a `task` finishes in
  subagent mode, the parent receives the subagent's **final reply
  only**, not the subagent's full conversation — and the subagent
  itself only ever saw the task brief, not the parent's history.
  Two layers of context-economy in one primitive. Fork mode is a
  different trade-off (see §4c): the branch inherits parent context
  on purpose so that "explore an alternative" preserves the setup
  cost; pick fork when the *shared history* is the value.
- **Subagent reports are token-capped.** Default report budget is
  **≈2K tokens**; the caller may override on the `task` invocation
  up to a **hard ceiling of ≈10K tokens**. The cap is enforced
  deterministically at report-finalization time, not just suggested
  to the subagent in the system prompt — over-budget reports are
  truncated and a footer is appended:
  `[... ≈N tokens elided; re-invoke with report_budget=X to see more ...]`
  so the parent knows truncation happened and how to ask for more.
  Token counts are approximate (the truncation contract is "≈",
  not "="); cockpit uses a default tokenizer (cl100k_base via
  `tiktoken-rs`) as the budget enforcer when the active provider
  doesn't expose its own counter, and prefers the provider's
  counter when available. This matters most at fan-out: five
  parallel `explore` subagents reporting back to `orchestrator-plan`
  with 2K each is 10K of citations to weigh; with no cap it could be
  50K. Across deep delegation trees the savings compound.
- **Redaction placeholder is short.** `***redacted-by-cockpit***`
  is 30 chars; we deliberately don't include the var name. Naming
  `OPENAI_API_KEY` in 47 places across a transcript is a leak vector
  (it telegraphs which providers the user has configured) and a token
  cost.

### Operational hygiene

- A nightly `cargo run --bin context-budget` (or a CI check) prints the
  exact token count of the base system prompt and every tool
  description, so regressions are visible in PRs.
- The `cockpit debug context` command (cockpit-specific addition; mirror
  `cockpit debug redact`) dumps the complete prompt that *would* be sent
  for the next turn, with token counts, so users can audit what cockpit
  is spending their budget on.

### Why this matters

A 200K-token model that loses 30K to bloated tool descriptions and
redundant guidance files is effectively a 170K-token model. Long
sessions, big repos, and any "let me read the whole file" pattern all
trade against that overhead. cockpit's competitive edge over opencode (and
its own future ambitions like `cockpit meta`, where multiple harnesses
chain together) depends on **not being the thing that ate the user's
context budget**.

---

## 11. Naming and the `cock` shortcut

- **Binary:** `cockpit`. Crate: `cockpit-cli` on crates.io
  (the crate keeps the `-cli` suffix because the unsuffixed name
  may already be claimed by an unrelated project; the binary
  installed by the crate is just `cockpit`).
- **Optional shortcut `cock`** installed via opt-in prompt at
  install time (or on first run for `cargo install` users) —
  see `miscellaneous.md` §3a. The shim sets `COCKPIT_ROOSTER=1`
  and execs `cockpit`; the binary detects the env var on launch
  and renders an ASCII-rooster splash. Pure easter egg; `cock`
  is identical to `cockpit` in every other respect. Lifecycle
  managed by `cockpit shortcut {install,remove,status}`.
- **Naming-conflict note:** the cockpit-project.org server
  admin UI also ships a `cockpit` binary on some Linux distros.
  `miscellaneous.md` §9a covers the mitigations (PATH
  precedence, the `cock` shortcut as a guaranteed-unique alias,
  one-time launch warning if a conflicting binary is detected
  ahead of ours on PATH).

---

## 12. Tool-input repair — make open-source models first-class

A strict tool-call schema filters out a lot of recoverable noise.
Large commercial models eat that cost invisibly because they've seen
enough of every JSON contract during pretraining; open-weights models
pay it loudly. The failure modes, across DeepSeek, GLM, Qwen, and
similar, are not random — they're a small finite compositional set of
*shape* mistakes ("sent `null` for an optional field", "emitted an
array as a JSON string", "passed a bare string where the schema wants
an array"), not capability gaps.

`cockpit` ships a tool-input repair layer between rig's tool-call JSON
and the typed dispatcher. This pairs directly with §10 (token
economy): a repaired call costs one extra validate pass; a
non-repaired call costs a full retry round-trip (re-inference, re-
streamed assistant turn, re-emitted tool args).

### Concrete commitments

- **Validate first, repair on failure — never preprocess.** Inputs
  that validate as-is are dispatched unchanged. A preprocessing pass
  that rewrites inputs *before* the schema sees them is a known
  silent-corruption hazard (`write` content that happens to be
  JSON-shaped getting "fixed" before it hits disk). When validation
  fails, the layer walks the validator's issue list and tries the
  catalogued repairs at the specific paths the schema disagreed at;
  on success it re-validates and dispatches. The schema localizes the
  bug for us; we only spend repair budget where it actually
  disagreed.
- **Catalog (v1), in this order.** `null`-for-optional → omit the
  field; stringified JSON array (`'["a","b"]'`) → parse to array;
  single-arg `{…}` where the schema wants an array → wrap in array;
  bare string where the schema wants an array → wrap in `[string]`.
  Order matters: parse-stringified-array must run before
  wrap-bare-string, or `'["a","b"]'` becomes `['["a","b"]']`. The
  catalog is small on purpose — every new repair must justify its
  presence against a logged failure mode (see observability below).
- **Schema hints, not raw `String`, for shape-prone fields.** Path
  parameters use a `PathString` newtype (or a `#[cockpit(path)]`
  marker on the parameter struct field) rather than `String`. The
  dispatcher unwraps degenerate markdown-link paths
  (`"/x/[notes.md](http://notes.md)"` → `"/x/notes.md"`) at the
  `PathString` boundary, leaving real markdown links
  (`[click](https://example.com)`) untouched. The hint centralizes
  the fix: every path field across every tool inherits it; no other
  field is affected. The same pattern is available for any field
  where the model's post-training distribution leaks through (URLs,
  shell commands).
- **Relational invariants extend the tool's semantics, not the
  repair catalog.** Repairs handle *shape* problems — wrong type,
  missing key, wrong container. *Relational* problems (e.g. `read`
  requires `offset` and `limit` together, or neither) are handled by
  filling in the missing field with a sane default and prepending a
  one-line note to the tool result so the model can self-correct on
  the next turn: "Note: `limit` defaulted to 2000; pass both
  `offset` and `limit` to override." No `Error:` prefix — the TUI
  doesn't paint the result red, and the model sees what we picked.
  Transparency over silent magic.
- **Retry message on hard failure is model-readable.** When all
  repairs fail, the layer returns a short message that names the
  offending field and the expected shape — not a raw validator
  issues blob. The model can act on "field `paths` expected array of
  string, got string"; it cannot act on a 600-token zod-style dump.
- **Repaired calls flow through the §14 wire/user split.** A
  repaired tool input is written to `wire_input` on the assistant
  turn's session-DB row; the model's original emission is preserved
  in `original_input`; `recovery` is set to
  `{kind: "shape_repair", repair: <repair-name>, path: <json-path>}`.
  The model's attention pass sees the clean, schema-valid form; the
  user transcript shows the original with a `⟲ repaired` chip. No
  in-prose `Note:` is emitted on success — the model learns from a
  self-consistent transcript, the user sees what the model actually
  did. Relational defaults are the one exception: they *do* prepend
  the `Note:` line described above, because the default is a
  semantic choice cockpit made, not a syntactic fix to a
  schema-invalid input. The user transcript marks those rows with a
  `relational_default` recovery kind.

### Observability — repairs must be investigatable by agents

Improving the repair catalog as we onboard new models depends on
seeing exactly where calls break and which model broke them. This is
non-optional: a tool-input repair feature without per-(model, tool,
kind) telemetry rots silently.

- **Structured `tracing` events.** Every repair attempt emits a
  record with `tool`, `model`, `kind` (one of the catalog names, or
  `relational_default`, or `markdown_link_unwrap`), `outcome`
  (`repaired` | `invalid`), and a redacted excerpt of the offending
  input. Same log destination as everything else
  (`~/.local/state/cockpit/logs/`, per `miscellaneous.md` §5). The
  redaction layer (§7) runs over the excerpt before it hits disk —
  repair telemetry never carries secrets.
- **`cockpit debug repair`.** Text summary of repair rates per
  `(model, tool, kind)` over the last N days plus the top failure
  modes that fell *through* the catalog (the ones we couldn't fix).
  Used by users to spot a model regressing on a contract before
  users do, and by agents asked "where are our tool corrections
  falling down?".
- **`cockpit debug repair --raw`.** JSONL stream of individual
  repair events with input-before / input-after / outcome, designed
  to be piped into a follow-up agent invocation (`cockpit run`) that
  proposes new repairs or new schema hints. This is the loop that
  lets the catalog grow against evidence.

Repair telemetry never leaves the device. Both commands read from
the rotating log files only.

### Why this matters

A lot of what looks like model capability is actually contract
design. Without this layer, swapping from a frontier model to a
strong open-weights model regresses tool-calling reliability
sharply, for reasons that have nothing to do with the model's actual
ability. The harness is where you mediate between provider
distributions — that's a cockpit responsibility, not the user's
problem.

---

## 13. File I/O tool semantics — `read`, `edit`, `write`

The read/edit/write trio is the model's primary contact with the
user's files. The design has to balance three competing concerns:
token cost (a `read` of a 4 KLOC file blows the context budget),
edit success rate on weaker models (exact-string match fails on
trailing whitespace), and "do not silently corrupt the file" (a
write that thinks it patched line 47 but actually patched line 49).

The §10 token-economy rules and the §12 repair layer set the
context; this section spells out the per-tool semantics.

### 13a. `read` — paginated, line-numbered, capped

- **Always paginated.** `read(path, offset, limit)` with `offset`
  1-indexed and `limit` in lines. There is no "read the whole file"
  call. The §12 relational-default rule fills missing fields
  (defaulting `limit` to 2000 lines, `offset` to 1) and prepends a
  `Note:` line to the result so the model sees what was filled in.
- **Output is line-numbered:** each line is emitted as
  `${line_number}: ${line_text}`. The line-number tax is ~3–5% of
  output bytes and pays for itself by giving the model durable
  citations (`file.rs:120`) and a frame of reference for follow-up
  reads. Line numbers are for *citation*, not for use as a write
  address (see §13d).
- **Two caps, whichever is smaller:** 2000 lines or ~8 KB. Hitting
  either truncates with the marker `... [truncated, ask read with
  offset N to see more]`, where `N` is the next offset the model
  should pass. This is the same chokepoint and the same marker that
  composer `@`-tagging (§1e) uses — exactly one read path through the
  codebase.
- **Same redaction.** Output runs through §7 redaction before it
  hits the model context, with no per-call bypass.
- **`read` is the unlocked snapshot variant.** `readlock` (§10's
  tool surface, `plan.md` §4.1) is the locking variant for work that
  intends to modify. They share the formatting, caps, and redaction
  rules — locking is the only difference.
- **Binary files are refused, not silently mangled.** Same
  heuristic as composer `@`-tagging (§1e): NUL bytes in the first
  1 KB, or an extension on the binary blacklist. The error names
  the heuristic that fired so the model can choose to invoke `bash`
  with `head -c` or `file` if it really wants a binary inspection.

### 13b. `edit` — search/replace with a fuzzy fallback cascade

Edits are addressed by **content anchors** (`old_string` →
`new_string`), not by line number. Why content anchors are the only
addressing mode is in §13d.

When the exact `old_string` doesn't match, the tool runs an
eight-stage cascade, falling through on each failure:

1. **Exact match.** The cheapest stage; the only stage that
   succeeds for well-behaved frontier models most of the time.
2. **Line-trim match.** Trim trailing whitespace per line on both
   sides.
3. **Block-anchor match.** Use the first and last lines of
   `old_string` as anchors, find candidate regions in the file, and
   pick the one with the smallest Levenshtein distance against the
   interior.
4. **Whitespace-normalized match.** Collapse all whitespace runs to
   single spaces.
5. **Indent-flexible match.** Strip common leading indentation
   from both sides before comparing.
6. **Escape-normalized match.** Reconcile `\n` / `\t` / `\"`
   mismatches (the model emitted a literal escape; the file had the
   character).
7. **Trimmed-boundary match.** Trim outer whitespace of the whole
   block.
8. **Context-aware match.** First and last lines match exactly;
   interior matches ≥50% by character content.

If a stage matches **multiple** regions and `replace_all` is not
set, the tool errors with `Found multiple matches; pass more
surrounding context or set replace_all: true.` — the same loud
failure mode opencode uses. Ambiguous edits never silently pick
the first hit.

If **no** stage matches, the tool errors with a near-miss
diagnostic (see §13c).

This design is cribbed from opencode; the cockpit-specific addition
is in §13c.

### 13c. Edit corrections — rewrite the model's tool call into its canonical form

When the cascade succeeds at any stage past stage 1, cockpit
populates the row's `wire_input.old_string` with the canonical
bytes the cascade matched (see §14 for the wire/user transcript
split). `original_input` keeps the model's actual emission;
`recovery` is set to `{kind: "edit_cascade", stage: <name>, path:
"old_string"}`. The next inference call carries `wire_input`, so
the model's attention pass over its own prior outputs sees the
form that *would have* matched at stage 1 — and the tool result
returned is a clean success, identical to what a stage-1 exact
match would have produced. No `Note:` line, no correction prose,
no token tax. The user transcript renders `original_input` with a
recovery chip; the model never sees the chip.

The rewrite is deterministic and content-equivalent: every stage
of the cascade is a semantic-equivalence match (whitespace,
indentation, escape forms, anchor-bounded interior content), so
substituting the canonical form for the submitted form does not
change the edit's effect. The bytes replaced and the bytes written
are unchanged; only the *address* the model nominally used is
normalized.

Scope of the rewrite:

- **`old_string` is rewritten** in `wire_input` to the exact bytes
  from the file that the cascade matched.
- **`new_string` is not touched.** It's the model's intent and we
  respect it verbatim.
- **The assistant's reasoning text is not touched** in either
  projection. If the model said "I'll trim the whitespace and patch
  this block," the reasoning stays even though `wire_input.old_string`
  is now the canonical un-trimmed form. Mild incongruence between
  reasoning and args is acceptable; rewriting prose is a much
  bigger intervention than rewriting structured tool args.

Cache implications:

- Rewriting `wire_input` changes the bytes sent on the next
  inference call, which invalidates the provider's prompt cache
  from that point forward. To bound the cost, cockpit places cache
  breakpoints **after each tool result** rather than at session
  start. A cascade rewrite then invalidates at most the in-flight
  turn's worth of cache (≈1 turn of prompt), not the entire prior
  session. For sessions with frequent cascade hits this is a
  measurable cost; for frontier models that rarely hit the cascade
  it's noise.

**On total miss (no stage matched).** There is no canonical form to
rewrite to, so this case falls back to a model-readable error: the
closest near-miss in the file plus a diff against the submitted
`old_string`. The error message is the only path where the model
sees a correction in prose — and it's an error message, where the
model already knows it has to act.

```
Error: no match for `old_string` in <path>.
Closest near-miss (lines 47-53):
<near-miss bytes, fenced>

Difference from your `old_string`:
- you submitted: 2-space indent, no trailing newline
- file actually: tab indent, trailing newline present
```

**Escalation for repeat offenders.** Pure-rewrite teaches the model
silently by example, which works when the model can pattern-match
its own prior outputs. For models that don't generalize from their
own corrected outputs (typically smaller open-weights models with
shallow in-context learning), cockpit pins an explicit
system-reminder after the same `(model, stage)` rewrite fires N
times in a session (default N=3): `this model has been
under-indenting Python edits by 4 spaces; check indentation before
submitting old_string`. Pinned reminders expire after K turns
without recurrence (default K=10). The rewrite is the primary
mechanism; the reminder is the v2-grade fallback for models the
rewrite alone doesn't reach.

This is genuinely cockpit-original; nothing in the surveyed
harnesses does it. opencode runs the cascade silently *and* leaves
the malformed `old_string` in the transcript — the next imperfect
edit gets nothing from the prior one.

### 13d. `write` — full file only, content-anchored, no line addressing

- **`write` overwrites the entire file.** It is the right tool for
  new files and for total rewrites. For partial changes, use
  `edit`. Two tools, two jobs.
- **`write` requires a prior `read`.** Mirroring Claude Code's
  invariant: the model must have `read` (or `readlock`'d) the file
  in this session before `write` will accept a payload for that
  path. The check exists to prevent the "rewrote a file it never
  looked at" failure mode. Lock-aware variants (`writeunlock`)
  carry the same prerequisite via the lock-acquisition handshake.
- **Line endings preserved.** Per `miscellaneous.md` §1g: a CRLF
  file round-trips as CRLF; an LF file round-trips as LF. The
  heuristic looks at the first 1 KB before overwriting.
- **No line-range write tool.** cockpit does not offer "replace
  lines N–M with this content." Line numbers go stale the moment
  another tool runs (an intervening `edit` shifts line numbers; a
  read of a different range doesn't, but the model has no reliable
  way to know which is which). Open-weights models routinely
  miscount line ranges by 1–2, and the failure is *silent
  corruption of an adjacent function* rather than a loud no-match.
  Content anchors fail loudly; line numbers fail quietly. We pick
  loud.

### 13e. No `apply_patch` / unified-diff tool

opencode ships `apply_patch` (unified-diff format) alongside its
`edit` tool; codex's primary write path is OpenAI's `*** Update
File:` patch format. cockpit ships neither.

- It's a **second way to do the job `edit` already does.** Two
  write paths means two failure surfaces, two locking dances, two
  schemas to repair against (§12).
- Unified diff requires the model to keep accurate context-line
  counts and `@@ line @@` ranges. Weaker open-weights models
  consistently miscount, and the §13b cascade gives us cheaper
  recovery from the same class of mistake.
- A multi-hunk edit can be expressed as repeated `edit` calls. The
  total token cost is comparable; the transcript stays legible;
  per-call locking semantics stay simple.

If a real workflow emerges where this is wrong — e.g. an LSP-driven
multi-file refactor that fits a clean unified diff but not a
sequence of search/replaces — we'll revisit. Until then, one write
path.

---

## 14. Wire transcript vs user transcript — two projections over one session log

§12 (shape repair) and §13c (edit-cascade rewrite) both fix model
mistakes deterministically and dispatch the corrected call. The
question is: when the model emits a malformed `tool_use` and the
harness repairs it, **what does each audience see afterwards?**

cockpit's answer is: one session DB, two projections.

- **Wire transcript** — what crosses the network on every
  subsequent inference call. Always carries the **canonical /
  repaired form** of every tool input. The model attends only to
  this projection; its own prior outputs, when it looks back at
  them, are well-formed by construction.
- **User transcript** — what the TUI renders, what
  `cockpit transcript view` shows, what gets persisted to
  on-device scrollback and exported on `cockpit session export`.
  Always carries the **original input** the model actually emitted,
  plus a structured `recovery` annotation describing what the
  harness fixed.

Both projections are derived from the same row in the session DB.
There is no fork, no double-write, no drift risk — just two render
paths over a row that holds both forms.

### 14a. Session DB row shape

Every tool invocation is one row in the `tool_call_events` table
(see §15b for the full schema). The fields that drive the
wire/user split:

- `original_input_json` — exact bytes the model emitted. Immutable
  after the turn lands.
- `wire_input_json` — what the next inference call carries. Equal
  to `original_input_json` for clean calls; differs when §12 or
  §13c fired.
- `recovery_kind` / `recovery_stage` — `NULL` for clean calls;
  otherwise the structured annotation:
  - `(shape_repair, wrap_bare_string)` (§12)
  - `(edit_cascade, whitespace_normalized)` (§13c)
  - `(relational_default, limit)` (§12; not counted as malformed,
    see §15h)
- `hard_fail` — `1` if all repair stages failed and the model
  received an error result.

The §12 repair telemetry stream and the §15 `/stats` pane both
read from these columns. There is one source of truth; the two
audiences (developer-grade `cockpit debug repair` and user-grade
`/stats`) are different projections over the same rows.

### 14b. What each audience sees

- **The model** sees `wire_input` only. From its perspective, every
  prior tool call it emitted in this session was syntactically
  perfect and produced a clean success. Future calls inherit
  well-formed examples from the model's own outputs.
- **The user** sees `original_input` with a visual marker when
  `recovery != null` — e.g., a small `⟲ recovered (whitespace-norm)`
  chip on the tool-call row in the TUI, click-to-expand for the
  before/after comparison.
- **`cockpit debug repair`** reads the same `recovery` field and
  rolls it up per `(model, kind)` (already specified in §12).

### 14c. Per-model performance surfaces naturally

Because every recovery is annotated on the row that triggered it,
the harness can compute per-model recovery rates with no extra
instrumentation:

```
$ cockpit debug models
Model                    Tool calls  Shape repairs  Edit cascades  Recovery%
claude-opus-4-7-1m              145              0              2       1.4%
gpt-5                            87              1              5       6.9%
qwen3-30b-coder                 132             12             47      44.7%
deepseek-v3                      94              3             21      25.5%
```

This is the user-visible signal the §13c rewrite enabled. The model
gets full utility from the repair layer; the user gets a
calibrated view of how much repair each model needed. Both
audiences are served by the same recovery annotation.

### 14d. Where this pattern applies

Today (v1):

- §12 shape repairs (`null`→omit, wrap-bare-string, parse-stringified-array, etc.)
- §12 relational defaults (e.g. `read` `offset`/`limit` pairing)
- §13c edit-cascade rewrites
- `PathString` markdown-link unwrap (§12)

Future extensions should follow the same pattern: when a
deterministic correction exists, store the canonical form as
`wire_input`, keep the original as `original_input`, annotate
`recovery`, and let the two projections render themselves.

### 14e. What does *not* go through the wire/user split

- **The model's natural-language reasoning** is never rewritten.
  Both projections show it verbatim.
- **The system prompt** and pinned reminders are not user-facing
  by default (already a kind of wire-only content, but distinct
  from the recovery flow — they aren't responses to a model
  mistake).
- **Hard failures** that the harness could not repair are surfaced
  identically to both audiences: the model sees an `Error:`
  result; the user sees the same `Error:` in the transcript.
  There's nothing to project differently.

### 14f. Why this design

Two reasons to keep them as projections rather than literal
separate logs:

1. **No drift.** A two-log world has a synchronization problem the
   instant a turn is edited, replayed, or compacted. A
   one-row-two-projections world cannot drift; `original_input` and
   `wire_input` are pinned to the same row's lifecycle.
2. **Compaction and replay stay simple.** §10's `/compact` (fresh-
   thread handoff) and `/prune` (mechanical staleness collapse) read
   the user transcript for what to summarize, and emit a new wire
   transcript for the fresh thread. Both operations remain pure
   transformations over one log, not a join across two.

---

## 15. `/stats` — on-device model and project performance

The §12 repair telemetry and §14 recovery annotations already
record everything needed to surface model performance to the user.
`/stats` is the user-facing pane that exposes it. Same data is
also available as `cockpit stats` for headless / scripted use.

All stats are local-only — they're a SQL query over the session DB
in `~/.local/share/cockpit/cockpit.db`. Nothing leaves the machine,
ever (this is a per-machine surface; cross-device aggregation is a
non-goal).

### 15a. What the pane shows

Three sections in one pane, each with a **scope** toggle (current
project / all projects on this machine) and a **range** toggle
(last 7 days / all time):

1. **Token spend** per model. Columns: model, input, output,
   cached-input, total, optional dollar cost. Cost is shown only
   when a price table is available (see §15d).
2. **Tool-call recovery** per model. Columns: total calls,
   malformed%, recovered%, hard-fail%. Press Enter on a row to
   expand into per-tool and per-`(kind, stage)` breakdowns
   (`edit_cascade/whitespace-norm`, `shape_repair/wrap-bare-string`,
   etc.). This is the per-model strength signal previewed in §14c.
3. **Language breakdown** of tool-call activity (§15c). A
   horizontal bar showing percentage of `read`/`edit`/`write`
   calls per language, plus a count column.

Each section is one screenful at most; the pane scrolls vertically
if the model list is long.

**Definitions.** `malformed = recovered + hard_fail`. `recovered`
= tool calls where `recovery != null` and the harness produced a
clean dispatch. `hard_fail` = tool calls where validation failed,
no repair stage succeeded, and the model received an error result.
Relational defaults (§12) are *not* counted as malformed — they
are choices, not corrections.

### 15b. Schema — one event log, one aggregate table, one derived view

All session data — sessions, turns, tool calls, inference calls —
lives in the single `cockpit.db` SQLite database in
`~/.local/share/cockpit/`. The stats pane reads from two new
tables and one view; audit queries cross-reference the existing
session and turn tables.

**1. `tool_call_events` — one row per tool invocation.** This is
the denormalized analytics-friendly event log. Every tool the
model calls (whether the call was malformed, recovered, or clean)
becomes one row here, with enough columns inline to answer "show
me per-(model, provider, tool, project, language) performance"
without joins.

```sql
CREATE TABLE tool_call_events (
  event_id            TEXT    PRIMARY KEY,
  session_id          TEXT    NOT NULL,
  call_id             TEXT    NOT NULL,            -- references inference_calls.call_id
  timestamp           INTEGER NOT NULL,            -- epoch seconds

  -- denormalized for fast group-bys (model/provider/project rarely change inside a call)
  model               TEXT    NOT NULL,
  provider            TEXT    NOT NULL,
  project_id          TEXT    NOT NULL,            -- hash of project root
  project_root        TEXT    NOT NULL,            -- displayed path

  tool                TEXT    NOT NULL,            -- read / readlock / edit / write / bash / ...
  path                TEXT,                        -- NULL for non-file tools
  language            TEXT,                        -- resolved from path at write time; NULL for non-file

  -- recovery telemetry (§14)
  recovery_kind       TEXT,                        -- NULL | edit_cascade | shape_repair | relational_default
  recovery_stage      TEXT,                        -- the specific stage/repair name; NULL when kind is NULL
  hard_fail           INTEGER NOT NULL DEFAULT 0,  -- 1 = no repair worked, model received an error

  -- audit (the same fields §14 describes as living on the tool-call row)
  original_input_json TEXT NOT NULL,               -- model's emission
  wire_input_json     TEXT NOT NULL,               -- canonical form sent to provider on next call

  duration_ms         INTEGER
);

CREATE INDEX idx_tce_project_ts ON tool_call_events (project_id, timestamp);
CREATE INDEX idx_tce_model_ts   ON tool_call_events (model, timestamp);
CREATE INDEX idx_tce_tool_ts    ON tool_call_events (tool, timestamp);
CREATE INDEX idx_tce_lang_ts    ON tool_call_events (language, timestamp);
```

**2. `inference_calls` — one row per LLM call.** Per-call token
counts and (optional) cost. One inference call can contain many
tool calls; the join is `tool_call_events.call_id =
inference_calls.call_id`. Tool-level aggregates are *not* stored
here — they're computed from `tool_call_events` directly so there
is one source of truth.

```sql
CREATE TABLE inference_calls (
  call_id              TEXT    PRIMARY KEY,
  session_id           TEXT    NOT NULL,
  project_id           TEXT    NOT NULL,
  project_root         TEXT    NOT NULL,
  model                TEXT    NOT NULL,
  provider             TEXT    NOT NULL,
  timestamp            INTEGER NOT NULL,
  input_tokens         INTEGER NOT NULL,
  output_tokens        INTEGER NOT NULL,
  cached_input_tokens  INTEGER NOT NULL DEFAULT 0,
  cost_usd_micros      INTEGER                     -- NULL unless price table available
);
CREATE INDEX idx_ic_project_ts ON inference_calls (project_id, timestamp);
CREATE INDEX idx_ic_model_ts   ON inference_calls (model, timestamp);
```

**3. `tool_call_stats` — derived view that surfaces `recoverable` and `severity`.**
Defined as a SQL VIEW so the rubric (§15h) can evolve without a
backfill. Queries against the view look identical to queries
against a denormalized table; the rubric mapping is just expressed
in SQL.

```sql
CREATE VIEW tool_call_stats AS
SELECT
  event_id, session_id, call_id, timestamp,
  model, provider, project_id, project_root,
  tool, path, language,
  recovery_kind, recovery_stage, hard_fail,

  -- recoverable: did the harness save the model from a malformed call?
  -- (relational_default doesn't count — it's a choice, not a save)
  CASE
    WHEN recovery_kind IS NOT NULL
     AND recovery_kind != 'relational_default'
     AND hard_fail = 0
    THEN 1 ELSE 0
  END AS recoverable,

  -- severity 0..1; see §15g for the rubric and rationale
  CASE
    WHEN hard_fail = 1                                  THEN 1.0
    WHEN recovery_kind IS NULL                          THEN 0.0
    WHEN recovery_kind = 'relational_default'           THEN 0.0
    WHEN recovery_kind = 'edit_cascade'
         AND recovery_stage = 'line_trim'               THEN 0.10
    WHEN recovery_kind = 'shape_repair'
         AND recovery_stage = 'null_for_optional'       THEN 0.20
    WHEN recovery_kind = 'edit_cascade'
         AND recovery_stage = 'whitespace_normalized'   THEN 0.30
    WHEN recovery_kind = 'shape_repair'
         AND recovery_stage = 'wrap_bare_string'        THEN 0.30
    WHEN recovery_kind = 'edit_cascade'
         AND recovery_stage = 'indent_flexible'         THEN 0.40
    WHEN recovery_kind = 'shape_repair'
         AND recovery_stage = 'parse_stringified_array' THEN 0.40
    WHEN recovery_kind = 'edit_cascade'
         AND recovery_stage = 'escape_normalized'       THEN 0.50
    WHEN recovery_kind = 'shape_repair'
         AND recovery_stage = 'wrap_single_arg'         THEN 0.50
    WHEN recovery_kind = 'edit_cascade'
         AND recovery_stage = 'block_anchor'            THEN 0.60
    WHEN recovery_kind = 'edit_cascade'
         AND recovery_stage = 'trimmed_boundary'        THEN 0.70
    WHEN recovery_kind = 'edit_cascade'
         AND recovery_stage = 'context_aware'           THEN 0.90
    ELSE 0.50                                            -- unknown stage; safe middle
  END AS severity
FROM tool_call_events;
```

**Why this split.**

- `tool_call_events` is the source of truth for everything tool-
  shaped: audit, recovery telemetry, language attribution,
  /stats. One row per tool call, indexed three ways, with the
  audit blobs colocated so `cockpit transcript view` is also a
  simple SELECT.
- `inference_calls` is per-LLM-call; token spend and cost don't
  factor down to individual tool calls (a single LLM call emits
  many tool calls, all sharing the same input-token bill).
- `tool_call_stats` is the view. `recoverable` and `severity` are
  computed from the underlying columns via CASE expressions, not
  stored. **The rubric can change without a backfill.** A user
  who upgrades cockpit and gets a new severity weighting sees the
  new weights applied to all historical rows on the next stats
  query.
- `language` *is* stored at write time, not derived in the view.
  Extension→language attribution is stable enough that we
  prefer the storage cost (a few bytes per row) over the
  query-time computation. If the v2 hyperpolyglot upgrade lands
  and reclassifies anything, `cockpit stats rebuild --languages`
  is a one-shot UPDATE.

**`project_id` is stable per project root.** Resolved at session
creation: `git rev-parse --show-toplevel` if the cwd is inside a
git repo, otherwise the realpath of the cwd. The displayed
`project_root` is the human-readable path; `project_id` is a
short hash so renames and symlink shifts don't fragment history.

### 15c. Language attribution

For every `tool_calls` row with a non-`NULL` `path`, attribute the
call to a language by file extension. Bucket non-file tools
(`bash`, `task`, `webfetch`, ...) as `shell` and report them in a
separate "non-file activity" row beneath the language bar.

**v1: static extension table.** A baked-in map of ~40 extensions
to languages (`rs` → Rust, `ts`/`tsx` → TypeScript, `py` → Python,
`go` → Go, `md` → Markdown, ...). Anything unmapped becomes
`Other`. The table lives in `src/stats/languages.rs` and is
trivially extensible.

**v2 (deferred): GitHub-Linguist-style heuristics** via the
[`hyperpolyglot`](https://github.com/monkslc/hyperpolyglot) crate
or equivalent. Linguist's value is disambiguating extensions like
`.h` (C / C++ / Objective-C), `.m` (Objective-C / MATLAB), `.t`
(Perl / Turing), and shebang sniffing for extensionless files. v1's
static table gets ~95% of the value without that complexity; we
add hyperpolyglot only if real users hit the ambiguous-extension
edge cases. The attribution is computed at query time, not at
write time, so swapping the v1 table for v2 heuristics doesn't
require a backfill.

The bar uses one row per language, sorted descending by call
count, with the top 8 shown and a rolled-up `Other` row for the
tail. GitHub Linguist's project-composition bar is the visual
reference; the difference is we're showing *tool-call activity*
distribution, not bytes-on-disk distribution.

### 15d. Cost computation

Token counts are accurate from the provider; converting them to
dollar cost needs a per-(model, input-vs-output-vs-cached) price
table.

- **v1:** no built-in price table; the pane shows tokens only.
  Users who care about dollars can supply
  `~/.cockpit/prices.json` (schema: `{model: {input_per_mtok,
  output_per_mtok, cached_input_per_mtok}}`). When the file exists
  and a row for the current model is present, the pane fills the
  `Cost` column; otherwise the column reads `—`.
- **v1.5:** ship a curated `prices.json` with the binary,
  refreshed each release; users' `prices.json` overrides.
- **Out of scope:** automatic price-table fetching from a remote
  source. Pricing is data, not behavior; pulling it at runtime
  would be one more network dependency and one more attack
  surface. Users update on release cadence or by editing the file.

### 15e. `/stats` UI sketch

```
┌─ /stats ─ project: cockpit-cli   scope: [project ▼]  range: [7d ▼] ─┐
│                                                                     │
│  Token spend                                                        │
│    Model                  In       Out    Cached    Total    Cost   │
│    claude-opus-4-7-1m    12.3K    4.1K    45.2K    61.6K    $0.92   │
│    gpt-5                  3.1K    1.4K        0     4.5K    $0.05   │
│    qwen3-30b-coder       18.7K    5.9K        0    24.6K       —    │
│                                                                     │
│  Tool-call recovery                                                 │
│    Model                  Calls  Malformed%  Recovered%  Hard-fail% │
│    claude-opus-4-7-1m       145        1.4%        1.4%        0.0% │
│    gpt-5                     87        6.9%        5.7%        1.1% │
│    qwen3-30b-coder          132       58.3%       44.7%       13.6% │
│    ↳ enter expands per-tool / per-stage                             │
│                                                                     │
│  Language (file-touching tool calls)                                │
│    ████████████████░░░░░░░░░░░░░░░░  Rust         45.2%   189 calls │
│    ████████░░░░░░░░░░░░░░░░░░░░░░░░  TypeScript   22.1%    92 calls │
│    █████░░░░░░░░░░░░░░░░░░░░░░░░░░░  Python       14.0%    58 calls │
│    ███░░░░░░░░░░░░░░░░░░░░░░░░░░░░░  Markdown      8.3%    35 calls │
│    ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░  Other        10.4%    43 calls │
│                                                                     │
│  Non-file activity: 412 bash / 76 search / 22 task                  │
│                                                                     │
└─ q quit  s switch scope  r switch range  e expand row  ─────────────┘
```

### 15f. CLI mirror — `cockpit stats`

Same data, plain-text. Useful for scripting and for the `cockpit
meta` workflow (an outer harness can query the inner harness's
performance). Flags: `--project=current|all`, `--range=7d|all`,
`--format=table|json|csv`. Implementation is a thin wrapper around
the same queries the TUI runs.

`cockpit debug repair` (§12) stays as the developer-oriented per-
event view; `cockpit stats` is the user-oriented rolled-up view.
They read the same telemetry; they exist at different abstraction
levels.

### 15g. Severity rubric

The `severity` column in `tool_call_stats` is a float in `[0.0,
1.0]`. The interpretation is *"how badly off was the model's
emission from a clean, schema-valid tool call?"* — not *"how bad
was the outcome,"* since the harness recovers most of these.

Bands:

| Severity | Meaning |
|----------|---------|
| **0.0** | Clean call. Schema-valid input, no repair needed. Tools without recovery semantics (`bash`, `tree`, plain `read`) sit here unless they hard-fail. |
| **0.1–0.3** | Trivial mismatch. Whitespace trim, single-field shape fix, or escape normalization. Fully semantics-preserving recovery; the model was *almost* right. |
| **0.4–0.6** | Moderate mismatch. Multiple normalizations needed, or a structural rearrangement of the args (`wrap_single_arg`, `parse_stringified_array`, `escape_normalized`). |
| **0.7–0.9** | Major mismatch. The harness recovered (e.g. anchor-based or context-aware cascade matched), but the model emitted something substantially different from the file's bytes. A 0.9 is a near-miss-of-the-near-miss; only the last cascade stage saved it. |
| **1.0** | Hard fail. No repair stage matched; the model received an error result. |

Per-stage assignments (this is the table the §15b VIEW
encodes):

| Recovery kind | Stage | Severity | Rationale |
|---------------|-------|----------|-----------|
| `null` (clean) | — | 0.0 | Nothing to repair. |
| `relational_default` | any field | 0.0 | The harness made a semantic *choice* (default `limit`, etc.). Not a model mistake. |
| `edit_cascade` | `line_trim` | 0.10 | One trailing-whitespace miss; model essentially correct. |
| `shape_repair` | `null_for_optional` | 0.20 | Emitted `null` instead of omitting an optional field. Common LLM tic. |
| `edit_cascade` | `whitespace_normalized` | 0.30 | All-whitespace differences across the block. |
| `shape_repair` | `wrap_bare_string` | 0.30 | Emitted `"a"` where the schema wants `["a"]`. |
| `edit_cascade` | `indent_flexible` | 0.40 | Indentation level wrong throughout the block. |
| `shape_repair` | `parse_stringified_array` | 0.40 | Emitted `'["a","b"]'` instead of `["a","b"]`. |
| `edit_cascade` | `escape_normalized` | 0.50 | Confused `\n`/`\t` with the literal character. |
| `shape_repair` | `wrap_single_arg` | 0.50 | Emitted an object where the schema wants an array of objects. |
| `edit_cascade` | `block_anchor` | 0.60 | First/last lines correct, interior content wrong; recovered by Levenshtein on the middle. |
| `edit_cascade` | `trimmed_boundary` | 0.70 | Outer-whitespace and content both off; recovered by trimming + comparison. |
| `edit_cascade` | `context_aware` | 0.90 | Only ≥50% of interior content matched; this is the last-chance stage and a strong signal of model miscalibration. |
| any | `hard_fail = 1` | 1.0 | Nothing matched; model received an error. |

The rubric is **deterministic and stage-keyed**, not learned. We
don't want severity to vary call-to-call for the same `(kind,
stage)`; that would make aggregates unstable. New repair stages
added to §12's catalog or §13b's cascade get a one-line addition
to the rubric and a band assignment based on which family of
mistake they cover.

For tools that don't participate in recovery (`bash`,
plain `read`, `task`, `skill`, ...), severity is binary: 0.0 or
1.0. The middle of the scale is empty, which is fine — they're
still useful to roll up (average severity per project, per model,
per language) and the binary case doesn't drag the average around
much.

**Why store stage names, not severity values.** Storing
`recovery_stage` instead of a baked-in severity scalar means:

- Rubric corrections after release are free (a SQL VIEW change, no migration).
- Stage-level breakdowns are still available for the per-row expand-on-Enter view.
- Cross-stage analytics ("how often does block_anchor fire on Python edits vs Rust edits?") are trivial.
- Telemetry stays portable: a `cockpit stats export --raw` JSONL stream carries the stage names; the consumer can apply any rubric they want.

### 15h. What `/stats` is not

- **Not a billing system.** Token counts and the optional cost
  column are for user awareness; we don't enforce budgets, alert
  on overruns, or stop calls when a quota is hit. (A future
  feature could; v1 stays observational.)
- **Not cross-device, by default.** `/stats` is local-only. The
  one path that ever crosses the network is §16's opt-in benchmark
  telemetry, which is off until the user explicitly enables it and
  ships only k-anonymized aggregates, never the audit blobs.
- **Not a generic model benchmark.** Recovery% on the local pane
  measures how often cockpit's safety net catches *this user's
  models* on *cockpit's specific* tool contract. The cross-user
  public benchmark in §16 is the artifact that *is* a model
  benchmark — `/stats` itself is a per-user observability surface.

---

## 16. Opt-in tool-call performance telemetry — the "at-scale" model question

The interesting research question §15 cannot answer alone: **how
do various models actually perform on cockpit's tool contract at
scale, across many users and many projects?** Right now nobody has
that data published. The local `/stats` pane answers it for one
machine; an opt-in, aggregated, anonymized cross-user channel
would answer it for the community — and the output is a public
benchmark anyone can cite when picking a model.

This is the *only* path through which any cockpit data crosses
the network on its own. It is off until the user opts in. It is
never gated behind payment (see Non-goals). It is aggregated
before transmission and carries no inputs, no outputs, no paths,
no installation IDs.

### 16a. What is sent

Per tool-call event, the following categorical fields are eligible
for inclusion in an aggregated batch:

- `model` (e.g. `claude-opus-4-7`, `qwen3-30b-coder`)
- `provider` (e.g. `anthropic`, `together`, `ollama`)
- `tool` (e.g. `edit`, `read`, `bash`)
- `language` (the §15c attribution — `Rust`, `Python`, `shell`, ...)
- `recovery_kind` and `recovery_stage` (NULL / cascade stage / repair name)
- `hard_fail` (boolean)
- `duration_bucket` (one of `50ms`, `100ms`, `250ms`, `500ms`, `1s`, `2.5s`, `5s`, `5s+`)
- `hour_bucket` (timestamp truncated to the hour, UTC)

That's it. Eight categorical columns. Every field is low-cardinality
on purpose — nothing here uniquely identifies a project, file, or
user.

### 16b. What is never sent

- `original_input_json` / `wire_input_json` — the actual bytes the
  model emitted or the bytes cockpit substituted. **Never.**
- `path` — the file path the call touched, in any form (raw,
  hashed, normalized).
- `project_id` / `project_root` — even the hash. A hash of
  `/home/$USER/projects/foo` is correlatable across batches for
  the same project and is therefore not safe to publish.
- `session_id` / `call_id` / `event_id` — anything that could
  string events together into a per-user timeline.
- Installation identifier of any kind. Each upload is independent.
- IP addresses. The relay logs the request *body* only; the source
  IP is dropped at TLS termination and never written to disk.
- Model output text, prose reasoning, conversation history,
  user prompts.
- Anything `redact::scrub()` (§7) would have caught. The
  telemetry flush runs through the same chokepoint as a final
  sanity check; if it ever flags a hit, the batch is dropped and
  a local error is logged.

The wire payload is a JSON document of count-only aggregates.
There are no per-event rows on the wire.

### 16c. Aggregation and k-anonymity

Events accumulate in a local `cockpit_telemetry_queue` table for
one hour. At flush time, the daemon groups events by the
eight-tuple in §16a and emits a count per cell:

```json
{
  "schema": 1,
  "hour": "2026-05-20T14:00:00Z",
  "cells": [
    {"model": "qwen3-30b-coder", "provider": "together", "tool": "edit",
     "language": "Rust", "recovery_kind": "edit_cascade",
     "recovery_stage": "whitespace_normalized", "hard_fail": false,
     "duration_bucket": "100ms", "count": 23},
    ...
  ]
}
```

**k-anonymity threshold.** Cells with `count < k` (default
`k = 5`, configurable via `telemetry.k_threshold`) are dropped
from the batch entirely — they don't get merged into a "rare
events" bucket, they just don't ship. Rare combinations are
fingerprinting risks; if you're the only user on the planet
making `edit_cascade/context_aware` calls on Brainfuck files, we
don't want to publish that you exist.

**No installation continuity across batches.** Each hourly flush
is one independent POST with no client ID. The relay cannot join
batches from the same machine.

### 16d. Disclosure, control, revocation

Off by default. Opt-in is explicit and gated behind a confirmation
flow:

- `cockpit telemetry enable` (or `/telemetry` in the TUI) opens a
  dialog showing:
  - A sample payload (the literal JSON that would be sent for
    the user's last hour of activity, in dry-run form).
  - The list of fields that are sent and the list that are not.
  - A link to the published privacy notice.
  - A required "I understand what is being sent" checkbox.
- `cockpit telemetry status` — current state, last flush time,
  total cells sent in the last 24h, total cells sent ever.
- `cockpit telemetry preview` — dry-run the next batch. Shows the
  cells that would ship; nothing is transmitted.
- `cockpit telemetry disable` — stops immediately. Any cells still
  in the local queue are dropped.
- `cockpit telemetry delete` — best-effort deletion request to the
  relay. Best-effort because the aggregates may have already been
  merged into the published quarterly dataset. Pre-publication
  cells can be dropped; post-publication data cannot be reliably
  un-aggregated. The CLI surfaces this honestly.

The `/stats` pane shows a one-line telemetry-status footer:
`telemetry: opt-in · last flush 2h ago · 412 cells sent today`
(or `telemetry: off`).

### 16e. Public benchmark — the actual point

The data exists to be published. Quarterly, the cockpit project
releases:

- **A human-readable report** ("cockpit tool-call performance,
  Q3 2026"): model leaderboards by recovery%, per-language
  breakdowns, per-tool patterns, time-trend deltas as new model
  versions ship. Hosted on the cockpit project website.
- **A machine-readable dataset** (JSONL on a public CDN) under
  **CC-BY-4.0** so anyone — model labs, research groups,
  competing harness authors — can build on it.

This is a public good. There is no published baseline today for
"how well do various LLMs behave on a specific harness's tool
contract at scale." Model labs benefit from it. Open-weights
projects benefit from it. End users benefit from it when picking
a model for cockpit. The benchmark cites cockpit by name; cockpit
gets distribution from being the harness that publishes the data.

The benchmark methodology page documents:

- The exact set of fields collected (mirrors §16a).
- The k-anonymity threshold and other suppression rules.
- The §15g severity rubric (so a reader can interpret stage
  weights consistently).
- The cockpit version range each quarter's data covers
  (rubric/cascade changes are versioned).
- A reproducibility note: any cockpit user can re-derive the same
  statistics over their own opted-in data via `cockpit stats
  export --aggregate`.

### 16f. Implementation sketch

- New table `cockpit_telemetry_queue` (mirrors the §16a fields,
  plus `flushed_at` for retention bookkeeping). Written at tool-
  call completion only when opt-in is active.
- Daemon flush task wakes every hour, applies §16c aggregation
  and the k-anonymity threshold, POSTs to a relay endpoint over
  HTTPS, then marks the rows `flushed_at` for 14-day local
  retention (so the user can audit what's been sent). After 14
  days, rows are dropped.
- **Relay endpoint** is a stateless HTTPS service distinct from
  the §8d WebSocket relay. It accepts batched aggregates and
  appends them to a public S3-style bucket. No accounts, no
  per-source tracking, no IP retention beyond the connection
  lifetime. The relay source code is open and published in the
  cockpit organization for inspection.
- **Failure mode:** if the relay is unreachable, batches stay
  queued locally and retry on the next flush. Telemetry transport
  failures are silent (no toast, no log spam) and **never affect
  local cockpit operation**.
- **Schema versioning:** the payload's `"schema"` field lets us
  evolve the wire format without breaking older clients. Clients
  refuse to ship a payload whose schema number the relay's most
  recent published methodology doesn't acknowledge.

### 16g. Why this isn't the consent-or-pay design

The two could look superficially similar — both involve telemetry.
They differ in the load-bearing direction of consent:

- **Consent-or-pay** (rejected, see Non-goals): the *default* is
  data collection; payment is the escape hatch. The user's choice
  is between "give us data" and "pay us money." GDPR Article 7(4)
  scrutinizes this hard because the consent isn't freely given.
- **§16 opt-in benchmark** (this design): the *default* is no
  data collection; explicit opt-in is the escape hatch. The
  user's choice is between "contribute to a public benchmark" and
  "use cockpit privately forever." No money is involved on either
  side. Consent is freely given because refusal carries no
  detriment.

The 16e public benchmark is the *reason* an opted-in user opted
in — they're contributing to a public good, not buying their way
out of a tax.

---

## 17. Sessions, forks, and resumption

cockpit's session model is a tree. Every interactive conversation is
a session; every session belongs to exactly one project; sessions can
be **forked** at any turn boundary, and the resulting forest is what
the user navigates from a single TUI surface. Resumption — picking
up a paused session, hours or days later — is the default mode of
operation, not a special command.

This section captures the session-shaped pieces (storage, IDs,
auto-titling, fork semantics, the `/sessions` browser, per-session
system-prompt injections) in one place. Related primitives live in
§4c (the model-facing `task` mode that exposes fork to the agent)
and `miscellaneous.md` §7 (the fork vs. subagent trade-off
rationale).

### 17a. Project scope

A "project" is the unit by which sessions are bucketed. Resolution
at session-creation time matches the rule §15b already specifies
for `project_id`:

- If cwd is inside a git repo (`git rev-parse --show-toplevel`
  succeeds), the project root is the repo top-level path.
- Otherwise, the project root is `realpath(cwd)` — symlinks
  collapsed.

`project_id` is a short hash of the project root, the same id
that drives `/stats` filtering (§15b). One project identity
across sessions, stats, and the future remote dashboard.

Consequences worth knowing:

- Sessions opened in `~/proj/frontend` and `~/proj/backend`
  (subdirs of one git repo) **share** a session list — they're
  both "this repo." Monorepo users who want subprojects separated
  must work in different repos or accept the unified list.
- Two `git worktree` working trees of the same repo have
  **different** project_ids, because `--show-toplevel` returns
  each worktree's own root.
- Symlinks pointing at the same canonical directory share a
  project_id (realpath resolves to the same target).
- A cwd not under git (e.g. `/tmp/scratch`) gets its own
  project_id keyed to the realpath. Renaming the directory
  fragments history.

The rule is intentional but not flag-controlled in v1. If a real
workflow makes the worktree-split or monorepo-merge behavior
wrong, we add a per-project override later — not a global config
knob.

### 17b. Session IDs

Six base-32 characters (Crockford alphabet — no `I`/`L`/`O`/`U`),
unique within a `project_id`. Generated at session creation,
collision-checked against active sessions in the same project,
regenerated on conflict. ~10⁹ namespace per project — orders of
magnitude beyond any single user's session count.

Display: 6-char id in `/sessions` chrome, in the system-prompt
injection (§17g), and in CLI URLs. The full `(project_id,
session_id)` pair is the DB key.

### 17c. Storage

The daemon owns `~/.local/share/cockpit/cockpit.db` (per §8,
§15b). A new `sessions` table:

```sql
CREATE TABLE sessions (
  session_id          TEXT PRIMARY KEY,
  project_id          TEXT NOT NULL,
  project_root        TEXT NOT NULL,           -- displayed path
  parent_session_id   TEXT,                    -- NULL for root sessions
  fork_point_turn_id  TEXT,                    -- NULL for root; turn id in parent where this fork branched
  title               TEXT,                    -- NULL until utility model auto-titles (§17d)
  user_renamed        INTEGER NOT NULL DEFAULT 0,  -- 1 = manual title; do not auto-overwrite
  created_at          INTEGER NOT NULL,        -- epoch seconds
  updated_at          INTEGER NOT NULL         -- last user interaction
);

CREATE INDEX idx_sessions_project_updated ON sessions (project_id, updated_at DESC);
CREATE INDEX idx_sessions_parent          ON sessions (parent_session_id);
```

The daemon is authoritative. The TUI fetches the list over the
wire schema (§8c); it never walks SQLite directly. This is what
makes the v2 remote dashboard (§8d) free — `/sessions` is one RPC
available to any client speaking the wire schema.

Crash recovery: sessions survive daemon restarts because they're
written through to SQLite at every interaction boundary. The
in-flight model call dies if the daemon crashes mid-stream
(§8b); the session itself does not.

### 17d. Auto-titling via the utility model

When a session's cumulative user-authored content (composer prose
+ `@`-tagged inlined file content, **excluding** the base system
prompt and the resolved `agent_guidance_files` per §4b) crosses
**500 tokens** (cl100k_base; same tokenizer §10 uses for
subagent caps), the daemon issues one inference call to
`utility_model` (§4h) to produce a title.

Title format:

- Slugified: `[a-z0-9-]` only; runs of non-allowed characters
  collapse to single hyphens; leading/trailing hyphens trimmed.
- ≤ 60 characters.
- One title per session. No auto-generated description in v1 —
  the second inference call costs token spend for a field with
  no consumer yet. `/sessions` shows the title; that's enough.

Behaviors:

- The 500-token threshold counts only fresh user-authored
  content. A short "hi" first message doesn't trigger titling; a
  substantive first message with several turns or a meaningful
  `@`-tag does.
- If `utility_model` is unset, no auto-titling occurs. The
  session's row keeps `title = NULL` and `/sessions` displays the
  session-ID as the label.
- Manual override is available: `/session rename <new-title>`
  from inside a session, or in-place from `/sessions`. Manual
  rename sets `user_renamed = 1` and the utility-model pass
  thereafter does not overwrite the title — even if the user
  clears it back to NULL, the row stays user-owned.
- **Forks get their own auto-titling pass keyed to the first
  user message *after* the fork**, not the parent's title.
  Triggered by the same 500-token rule applied to the fork's
  post-divergence content only.

The utility-model call is one-shot and non-interactive: it's a
small prompt (the user-authored content prefix + a "produce a
title" instruction) and the response is parsed strictly. If
parsing fails or the model returns garbage, the title stays NULL
and the daemon retries on the next interaction boundary.

### 17e. Fork semantics

`/fork` creates a new session that branches from a parent at a
turn boundary. The fork inherits the parent's full conversation
up to the fork point and diverges from there. This is the
user-facing slash command on top of the §4c primitive — the
model can fork (`task({mode: "fork"})`) and the user can fork
(`/fork`); both write the same `sessions` row shape.

Two ways to invoke from the TUI:

- **Tail fork.** `/fork` with no argument while inside a session.
  Forks at the last assistant turn. The common case: "save this
  state and try something different from here."
- **Mid-history fork.** Scroll the resumed session's transcript
  to a prior message, press the fork keybinding. Forks at that
  turn. Use case: "rewind, branch, retry" — escape a derailed
  path without losing it.

Both write `(parent_session_id, fork_point_turn_id)` on the new
row. The fork's wire transcript starts as a deep copy of the
parent's wire transcript up to the fork point; the §14 wire/user
split is preserved (both projections of the parent are reachable
in the fork).

Forks can be forked. There is no depth limit. The session tree
is `parent_session_id`-linked all the way down; `/sessions`
(§17f) renders it.

Forks do **not** share live state with their parent after
divergence. A `coder` file lock acquired in the parent session is
not held in the fork; the fork acquires its own locks against
the file system. This keeps "explore an alternative direction"
safe — a fork can edit files the parent was about to edit
without stepping on the parent's in-flight work.

### 17f. `/sessions` and `/resume`

`/sessions` opens a TUI browser of the active project's session
tree. `/resume` is an alias.

Layout:

```
┌─ /sessions ─ project: cockpit-cli ──────────────────────────────────┐
│                                                                     │
│  fix-redact-allowlist-regression                  2m ago            │
│ ▌add-pixel-banner                                45m ago [3 forks] │
│  refactor-config-loader                           3h ago            │
│  …                                                                  │
│                                                                     │
└─ ↑↓ jk select  → l forks  ↵ resume  f fork  r rename  q quit ──────┘
```

- **Recency.** Sorted by `updated_at` descending — most recent
  session first.
- **Navigation.** `↑`/`↓`/`j`/`k` cursor between sessions.
  `→`/`l` descends into the selected session's forks (when
  `[N forks]` is shown). `←`/`h` returns to the parent level.
  Arbitrary depth — fork-of-fork-of-fork composes the same way.
- **Resumption.** `Enter` on a cursor-selected session resumes
  it at its tail. The next message goes to whichever agent was
  driving when the conversation paused (§1a's active-agent
  chip identifies it).
- **Forking.** `f` (default; rebindable) tail-forks the
  cursor-selected session without entering it. To mid-history
  fork, press Enter to enter the session, scroll the transcript
  to the target message, press the fork keybinding there.
- **Rename.** `r` opens an inline rename for the cursor-selected
  session and sets `user_renamed = 1` (§17d).
- **Cross-project visibility.** None in v1. The browser shows
  only sessions in the current project. A cross-project surface
  is a different problem and is out of scope until someone asks.

### 17g. System-prompt injections

Three pieces of context attach to every session. Two are stable
(go in the cached system block); one is volatile (must not
invalidate the cache on every send).

**Stable — cached system prompt:**

1. **Operating system + version.** One line, e.g.
   `Operating system: Linux 6.8.0-111-generic` (or the
   macOS/Windows equivalent). Doesn't change for the session's
   lifetime; counts against the §10 ~400-token budget.
2. **Session ID.** One line: `Session: <6-char id>`. Counts
   against the same budget. The model can echo it back to the
   user when needed (e.g. surfacing the id in a status report).

**Volatile — message-level prelude:**

3. **Current time.** A one-line prelude prepended to user
   messages, of the form `[time: 2026-05-26T14:32:11Z]` (ISO
   8601, UTC). Rules:
   - The **first** user message of the session always carries a
     time prelude.
   - **Subsequent** messages carry a prelude only when ≥ N
     minutes have elapsed since the last prelude (default
     `N = 5`, configured as
     `system_prompt.time_injection_interval_minutes`, §4k).
   - The system prompt itself **never** carries the time.

This split is non-negotiable per §10's token-economy commitments:
putting the time in the cached system block would invalidate the
provider's prompt cache on every request. The prelude approach
gives the model a rolling sense of time without paying the cache
cost; in a continuous conversation the model sees a fresh
timestamp ~once per interval, not once per turn.

### 17h. Wire-protocol surfaces

Per §8c, sessions live in the daemon and clients (TUI today,
remote dashboard later) reach them via the wire schema. The RPCs
needed for §17:

| RPC                                                                 | Used by |
|---------------------------------------------------------------------|---------|
| `sessions.list(project_id, parent_session_id?)`                     | `/sessions` browser, recency-sorted |
| `sessions.get(session_id)`                                          | Loading metadata for resume |
| `sessions.resume(session_id)`                                       | Attaching the client to a session |
| `sessions.fork(parent_session_id, fork_point_turn_id?)`             | `/fork`, model `task({mode: "fork"})` |
| `sessions.rename(session_id, title)`                                | Manual title override (`r` in browser) |
| `sessions.delete(session_id, cascade?: bool)`                       | Cleanup — `cascade` controls whether forks are also deleted |

One schema, two transports (Unix socket today; outbound WebSocket
to relay later, per §8d). The remote-dashboard endgame falls out
of the v1 architecture without protocol churn.

---

## 18. MCP support — lazy discovery

cockpit supports the Model Context Protocol. The earlier "no MCP"
policy was driven by the §10 token-economy concern that MCP servers'
per-tool schemas routinely sum to thousands of tokens of system-prompt
overhead. The lazy-discovery design below removes that cost from the
hot path while preserving the user-facing "MCP just works" experience.

### 18a. Design

- **Two-stage tool exposure.** The model sees a single built-in tool:
  `mcp_invoke(server, tool, args)`. Alongside it, the system prompt
  carries a **catalog** of `(server.tool_name, one-line description)`
  pairs — one line per available MCP tool, regardless of how many
  servers are configured. The full JSON schema for any given tool is
  never in the system prompt.
- **On-demand schema load.** When the model calls
  `mcp_invoke("github", "create_issue", { … })`, the dispatcher
  fetches the tool's schema (cached after first fetch), runs the
  existing tool-input repair pipeline (§12) against it, and
  dispatches.
- **Catalog refresh cadence.** Catalogs are pulled at daemon startup
  and on demand via `cockpit mcp refresh`; they are not re-pulled per
  inference call. A tool that has been removed from the upstream
  server but is still in the catalog fails on `mcp_invoke` with the
  same `[error: unknown tool …]` shape as a stale skill reference.
- **Server config lives in `.cockpit/mcp.json`** (layered, walk-up,
  per §2). Each entry: `name`, `transport` (`stdio` | `http-sse`),
  `command` (for stdio) or `url` (for sse), optional `env`,
  `headers`, `timeout_secs`. The shape is opencode-compatible but
  cockpit owns the file.
- **`cockpit mcp` subcommand.** `add`, `list`, `test` (smoke-test the
  server and dump its tool catalog), `refresh` (re-pull catalogs).
  No `cockpit mcp run` — invocation happens via `mcp_invoke`.

### 18b. v1 scope

cockpit's MCP support covers **tools, resources, and subscriptions**
in v1 (decided 2026-05-27). Prompts and sampling are deferred until
there's concrete user demand.

- **Tools.** Lazy-discovery as described in §18a. Model sees
  `(server.tool, one-line description)`; full schema loads on
  `mcp_invoke`.
- **Resources.** URI-addressable content the server exposes (files,
  database rows, web content). Discovery is lazy on the same model
  as tools: the catalog carries `(server, uri_template, one-line
  description)` only. The model accesses a resource via
  `mcp_resource_read(server, uri)`; the result body and any MIME
  metadata land as a normal tool-result. Resources that map cleanly
  onto a local file (`file://` URIs from a filesystem server, for
  example) are still routed through `mcp_resource_read` rather than
  being silently aliased to `read`, so the wire/user transcript
  split (§14) preserves provenance.
- **Subscriptions.** When the model issues
  `mcp_resource_subscribe(server, uri)`, the daemon registers the
  subscription with the upstream server. Notifications are surfaced
  as **per-turn prelude notes** on the next user message in the
  affected session — same shape as T6.a's read-staleness note:
  `[note: resource <uri> changed since you subscribed]`. This keeps
  subscriptions inside the turn-boundary model (no mid-tool-call
  notification handling). The daemon retains the subscription across
  TUI client reconnects; explicit `mcp_resource_unsubscribe` or
  session-end tears it down. See
  `design-need-to-discuss-or-test.md` for the open questions on
  notification fan-out and rate-limiting.
- **Prompts and sampling.** Deferred. No concrete user demand and
  both interact awkwardly with cockpit's existing primitives
  (prompts overlap with skills; sampling overlaps with delegation).
- **Stdio + http-sse transports.** WebSocket and custom transports
  deferred.
- **Per-daemon server processes.** The daemon spawns and manages MCP
  stdio servers; clients (TUI, future web) do not. One server
  process per `mcp.json` entry, reused across sessions.
- **No per-session OAuth state.** OAuth-bearing MCP servers use a
  shared token store at the daemon level; per-session credential
  isolation is deferred until we have a multi-tenant story.

### 18c. Why this isn't a regression

The §10 token-economy bullet that previously forbade MCP cited
"thousands of tokens per server" — that was the right diagnosis of
the wrong design. The lazy catalog reduces per-tool overhead to one
line; a 50-tool MCP server adds ~50 lines (≈400 tokens) to the
system prompt, comparable to one skill. The full schema cost is paid
once per `mcp_invoke` call, exactly where the model needs it.

`mcp2cli-rs` remains a valid escape hatch for users who specifically
want to wrap MCP tools as CLI invocations under `bash`, but it is
no longer the recommended path.

---

## 19. Future paid surfaces (roadmap, not v1)

cockpit is OSS-core. A small set of paid surfaces are on the roadmap
and constrain the v1 architecture even though they don't ship in v1.
None of these are committed; they are listed here so that v1 design
decisions don't foreclose them.

- **Mobile / browser client.** The §8d `cockpit connect` outbound
  WebSocket is the mechanism; the hosted relay + mobile/web client is
  the surface. **Constraint on v1:** the NDJSON wire schema (§8c) must
  stay client-agnostic. TUI-specific assumptions (cursor positions,
  terminal capabilities) must not leak into the protocol.
- **Account-synced configs.** Save/restore the user-level config
  layers (§2) to a cockpit account for easy import on new machines.
  **Constraint on v1:** layer identity must be stable (so a sync
  target is well-defined); machine-local layers (`/srv`, `/opt`) and
  `credentials.json` are categorically not syncable. Conflict
  resolution is unresolved (see `design-need-to-discuss-or-test.md`).
- **Games while agent works.** Entertainment surface in the TUI / web
  client for long-running agent tasks. **Constraint on v1:** the
  fullscreen TUI layout (§1) must leave structural room for a
  sidebar or modal so this isn't foreclosed.

---

## 20. Provider auth — sanctioned vs passthrough

Provider authentication paths in cockpit fall into two categories.
This distinction must be surfaced in the Add-Provider wizard and in
`cockpit providers status`.

- **Sanctioned.** The vendor publishes an OAuth client-registration
  program or documents the auth flow for third-party clients.
  Examples: every API-key flow (Anthropic, OpenAI Platform, Mistral,
  DeepSeek, OpenRouter, etc.); GitHub Copilot's editor device-code
  flow (the published `01ab8ac9400c4e429b23` client id).
- **Passthrough.** cockpit re-uses a first-party client's own OAuth
  client id to access the vendor's subscription service from a
  non-first-party tool. The vendor neither publishes nor sanctions
  this path; it works because the source for the first-party client
  is open. Examples:
  - **Codex (ChatGPT Plus/Pro).** `src/auth/codex.rs` re-uses the
    Codex CLI's `CLIENT_ID = app_EMoamEEZ73f0CkXaXp7hrann` to spend
    ChatGPT subscription quota.
  - **Claude (Pro/Max).** *If* we add it — same posture; under
    discussion.

**Policy.**
1. Passthrough flows must not be the default in the Add-Provider
   wizard. API-key paths are first.
2. The wizard and `cockpit providers status` must label passthrough
   flows with a one-line warning: *"Uses your ChatGPT Plus quota via
   the Codex CLI's OAuth client. Not officially supported by OpenAI;
   may stop working without notice."*
3. Implementation lives under `src/auth/<vendor>.rs`; never share an
   OAuth client id across `auth/` modules.

---

## 21. Codebase-intelligence tools

A first-class set of read-only navigation tools backed by a
tree-sitter outline index, so agents orient and navigate at minimal
token cost instead of reading whole files or shelling to `rg`. Full
design in `codebase-intelligence.md`; build spec in
`prompts/codebase-intelligence-tools.md`. `kcl` ships the same tools
as `kcl explore …` subcommands — study it (`kcl ask kcl …`) before
implementing.

**Phase-1 surface (all CK-approved):** `tree` (annotated dir tree),
`outline` (per-file symbols), `symbol_find` (definition sites),
`word` (exact-identifier inverted index), `deps` (file import graph,
forward + reverse), `hot` (recently modified), `circular` (import
cycles), `search` (budget-capped structured content search), plus a
line-range extension to the existing `read`. `impact` (symbol blast
radius) and a trigram search index are Phase 2.

**Decisions.**

- **Distinct tools, precise schemas** — not a meta-tool. The set is
  known per-agent at session start, so fixed precise schemas give the
  weak target models and the repair layer (§12) a clear contract.
- **On-demand invalidation, no file watcher.** Each call re-stats
  tracked files (mtime+size, hash tiebreaker) and re-indexes
  stale/removed ones through one central indexing helper before
  answering. A watcher's silent-staleness failure mode loses to the
  top priority (correctness for weak models); it may later be added
  only as a dirty-marking accelerator over the on-demand path.
- **Index in the cockpit SQLite DB**, project-scoped so multi-project
  is an additive change. Six tables (`files`, `symbols`, `imports`,
  `identifiers`, `deps`, `callsites`).
- **Budgeted output** via `tokens.rs` (`search` default 4 000-token
  cap), dropping whole writes atomically to keep a valid prefix (§10).
- **No `grep`/`glob` intel tool.** Raw search is `bash` + `rg`/`fd`;
  `search` is the budgeted/structured path. (The separate sandboxed
  `grep`/`glob` *tools* on the `docs` answerer — §3a — are not intel
  tools and are not part of this index; they run the ripgrep
  libraries directly over a confined dependency-clone cwd.)

**Per-agent assignment (starting default; revisit later).** `explore`:
all. `coder`: `read`(line-range), `outline`, `symbol_find`, `deps`,
`circular`, `word`, `search` (+ its write tools and `task→docs`;
`impact` joins in Phase 2). `orchestrator-plan`: `tree`, `deps`,
`circular`, `hot`. `orchestrator-build`: `tree`, `hot`. The `docs`
answerer (Docs.2) does **not** use the intel index — it uses
`read` + the sandboxed `grep`/`glob` (a clean seam is left to add the
intel tools to Docs.2 later, but they are not wired here).

---

## 22. Async jobs — loop / timer / background

Agents can schedule recurring self-prompts (`loop`), one-shot delayed
prompts (`timer`), and background shell jobs (`background`) that run
without blocking the human. Build spec in
`prompts/async-jobs-subsystem.md`. The three share one daemon-resident
async subsystem; they differ only in trigger (interval / once /
process-exit) and body (re-prompt / shell).

**One `jobs` meta-tool, not three tools.** The surface grows
mid-conversation (`background.tail`/`cancel` are meaningless until a
background exists). A meta-tool with a **fixed minimal schema**
(`action` + `args`) keeps the tools array byte-stable so capability
growth never busts the prompt cache; a branch is enabled by appending
a hint message + accepting the action at dispatch (both cache-safe),
with per-action args validated through the repair layer (§12). This is
the canonical mid-conversation-growth pattern (see CLAUDE.md design
rules); distinct precise-schema tools are still used where the set is
fixed at start (e.g. §21).

**Branches.** `loop.start(interval, prompt, backoff=false,
limit=10|∞, keep_in_context=true, independent=false)`; `loop.cancel`
(enabled while a loop is live; available in main, and a fork may
cancel its own loop); `background.start(cmd)` → returns a handle
immediately; `background.tail` / `background.cancel` (enabled after a
background exists).

**`timer` = `loop.start` with `limit=1`** — no separate tool; the UI
renders a `limit=1` loop as a timer.

**Ephemeral-fork loops (`keep_in_context=false`).** Each iteration
runs in a fork branched from the main context at registration.
`independent` chooses fresh-fork-per-iteration vs accumulate-in-fork
(default). Nothing crosses to main during the loop except `note(text)`
— the only fork→main channel, shown live in the UI but injected into
main context only at termination; the terminal iteration's full result
is promoted to main.

**Single async-job authority (anti-runaway).** Main owns all jobs —
same shape as single-writer `coder` (§3a). A fork's
`loop.start`/`background.start` do not execute; they emit a request
routed back to main, which decides whether to run it. Prevents
recursive/runaway loops.

**Surfacing.** Completions, timer fires, and background exits inject
as a late-arriving turn at the next turn boundary (daemon NDJSON
proto, §8); output is budget-capped (§10). A transient jobs strip
(shown only when ≥1 job is active — additive to the fixed chrome §1a),
inline completion events, a `/jobs` list-and-cancel command, and
`needs_attention` flagging on end/failure.

---

## 23. Compact-after-delegation

When the main agent delegates to a sub-agent, the wait can outlast the
provider's prompt-cache TTL, so the main context's cache goes cold.
This hides the cost by preparing a smaller main context to resume from
when the cache is lost. Build spec in
`prompts/compact-after-delegation.md`.

- **Cache-capable provider — lazy.** Don't shrink at delegation start;
  only if the sub-agent is still running at TTL-minus-margin, shrink
  in parallel. Fast delegations waste nothing.
- **No-cache provider — eager.** Shrink at delegation start (no cache
  to protect; latency hides under the delegation).
- **On return:** cache hot → resume on the full context (no quality
  loss); cache cold → resume on the shrunk context (smaller cold read).
- **Shrink strategy is a setting:** `prune` (default — matches the
  existing "auto-prune when expected cache-hit = 0" policy, §10 /
  `plan.md` T6.f; lower quality loss) or `compact` (the §T6.e
  fresh-thread handoff; heavier, more savings).
- **Cache-cold check reuses the existing auto-prune predicate** (§10)
  — no second cache-state heuristic.
- Runs daemon-side (the daemon owns session/inference state). The same
  logic applies to background delegations (§22).

**Future (hook now, UI later).** Per-provider/model context-usage
thresholds for autocache/autoprune live in the per-model config layer
(§4; the three-knob cap per `design-need-to-discuss-or-test.md` D8),
so adding the threshold UI later is additive.

---

## Non-goals

- **(Removed)** *MCP support* was previously a non-goal. As of
  2026-05-27 this is reversed — cockpit ships first-class MCP support
  via a lazy-discovery design that preserves the §10 token-economy
  invariant. See §18.
- **Web UI / remote-accessible HTTP server** (`opencode serve`,
  `opencode web`, `opencode acp`). v1 ships a local daemon (§8)
  with a TUI client, no web UI. The daemon's socket is
  local-only and listens on a Unix socket (or per-user named
  pipe on Windows), not a TCP port. The remote story is
  `cockpit connect` (§8d), which is a daemon-initiated outbound
  WebSocket to a hosted relay — not a server the user exposes.
- **Hosted session sharing** (opencode `/share`). Privacy concerns plus
  no clear user demand.
- **Auto-update** (`opencode upgrade`). Users install via cargo or
  their package manager; `cockpit` does not self-modify.
- **GitHub agent** (`opencode github`). Out of scope for the
  user-controlled-harness use case.
- **Hosted plugin marketplace** (`opencode plugin <npm-pkg>`). Plugins
  are out of scope; the meta-harness (§6) covers most extension needs.
- **Paywalled telemetry opt-out is out of scope.** cockpit does not
  gate the right to refuse data collection behind a paid SKU.
  Consent-or-pay is legally exposed in the EU (GDPR Article 7(4),
  EDPB April 2024 opinion), creates a financial-incentive-disclosure
  obligation under CCPA, and would be a community-relations
  liability for an open-source developer tool. The §16 opt-in
  benchmark channel is the only mechanism through which cockpit
  ever transmits data, and it is off until the user explicitly
  enables it. Cloud features that inherently require telemetry
  (the §8d remote dashboard, cross-device sync) may be paid in the
  future, but the data collection is bundled with the feature it
  powers, not a condition for avoiding it.
