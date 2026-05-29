# Inject harness identity, version, URLs, and user name into the cached system block

## Goal

Tell the model what harness it's running in. Add harness identity, the
running cockpit version, the cockpit URLs, and (when configured) the
user's name to the cached system block that every built-in agent
receives.

## Current behavior

`compose_system_prompt()` (`src/engine/builtin/mod.rs`, ~line 57) builds
each agent's system prompt as: the role prompt, then an
`Operating system: <uname>` line, then an optional `Session: <id>` line
(omitted when the id is empty), then any injected project-guidance file
(`AGENTS.md`/`CLAUDE.md`).

The model is therefore never told it is running inside cockpit, what
version it is, or who the user is — it could self-identify as a generic
assistant or the wrong harness.

## Desired behavior

Prepend three new lines to the per-session block. These are **stable for
the session lifetime**, so per GOALS §17g they belong in the **cached**
system block (alongside the existing OS/Session lines) — **not** appended
to user messages like the volatile time prelude.

Add, in this order, before the existing `Operating system:` line:

1. `Harness: cockpit <version>` — version from
   `env!("CARGO_PKG_VERSION")`.
2. `Website: https://flycockpit.dev | App: https://app.flycockpit.dev`
   — **both** URLs (decided; the marketing site and the web app).
3. `User: <name>` — **only when** the user's display name is set;
   omit the entire line when unset.

So the full appended block becomes (name present):

```
Harness: cockpit 0.1.1
Website: https://flycockpit.dev | App: https://app.flycockpit.dev
User: Christopher
Operating system: Linux 6.8.0 x86_64
Session: a1b2c3
```

And with no name configured, the `User:` line is simply absent.

## Edge cases & decisions (settled)

- **User name source.** Reuse the existing `extended.name:
  Option<String>` field (`src/config/extended.rs`, ~line 44) — the same
  one that drives the `Welcome, {name}` startup logo. Do **not** add a
  new config field.
- **Where to read it.** `compose_system_prompt()` already loads the
  layered `extended-config.json` for guidance-file discovery (via
  `load_agent_guidance` → `discover_config_dirs` →
  `ExtendedConfigDoc::load`). Read `name` from that same already-loaded
  config inside `compose_system_prompt` rather than threading a new
  field through `SpawnArgs`. (If a small refactor of `load_agent_guidance`
  is needed so the loaded config is available for both the name and the
  guidance file without loading it twice, do that cleanly.)
- **Name-absent.** When `name` is `None` (or empty after trimming),
  omit the `User:` line entirely — mirror the existing empty-`Session:`
  omission. Trim whitespace before deciding.
- **Determinism / cache-safety.** The line order must be fixed and
  stable across runs (same inputs → byte-identical block) so the
  prompt cache is not disturbed. Use the order shown above.
- **Both URLs are intentional.** This was an explicit user decision
  over a token-economy objection — keep both; do not trim to one.
- **Terseness.** Keep the wording exactly as specified; no extra prose,
  labels, or punctuation beyond the lines above.

## Tests

Update the existing `compose_system_prompt_*` unit tests in
`src/engine/builtin/mod.rs`:

- Assert the block contains `Harness: cockpit ` followed by the
  `CARGO_PKG_VERSION` value (compare against `env!("CARGO_PKG_VERSION")`,
  not a hardcoded string).
- Assert it contains both URLs
  (`https://flycockpit.dev` and `https://app.flycockpit.dev`).
- Add/extend a case where the loaded config has a `name` set →
  `User: <name>` present.
- Add/extend a case where no `name` is set → no `User:` line.
- Keep the existing OS / Session / guidance assertions passing.

If a test needs a configured `name`, write a minimal
`extended-config.json` into the temp `cwd` the test already uses (the
tests construct a `tempdir`), so the layered loader picks it up — don't
mock around the real load path.

## Docs

Update **GOALS.md §17g** to document that the cached system block now
includes: harness identity, running version, both cockpit URLs
(`flycockpit.dev` + `app.flycockpit.dev`), and the optional configured
user name — in addition to the existing OS and session-id lines.

## Acceptance

- A built-in agent's system prompt opens its per-session block with the
  `Harness:`, `Website:/App:`, and (when configured) `User:` lines, then
  the existing `Operating system:` / `Session:` lines.
- No `User:` line when `extended.name` is unset.
- Version reflects the actual build (`CARGO_PKG_VERSION`).
- GOALS §17g reflects the new fields.
- `cargo fmt --check`, `cargo build`, `cargo test`, and
  `cargo clippy -- -D warnings` (no *new* errors) all pass.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise (none expected here).
- Verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring anything new in.
- Honor the project invariants in `CLAUDE.md`: token economy (terse),
  cached-prefix stability (deterministic ordering), reuse existing
  machinery rather than adding a parallel config-load path.
- Touch only files needed for this feature.
