# cockpit TUI — design philosophy

This doc distills the design choices behind the ralph-rs TUI (see
`../ralph-rs/TUI-plan.md` for the source spec) into general guidelines for
`cockpit`. It covers how the TUI feels under the fingers, how it stays
discoverable for non-vim users, and how it stays scriptable for users who
want it out of the way.

It is not a feature list. It is a set of rules that any TUI surface in
`cockpit` — composer, slash menu, approval dialog, future sub-views — has to
honor.

---

## 1. North stars

1. **Vim-friendly, not vim-exclusive.** Every binding has a non-vim
   equivalent (`h j k l` ⇌ arrows, `enter`/`l`/`→` to open, `q`/`esc`
   to back out). Power users get the muscle memory; everyone else gets
   the GUI.
2. **Discoverability is one keystroke away.** `?` opens a per-view help
   overlay. The bottom hint bar reminds you of the current view's
   common keys without asking.
3. **The TUI is opt-in by default and opt-out under load.** Bare
   invocation (`cockpit`) opens the TUI. Any non-default flag, a non-TTY
   stdout, or `--non-interactive` drops to plain mode. Existing
   scripted use must never regress.
4. **State is a pure function of input.** Each view is a state struct
   with a pure `handle_key(KeyEvent) -> Action` method. Rendering and
   side effects live elsewhere. This is how the TUI gets unit-tested
   without a real terminal.

---

## 2. Keymap

### 2a. Universal navigation

The same six keys move you everywhere — list view, message history,
slash menu, approval dialog, every focusable surface:

| Direction | Vim | Arrows / Enter |
| --------- | --- | -------------- |
| Up        | `k` | `↑`            |
| Down      | `j` | `↓`            |
| Left / back / collapse | `h` | `←`            |
| Right / open / expand  | `l` | `→` or `enter` |
| Top       | `g` | `Home`         |
| Bottom    | `G` | `End`          |

`enter` is interchangeable with `l`/`→` for "open this." Don't bind
`enter` to do something different from "open" unless the surface is an
input field.

### 2b. Modes

The composer is modal (Normal / Insert), per `GOALS.md` §1b. Other
TUI surfaces can be modal too (e.g., a list with a Normal mode and
an inline-input mode for adding items). When a surface goes modal:

- `<esc>` always returns to Normal mode.
- Normal mode never accepts text input. Single-key bindings only.
- Insert mode passes everything except `<esc>` and `Ctrl-C` through.

### 2c. Modifiers carry weight, not novelty

Capital and Shift-modified vim keys are reserved for **stronger**
versions of the lowercase action — not arbitrary unrelated
operations:

- `j` / `k` move; `J` / `K` push items down/up.
- `g` / `G` jump top/bottom.
- `d` deletes one item; `D` opens a destructive sub-view (or
  prompts for a multi-delete).

Don't burn `Shift-X` on something semantically unrelated to `x`.

### 2d. `<esc>` and `q` are escalating cancels

- `<esc>` first clears transient state (selection, in-progress
  input, dismissable toast).
- A second `<esc>` (or `q`) pops the view.
- `q` from the root view quits.
- `Ctrl-C` is always equivalent to "the most aggressive cancel
  available here" — close the modal, kill the prompt, exit the TUI.

Never make `<esc>` do something destructive. It's the universal
"oops" key.

### 2e. `?` is sacred

Every view binds `?` to toggle a help overlay listing **every
binding the current view accepts**, grouped by category
(Navigation, Edit, Run, Other). Don't ship a view without one.

The overlay intercepts input before the view's own handler. Inside
the overlay, only `?`, `<esc>`, `q`, `Ctrl-C` close it; every other
key is swallowed (no accidental fall-through).

---

## 3. Slash / colon command palette

Both `/` and `:` open the same single-line input bar at the bottom
of the screen and submit through the same parser. Codex / Claude
muscle memory and vim ex-command muscle memory both work; pick
whichever is yours.

Rules:

- The palette is a **superset** of the keybindings. Every
  destination reachable by a key must also be reachable by a
  named command (and the help overlay should mention both).
- `<tab>` cycles completions: command names first, then valid
  argument values pulled from the same source as `clap_complete`.
- Unknown command → toast error, never a crash.
- `<esc>` closes the bar; partial input is discarded.
- Submitting a recognized command that has no implementation yet
  is allowed during development — toast `"<verb> not yet
  implemented"` rather than refusing to parse it.

The palette is also where any feature that's "too rare to keybind"
lives. Don't accumulate Shift-Alt-Ctrl chord bindings; route them
through `/`.

---

## 4. Visual chrome

### 4a. Always-visible structural cues

Per `GOALS.md` §1a, cwd and git branch are part of the chrome and
not opt-in. Add to that:

- **Top breadcrumb** showing the user's location
  (`cockpit › conversation › approval`). One line, bold, right-truncated
  with `…` when it overflows.
- **Bottom hint bar** showing the most useful 4-6 keys for the
  current view (e.g., `[j/k] nav  [enter] open  [/] cmd  [?]
  help  [q] quit`). Update it per view.
- **cwd + version** bottom-right, left-truncated with `…` when the
  path is long. cwd uses `~` substitution (`~/p/d/cockpit-cli`).

### 4b. Status banners override hints

When the TUI enters a non-default state (read-only attach,
redaction failure block, harness disconnected), replace the bottom
hint bar with a colored banner that's bold and stands out. The
banner takes priority over the hint until the state clears.

Don't spawn a separate banner row — reuse the hint slot. Vertical
real estate is precious.

### 4c. Status is always glyph-and-color, never color alone

Color-blind users, tmux-in-a-screenshot, log paste-ups: all of
them lose color. Pair every colored status indicator with a
shape:

- `○` pending, `▶` running, `✔` done, `✘` failed, `⊘` skipped.
- Counts (`3/7`) where applicable.
- Border weight: dim by default, solid for cursor, accent for
  selection.

### 4d. Centralized theme

All colors live as named constants in one module
(`src/tui/theme.rs`). Token names describe the **role** (`cursor`,
`selection`, `status_complete`, `toast_error`) rather than the
hue. Views never hardcode `Color::Rgb(...)` inline.

Cursor highlight (`#f7d135`-ish yellow) and multi-select
(`#56d0d9`-ish cyan) must be visually distinct so a row that is
both highlighted *and* selected is unambiguous.

Truecolor is assumed; ratatui's degradation to 256-color is
acceptable. Don't write explicit fallback paths.

### 4e. History cell collapse for long content

Some history cells carry content that's load-bearing the moment
it's produced and largely uninteresting after — thinking blocks
are the canonical example; long tool results and bash output
fall in the same bucket. The default render rule:

- **≤ 2 lines:** show in full. No collapse mechanism.
- **≥ 3 lines:** collapse to four visible rows:
  ```
  <first line of content>
  . . .                              ← animated while streaming, static when complete
  <second-to-last line>
  <last line>
  ```

For thinking blocks specifically, the animated ellipsis cycles
`.` → `. .` → `. . .` → loop at ~400ms per frame while the model
is actively emitting. Frame rate is honest "thinking is
happening" feedback without being twitchy. When the block
completes, the animation stops and the row settles on static
`. . .`.

The collapsed form applies to **both** active and completed
blocks. The user shouldn't have to re-learn the rule once the
stream finishes; scrollback stays scannable for the same reason
live thinking does.

**Click to expand.** A click on the cell toggles between
collapsed and expanded. Per-cell state machine:

```rust
enum ThinkingDisplay {
    Collapsed,
    Expanding { started_at: Instant },
    Expanded,
    Collapsing { started_at: Instant },
}
```

Each frame, `Expanding`/`Collapsing` reveals or hides one row
based on a cubic-bezier (0.4, 0, 0.2, 1) curve mapping elapsed
time to row count. Slow at the edges, fast through the middle —
reads as ease-in-out to the user despite the discrete row
boundary. Linear at ~16ms/row is the acceptable baseline if the
curve is more work than it's worth.

**Honesty about smoothness.** Terminal cells are discrete. True
sub-cell animation is a lie. The illusion of smoothness comes
from varying *when* rows reveal, not from rendering partial
rows. Don't ship "smoothing" tricks that bend that constraint
(half-block glyphs as half-rows, etc.) — they break in screen
readers, tmux paste-ups, and SSH-over-mosh, all of which the TUI
is otherwise honest in.

**Mouse plumbing.** `crossterm::event::MouseEvent` carries the
click; cells stash their screen rect each frame in a hit-test
table the chat surface owns. No global mouse-handler — the cell
type registers itself like any other interactive element. Same
shape that powers the slash-command menu.

**Source of truth.** The collapsed view never deletes content.
The full body always lives in the on-disk transcript (see
`plan.md` T6.f three-layer fidelity model). Re-rendering on
expand reads from the cell's in-memory `Part`, or — if the cell
has been wire-elided to `Part::Elided` — resolves
`original_event_id` against the persisted event log.

---

## 5. Selection model

When a view supports selecting multiple items:

- `space` toggles selection on the highlighted item.
- Selection is **ordered** — render `[1] [2] [3]` badges so users
  can see the order they picked things in.
- `<esc>` clears selection (escalating cancel — see §2d).
- Destructive actions follow the rule: **selection wins; if no
  selection, target the cursor**. A single confirm dialog covers
  the batch ("Delete 5 items?"), never one dialog per item.
- Selection survives list resorts/refreshes by keying on stable
  IDs (UUID, slug), not list index.

---

## 6. Confirmations and destructive actions

- Anything that destroys data, sends a network request the user
  didn't explicitly ask for, or affects state outside the TUI
  process **must** prompt with a confirm dialog.
- Default button mirrors the safety: `[y/N]` for destructive,
  `[Y/n]` for "yes is the obvious answer."
- `<esc>` and `Ctrl-C` always cancel. `<enter>` always picks the
  default button.
- Approval dialogs for harness actions (per `GOALS.md` §1) are
  the same primitive — same key bindings, same default semantics.

Don't add a "are you sure" dialog for reversible actions. `<esc>`
is enough.

---

## 7. Toasts

Transient feedback (`Saved.`, `No $EDITOR set`, `Plan archived`)
goes in a toast slot that overlays the bottom hint bar with a
3-second TTL.

Rules:

- Color says intent: green = success, red = error, blue = info.
- `<esc>` dismisses early.
- Newest toast covers older ones; popping it reveals the next.
- Errors that **block** an action don't go in a toast — they go in
  a dialog or banner. Toasts are for "FYI"-class messages.

---

## 8. Editor handoff

Long-form editing — composer overflow, agent file editing, prompt
editing — happens in `$EDITOR`, not in a ratatui textarea.

The handshake is the standard one: `LeaveAlternateScreen`
+ `disable_raw_mode` → spawn `$EDITOR` (then `$VISUAL`) on a
tempfile inheriting stdio → `EnterAlternateScreen`
+ `enable_raw_mode` on exit. Editor non-zero exit = cancel,
preserve original text.

Tempfiles live under cockpit's data dir, namespaced by scope. Delete
on successful save; leave on cancel so users can recover their
work.

If neither `$EDITOR` nor `$VISUAL` is set, toast a red error
telling the user to set one. Don't fall back to a built-in editor
silently.

---

## 9. State machines, not god-objects

Each view is three pieces:

```
src/tui/views/<view>.rs              — App state struct + pure mutators
src/tui/views/<view>_input.rs        — handle_key(KeyEvent) -> Action
src/tui/views/<view>_ui.rs           — render(frame, area, &App)
```

The dispatcher (in `src/commands/...`) owns the alternate-screen
session, the crossterm event loop, and any side effect (network
call, file write, subprocess spawn). It calls `handle_key`, gets
back an `Action` enum, executes it, and loops.

Why the split:

- `handle_key` is pure → unit-testable without a TTY.
- Render is pure → snapshot-testable against `TestBackend`.
- Side effects are concentrated → easy to mock the surrounding
  shell and assert on what the dispatcher tried to do.

Help overlay state (`HelpState`) lives on the App and is
consulted before the view's own handler. Don't open-code `?`
handling per view.

---

## 10. When the TUI is the wrong answer

The TUI is the default when the user typed `cockpit` (or any
TUI-eligible subcommand) at a TTY. It is **not** the default
when:

- stdout is not a TTY (piped, captured by a parent harness, etc.).
- The user passed `--non-interactive` (a global flag).
- The user passed any non-default flag to a subcommand that has
  scripted semantics (mirrors ralph's "any flag drops to
  non-interactive" rule).

Auto-detection beats configuration. Don't ask the user to
remember a flag for the common case.

When the TUI auto-disables, the output format must match the
existing scripted contract exactly. New event types added to
`--json` / NDJSON output are additive only, and old event types
stay around for at least one release cycle so meta-harnesses
pinned to them don't break.

---

## 11. Cooperative attach

When another `cockpit` (or another harness) is already touching the
same project — holding a lock, mid-conversation, mid-approval —
the TUI **attaches read-only** rather than refusing to start:

- Show a banner explaining the state and the PID.
- Disable every edit binding (`c`, `i`, `a`, `d`, `r`, etc.).
- Keep navigation, view-switching, `?`, `q`, and the most
  aggressive cancel (`Ctrl-C`, `S` for stop) active.
- Re-enable edit bindings automatically when the lock releases;
  don't make the user reload.

Read-only attach is what makes the TUI feel cooperative with the
rest of the device's harnesses, including future `cockpit meta`
flows where multiple cockpit processes are in flight.

---

## 12. Token-economy is also a TUI concern

`GOALS.md` §10 sets the token-economy rules for prompts and tool
descriptions. The TUI's contribution to that budget is:

- Help overlay text and palette descriptions are **not** sent to
  the model. They can be as long as they need to be for clarity.
- Approval dialogs and slash-menu hints **are** rendered locally
  only — never embedded in a system prompt or tool result.
- Anything the TUI gathers from the user (text input, picker
  selection, `c`-handoff result) flows through the same
  `redact::scrub()` chokepoint as everything else before crossing
  the network. There is no TUI-side bypass.

The TUI is allowed to be verbose with the user. It is not allowed
to be verbose with the model.

---

## 13. Quick checklist for new TUI surfaces

Before merging a new view, picker, dialog, or sub-view, confirm:

- [ ] `j`/`k` and `↑`/`↓` both work for navigation.
- [ ] `h`/`←` and `l`/`→` work where left/right is meaningful.
- [ ] `enter` is interchangeable with `l`/`→` for "open."
- [ ] `?` opens a help overlay listing every binding the surface
      accepts, grouped sensibly.
- [ ] `<esc>` clears transient state; second `<esc>` pops the
      surface.
- [ ] `Ctrl-C` is bound and does the most aggressive cancel.
- [ ] Bottom hint bar shows the 4-6 most useful keys.
- [ ] Status / state cues use a glyph **and** a color, never just
      one.
- [ ] All colors come from `theme.rs`. No inline `Color::Rgb`.
- [ ] Destructive actions confirm via the standard dialog
      primitive.
- [ ] State, input, and render live in separate modules.
- [ ] `handle_key` is pure and has unit tests.
- [ ] Render has a snapshot test against `TestBackend` for at
      least the empty and populated cases.
- [ ] If the surface is reachable from a slash command, `/help`
      lists it.

If any box is unchecked, the surface isn't done.
