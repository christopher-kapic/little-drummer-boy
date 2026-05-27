# Config reconciliation: `config.json` + `extended-config.json` → one file

## TL;DR

`extended-config.json` is a vestige of the dropped opencode-compat layer
(`GOALS.md` §2a). The plan of record is to fold every key it carries into
`config.json` under top-level namespaces, delete `ExtendedConfigDoc`, and
keep a one-cockpit-version legacy reader so existing on-disk files don't
get orphaned. After that, every cockpit config layer is a single file:
`.cockpit/config.json`.

This document is the migration plan. It is not implemented yet.

---

## Why the split exists today

The original design assumed `opencode.json` would be the base layer
(byte-compatible with opencode) and `extended-config.json` would be the
cockpit-only superset layered on top. With opencode-compat dropped
(`CLAUDE.md` "Design rules"), the rationale evaporated, but the code
shipped the split anyway:

- `src/config/providers.rs::ConfigDoc` — round-trips `config.json` for
  `providers`, `on_unlisted_models_fetch`, `active_model`.
- `src/config/extended.rs::ExtendedConfigDoc` — round-trips
  `extended-config.json` for everything else (harnesses, tui, redact,
  agent_guidance_files, tools, utility_model, prompt_injection_guard,
  system_prompt, …).

Both files live side-by-side in each `.cockpit/` directory. Callers
load each independently and tolerate either being missing.

---

## Target layout

A single `.cockpit/config.json` per layer:

```jsonc
{
  "providers":             { /* ProvidersConfig — unchanged */ },
  "on_unlisted_models_fetch": "ask",
  "active_model":          { "provider": "…", "model": "…" },

  "harnesses":             { /* HarnessConfig map */ },
  "agent_guidance_files":  ["AGENTS.md", "CLAUDE.md"],
  "default_delegation":    "subagent",
  "agent_dirs":            [],
  "agents":                { "docs_dir": "~/packages" },
  "redact":                { /* RedactConfig */ },
  "tui":                   { /* TuiConfig — vim_mode, thinking, … */ },
  "composer":              { "tagging": { /* … */ } },
  "name":                  null,
  "packages_directory":    null,
  "tools":                 { /* user-defined bash tools */ },
  "allow_remote_config":   false,
  "utility_model":         null,
  "prompt_injection_guard":{ "enabled": false, "model": null },
  "system_prompt":         { "time_injection_interval_minutes": 5 }
}
```

This is exactly what `GOALS.md` §4 already documents — none of the
schema field names change. The only difference is that the keys move
from a separate file into the same JSON object as `providers`.

There is no `extended.*` wrapper namespace. Each top-level key from
`ExtendedConfig` becomes a top-level key of `config.json` directly.
This matches §4 and avoids deepening the path for every UI surface
that already references e.g. `tui.vim_mode`.

---

## Migration plan

### 1. Unify the schema in code

Collapse `ProvidersConfig` and `ExtendedConfig` into a single
`Config` struct (working name) in a new module `src/config/file.rs`,
backed by `ConfigDoc` (the existing raw-`Value` round-tripper). The
new shape:

```rust
pub struct Config {
    pub providers: BTreeMap<String, ProviderEntry>,
    pub on_unlisted_models_fetch: Option<OnUnlistedModelsFetch>,
    pub active_model: Option<ActiveModelRef>,

    pub harnesses: HashMap<String, HarnessConfig>,
    pub agent_guidance_files: Vec<String>,
    pub concurrency: Concurrency,
    pub agent_dirs: Vec<PathBuf>,
    pub redact: RedactConfig,
    pub tui: TuiConfig,
    pub name: Option<String>,
    pub packages_directory: Option<PathBuf>,
    pub tools: HashMap<String, ToolCommandTemplate>,
    pub allow_remote_config: bool,
    pub utility_model: Option<String>,
    pub prompt_injection_guard: PromptInjectionGuardConfig,
    pub system_prompt: SystemPromptConfig,
}
```

The inner types (`HarnessConfig`, `RedactConfig`, `TuiConfig`, …)
move from `extended.rs` to a shared module but are otherwise
unchanged. Their `#[serde(default)]` attributes already make every
field optional, so omitting them from on-disk JSON degrades cleanly.

`ConfigDoc::load`, `ConfigDoc::write`, and the `raw: Value` field
remain — preserving unknown root keys across writes is a real
property we want to keep (cockpit upgrades, third-party tooling
that adds keys, etc.).

### 2. Legacy reader for `extended-config.json`

For one cockpit release after this change ships, `ConfigDoc::load`
also reads `extended-config.json` from the same directory and
merges its keys into the in-memory `Config` only if they are not
already present in `config.json`. (`config.json` always wins for
the keys it owns.) The legacy reader logs at `WARN` when it pulls
fields from `extended-config.json`:

```
.cockpit/extended-config.json is deprecated; run `cockpit config migrate`
to fold it into config.json. Cockpit will stop reading this file in
the next release.
```

`ConfigDoc::write` always writes to `config.json` only — the
in-memory merge means the next save folds the legacy data in
naturally. After write, the legacy file is **not** deleted
automatically. A separate `cockpit config migrate` command removes
it after confirming `config.json` round-trips identically.

### 3. `cockpit config migrate` (new subcommand)

```
cockpit config migrate [--dry-run] [--keep-legacy]
```

For each discovered config layer (per `discover_config_dirs`):

1. Load `config.json` and `extended-config.json` (either may be
   absent).
2. Merge: `extended-config.json` keys folded into `config.json`
   only where `config.json` doesn't already define the key
   (`config.json` wins on conflict — matches the legacy reader's
   semantics so the migration is a no-op for users who haven't
   double-written by hand).
3. Re-serialize and write `config.json` (single atomic
   `write` → `rename`).
4. Verify the rewrite round-trips: load again, compare structural
   equality against the in-memory `Config` we just wrote. Bail if
   not.
5. Delete `extended-config.json` (unless `--keep-legacy` or
   `--dry-run`).

`--dry-run` prints the diff (the new `config.json` body and the
list of files that would be removed) without writing anything.

### 4. Caller refactor

These call sites collapse to a single `ConfigDoc::load` plus typed
access:

- `src/welcome.rs::user_display_name` / `banner_enabled_from_config`
- `src/auto_title.rs::load_configs_for`
- `src/tui/app.rs::diff_style_from_config` (lines ~2666–2674)
- `src/tui/settings.rs` — `SettingsDialog` drops `extended_path`,
  `extended`, `reload_extended`, `save_extended`; every page edits
  the unified `Config` and the dialog persists once.
- `src/daemon/server.rs::load_configs` returns `Config` instead of
  the `(ProvidersConfig, ExtendedConfig)` tuple.

The settings dialog UI doesn't visibly change — the rows it draws
already reference the same fields under the same names.

### 5. Update `extended-config.json` references in code and docs

- Delete `src/config/extended.rs` (the file) once nothing imports
  `ExtendedConfigDoc`. Move the types it owned (`RedactConfig`,
  `TuiConfig`, `HarnessConfig`, `ToolCommandTemplate`,
  `VimModeSetting`, `ThinkingDisplay`, `DiffStyle`,
  `BannerConfig`, `PromptInjectionGuardConfig`,
  `SystemPromptConfig`) into `src/config/types.rs` or fold them
  into `file.rs`.
- Strip the file-path mentions from `src/cli.rs` (`init`
  description / `--force` flag doc), `welcome.rs`, `tools/custom.rs`,
  `tui/settings.rs` doc comments.
- `GOALS.md` §2a — change "Migration: trivial; we weren't shipping
  `extended-config.json` separately yet" to point at this plan
  (since we *did* ship it).
- `GOALS.md` §4 already describes the unified schema, so no edit
  needed there.
- `CLAUDE.md` mentions `extended-config.json` in the project-structure
  block under `src/config/` — update once `extended.rs` is gone.
- `miscellaneous.md` — sweep for any mention.

### 6. Scaffolding behavior

`src/config/dirs.rs::scaffold_config_dir` writes a minimal
`config.json` today and nothing else; after the migration the file
already contains every namespace's defaults implicitly (via
serde defaults), so no change is needed. A future enhancement
could write a commented JSONC stub that mirrors the §4 schema as
documentation, but that's out of scope here.

---

## Backwards compatibility

| Scenario                                | Behavior with this plan |
|----------------------------------------|-------------------------|
| User has only `config.json`            | Unchanged. |
| User has only `extended-config.json`   | Read by the legacy reader; `cockpit config migrate` folds it in. |
| User has both                          | Loaded as a merge: `config.json` wins on overlap. `migrate` writes the merged result back to `config.json` only. |
| User has neither                       | Unchanged — defaults via serde. |
| Third-party tool wrote unknown keys    | Preserved by `ConfigDoc`'s raw `Value` field, same as today. |

Two cockpit releases after the migration ships, the legacy reader
is removed and `extended-config.json` is silently ignored. A WARN
log fires on every load that still finds the file, telling the
user to run `cockpit config migrate`.

---

## Tests

- `config::file::tests::loads_legacy_extended_when_config_missing_those_keys`
  — `.cockpit/config.json` has only `providers`,
  `extended-config.json` has `tui.vim_mode = "enabled"`, the
  resulting `Config` carries both.
- `config::file::tests::config_wins_over_extended_on_overlap` —
  both files set `tui.vim_mode` to different values; the
  `config.json` value wins.
- `config::file::tests::migrate_round_trips_unknown_keys` —
  arbitrary `"future_feature": {...}` keys in either file survive
  the migration.
- `config::file::tests::migrate_is_idempotent` — running the
  migrator twice produces no further changes.
- `commands::config_migrate::tests::dry_run_writes_nothing` —
  no-op on disk; the diff goes to stdout.
- `tui::settings::tests::unified_config_roundtrip` — the settings
  dialog can read, mutate, and save every field that previously
  lived in `extended-config.json` after the collapse, and the
  resulting file has them at the top level.

---

## Out of scope (deliberately)

- Reshaping the §4 schema. Keys keep the names they already have.
- Changing the merge-mode taxonomy (§2b). That stays as-is.
- The `/config` TUI tabbed layer view (`GOALS.md` §2c). It's
  orthogonal — once `Config` is unified, the tabbed view becomes
  easier to build, but it's a separate change.
- Daemon wire-format changes. The daemon already returns typed
  config to the TUI; the wire is unaffected by collapsing the
  on-disk representation.
- A YAML/TOML alternative. JSON stays.

---

## Risks

1. **A user has manually written settings to *both* files,
   inconsistently.** The legacy-reader rule (`config.json` wins)
   makes this deterministic, but a user expecting
   `extended-config.json` to override may be surprised. Mitigation:
   the migrate command's `--dry-run` output flags every key where
   the two files disagree and shows which value will win.

2. **Other tools (rare but possible) read `extended-config.json`
   directly.** No such tools exist that we know of. If any are
   discovered before the legacy reader is removed, point them at
   the unified file.

3. **The settings dialog currently saves the two halves
   independently** (a Providers edit only touches `config.json`,
   a UI edit only touches `extended-config.json`). After the
   collapse, every save rewrites the whole `config.json`. That's
   fine — the file is small and `ConfigDoc::write` is
   already pretty-print round-tripped — but it does mean a save
   that races with an external editor could clobber non-cockpit
   keys *if* they were added between load and save. The raw-`Value`
   preservation in `ConfigDoc` mitigates this for unknown keys;
   for known keys we'd need a fine-grained reload-merge-save flow,
   which is a separate concern not introduced by this collapse.

---

## Rollout

1. PR 1: introduce `Config` and the legacy reader; keep
   `ExtendedConfigDoc` callable so the diff is contained. Land
   tests. **No on-disk change** — both files still readable.
2. PR 2: switch every caller from `ExtendedConfigDoc` to
   `ConfigDoc`. Remove `ExtendedConfigDoc`. Delete `extended.rs`
   once unused. Update doc comments.
3. PR 3: add `cockpit config migrate`. Print a one-line hint at
   TUI startup when `extended-config.json` is detected.
4. One release later: remove the legacy reader. Loading
   `extended-config.json` becomes a WARN with no effect; users
   who haven't run `migrate` lose their non-providers settings
   to defaults until they do.
