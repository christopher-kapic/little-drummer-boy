# Codebase Intelligence — Feature Design

This document surveys features from two reference projects —
[**codedb**](./codedb) (Zig, structural indexing + MCP) and
[**SocratiCode**](./SocratiCode) (TypeScript/Node, semantic vector search + AST
graph) — and classifies them against cockpit's design constraints.

**Design constraints that govern every decision here:**

- Rust only — no Node/Bun/Deno runtime, no Docker, no external service required.
- Zero hard system dependencies beyond the binary (rusqlite bundled, rustls).
- Token economy is non-negotiable (GOALS.md §10) — features earn their place by
  measurably reducing model context consumption.
- Primary target: OS models with ~120k context windows, not frontier models.
- Redaction is a chokepoint — nothing from the codebase index crosses the network
  without going through `redact::scrub()`.
- No MCP protocol support (`cockpit mcp` prints a pointer and exits).

---

## Part 1 — Add These

Features with clear ROI, implementable purely in Rust with no new heavy
dependencies, and directly serving the token-economy and OS-model goals.

### 1. `tree` — File Tree with Symbol Counts

**What it does:** Returns a compact directory tree annotated with language,
line count, and symbol count per file. No file content is transmitted.

**Why it belongs:** An agent oriented to a new codebase currently either reads
`ls -R` (raw paths, no signal) or speculatively opens files. A single `tree`
call gives structural orientation at ~50–200 tokens — far cheaper than reading
a README that may not describe the layout at all.

**Implementation:** `walkdir` crate + gitignore filtering via the `ignore` crate
(already in scope per GOALS.md §1e). Symbol counts come from the outline cache
(see §3 below). When the outline index is cold, emit counts as `?` rather than
blocking.

**Source inspiration:** `codedb tree`, `codedb_tree` MCP tool.

---

### 2. `outline` — Structural Symbols per File

**What it does:** Given a file path, returns all top-level symbols — functions,
structs/classes, enums, traits/interfaces, constants, imports — with line
numbers and visibility. No file content.

**Why it belongs:** This is the single highest-leverage token-saving tool in
both reference projects. An agent that wants to understand `src/engine/mod.rs`
currently reads the whole file. An outline call returns the structure in
~100–300 tokens. For a 500-line file with 20 symbols, that's a 15–30× reduction
before a single line of content is read.

**Implementation:** `tree-sitter` Rust bindings with per-language grammar crates.
Supported from day one: Rust, TypeScript/JavaScript, Python, Go, C/C++.
Additional grammars (Ruby, PHP, Zig, etc.) can be added incrementally.
Falls back to a regex-based lightweight scanner for unsupported languages so the
tool never hard-errors on unknown file types.

The outline index is persisted in the cockpit SQLite DB (one row per file, keyed
by path + mtime hash) so repeated calls are O(1) cache hits. The cache is
invalidated by the file watcher.

**Source inspiration:** `codedb outline`/`codedb_outline`, `codebase_symbols`
(SocratiCode).

---

### 3. `symbol_find` — Symbol Definition Lookup

**What it does:** Given a symbol name (exact or prefix), returns all definition
sites across the project — file path, line number, kind (fn/struct/enum/…), and
the parent module or class when applicable.

**Why it belongs:** "Where is `AuthManager` defined?" is among the most common
agent questions on an unfamiliar codebase. Without this tool, the agent either
greps (expensive, noisy) or speculatively opens files (worse). A single
`symbol_find` call resolves the question in one round-trip at near-zero token
cost.

**Implementation:** Built directly on the outline index from §2. The index is a
flat table `(name, kind, file, line, parent)` with a covering index on `name`.
Prefix and case-insensitive queries are supported. No new dependencies.

**Source inspiration:** `codedb find`/`codedb_symbol`, `codebase_symbol`
(SocratiCode).

---

### 4. `search` — Trigram-Accelerated Full-Text Search

**What it does:** Searches file contents for a query string or regex. Returns
matching file paths, line numbers, and surrounding context lines. Respects
gitignore rules and the sensitive-file block list.

**Why it belongs:** Cockpit already has a bash tool that can shell to `rg`, but
that tool has no token budget awareness — it returns raw ripgrep output that the
model must parse. A proper `search` tool returns structured, deduplicated,
budget-capped results and can be called from the tool loop without spawning a
subprocess.

**Implementation phase 1:** Wrap `rg` (or `grep-regex` crate) and post-process
results into a structured JSON response. Cap output at a configurable token
budget (default: 4 000 tokens of results). If results exceed the cap, return
the top N with a "truncated — use a narrower query or add a path filter" message.

**Implementation phase 2 (later):** Build a trigram index (3-byte → file-set
map) in SQLite using a blob-per-trigram scheme. Pre-indexing drops repeated
query latency from ~200ms (rg on a cold FS cache) to ~5ms. codedb benchmarks
this at 538× faster than rg on pre-indexed queries. Worth adding once the basic
tool is working.

**Source inspiration:** `codedb search`/`codedb_search`, `codebase_search`
(SocratiCode BM25 path).

---

### 5. `word` — Exact Identifier Lookup (O(1) Inverted Index)

**What it does:** Given an exact identifier token (function name, constant,
type), returns every file and line that contains it as a word boundary match.
Distinct from `search` in that it uses a pre-built inverted index and returns
results in microseconds regardless of repo size.

**Why it belongs:** "Where is `MAX_RETRIES` used?" is a common, cheap question
that shouldn't trigger a full regex scan. The inverted index makes it O(1) for
exact tokens. Complements `search` (which handles patterns and substrings).

**Implementation:** During outline indexing, tokenize each file into
identifier-boundary tokens and write `(token → [(file, line)])` rows into
SQLite. The index is maintained incrementally by the file watcher. Total storage
for a 100k-line Rust project is roughly 2–5 MB in SQLite.

**Source inspiration:** `codedb word`/`codedb_word`.

---

### 6. `deps` — Reverse Dependency Graph

**What it does:** Given a file path, returns (a) all files it imports and (b)
all files that import it. Optionally traverses N hops for transitive analysis.

**Why it belongs:** This is the "blast radius at file granularity" question:
before editing `src/config/mod.rs`, an agent needs to know that 14 other files
import it. Without this tool, either the agent guesses or does expensive
multi-file reads. A single `deps` call answers the question in one round-trip.

**Implementation:** Imports and `use` declarations are already extracted during
outline parsing (§2). The dependency graph is a second SQLite table
`(importer, importee)`. Queries are simple joins; transitive closure is done
with recursive CTEs. No new dependencies.

**Source inspiration:** `codedb deps`/`codedb_deps`, `codebase_graph_query`
(SocratiCode).

---

### 7. `impact` — Symbol-Level Blast Radius

**What it does:** Given a symbol name (function, method, type), returns every
symbol across the project that calls or references it, grouped by hop distance.
"What breaks if I rename `validate_token`?" answered in one call.

**Why it belongs:** File-level `deps` is useful but coarse. Symbol-level impact
analysis prevents agents from having to read every file that imports a module
to determine whether it actually uses the target symbol. SocratiCode benchmarks
this as the feature that most reduces tool calls on large codebases.

**Implementation:** During outline indexing, record call-site references
(function calls, type uses) extracted by tree-sitter queries. Store as
`(caller_file, caller_line, caller_symbol, callee_symbol)` rows. Blast radius
is a recursive CTE bounded at a configurable hop limit (default: 5). This is
meaningful work but entirely within SQLite and tree-sitter — no new heavy deps.

**Source inspiration:** `codebase_impact` (SocratiCode).

---

### 8. `hot` — Recently Modified Files

**What it does:** Returns the N most recently modified files in the project,
with modification timestamps and change counts since the last session.

**Why it belongs:** Cheap orientation signal. Useful at session start: the agent
can see what changed recently without parsing git log. Also valuable for
"what files am I likely to need?" heuristics when continuing a previous task.

**Implementation:** The file watcher (GOALS.md §1e gitignore integration) already
tracks mtime. `hot` is a SQL query on the watcher's file state table, sorted by
mtime desc. Zero new dependencies.

**Source inspiration:** `codedb hot`/`codedb_hot`.

---

### 9. `read` with Line Ranges

**What it does:** Read a specific line range from a file (e.g., lines 45–120)
rather than the entire file. Includes the file's current content hash for
cache-busting.

**Why it belongs:** Cockpit already has a read tool. The addition here is
structured line-range support so agents can request exactly the function body
they identified via `outline` without pulling in the whole file. A 2000-line
file where the agent needs lines 230–280 should cost ~300 tokens, not ~4000.

**Implementation:** Extend the existing `src/tools/read.rs` with `start_line`
and `end_line` parameters. Read the file, slice the lines, return with a
`{hash, total_lines, returned_range}` header so the model knows its context.

**Source inspiration:** `codedb_read` (line range + hash caching params),
`codebase_search` → targeted read workflow (SocratiCode).

---

### 10. `circular` — Circular Dependency Detection

**What it does:** Scans the dependency graph for import cycles and reports them
as path lists (e.g., `A → B → C → A`).

**Why it belongs:** Circular deps cause subtle bugs, unexpected initialisation
order, and test isolation failures. An agent that's refactoring module structure
needs to know whether a proposed change would create a cycle before making it.
This is a one-query operation on the already-built `deps` graph (§6).

**Implementation:** Johnson's algorithm or a simple DFS cycle-finder on the
SQLite dep graph. O(V+E), runs in milliseconds on any codebase cockpit would
realistically handle.

**Source inspiration:** `codebase_graph_circular` (SocratiCode).

---

## Part 2 — Maybe Add These

Features that have genuine value but require significant new infrastructure,
carry design trade-offs, or are better suited for a later version.

---

### M1. Semantic / Vector Search

**What it does:** Embedding-based search that finds semantically related code
even when the query words don't appear in the source. "authentication middleware"
finds `src/auth/jwt_guard.rs` even if that string never appears there.

**Why it's compelling:** SocratiCode's headline feature. Benchmarked at 61% less
context and 84% fewer tool calls vs grep-based exploration on the VS Code
codebase (2.45M lines). The hybrid RRF fusion (dense semantic + BM25 keyword)
is genuinely better than either alone.

**Why it's deferred:**
- Requires an embedding model. The two realistic options are (a) local Ollama
  (adds a native process dep, GPU-dependent quality) or (b) a cloud embedding
  API (adds network calls and an API key requirement — tension with privacy
  posture and redaction guarantees).
- `sqlite-vec` (a loadable SQLite extension for vector search) would fit the
  zero-system-deps philosophy but is still early-stage and requires bundling
  the extension into the binary.
- A simpler path exists first: `tree-sitter`-based structural search (§2–§7)
  already covers the vast majority of agent queries without vectors. Add vectors
  when the structural tools have proven insufficient.

**Recommendation:** Revisit after v1 structural tools ship. If a clean
`sqlite-vec` + local-model path exists at that point, build it as an opt-in
feature behind `extended.intelligence.semantic = true`.

---

### M2. Call Flow Tracing (`flow`)

**What it does:** Given an entry point symbol (or auto-detected entry points),
traces the forward call graph — what this function calls, what those functions
call, down to a configurable depth. Answers "what does this code actually do?"

**Why it's compelling:** Extremely useful for onboarding to a service — one call
to `flow main` reveals the entire request handling chain without reading dozens
of files.

**Why it's deferred:** Requires a complete, accurate call graph which in turn
requires resolving dynamic dispatch, trait objects, closures, and indirect calls
— all hard problems that tree-sitter alone can't fully solve. A partial call
graph that silently misses virtual calls is worse than no graph at all.

**Recommendation:** Build `impact` (§7) first, which uses the same data but only
requires tracking call-sites. Once that's proven, extend to forward tracing.

---

### M3. Context Artifacts

**What it does:** Index non-code project knowledge — database schemas, OpenAPI
specs, Terraform configs, architecture docs — and make them searchable alongside
code.

**Why it's compelling:** An agent working on a feature that touches the database
schema or an external API needs that context. Without it, it either hallucinates
the schema or reads large files speculatively.

**Why it's deferred:** Requires a per-project config file
(`.cockpit/artifacts.toml`) listing artifact paths, then a chunking + indexing
pipeline separate from the code pipeline. This is straightforward but adds
surface area. It also pairs naturally with semantic search (§M1) — artifact text
chunks are most useful when vector-searched, not just BM25'd.

**Recommendation:** Design the config schema now so it's in the cockpit config
layer. Defer indexing until semantic search is available.

---

### M4. Graph Visualization (Mermaid / HTML)

**What it does:** Renders the dependency graph as a Mermaid diagram (text,
embeddable in chat) or an interactive HTML file (for browser viewing).

**Why it's compelling:** Visual graph navigation catches architectural problems
quickly. The interactive HTML version with blast-radius overlay and PNG export
(SocratiCode's `codebase_graph_visualize mode="interactive"`) is genuinely useful
for design review sessions.

**Why it's deferred:** cockpit is a TUI. Mermaid text output in the chat surface
works fine and is low-cost to add (just render the dep graph as Mermaid syntax).
The interactive HTML requires opening a browser from the TUI, which is
plausible (`xdg-open` / `open`) but awkward on headless SSH sessions — which
cockpit explicitly optimises for (GOALS.md §4, "data-efficient over SSH").

**Recommendation:** Add a `cockpit graph --mermaid` CLI subcommand (not a TUI
tool) that writes a Mermaid diagram to stdout or a file. The TUI tool can emit
a compact Mermaid block in the chat surface. Interactive HTML viewer deferred
until a browser-facing surface exists (the future `cockpit connect` dashboard).

---

### M5. Resumable / Batched Indexing

**What it does:** Checkpoint indexing progress to disk after each batch so that
a crash or interrupt resumes from where it left off rather than re-indexing from
scratch.

**Why it's compelling:** Critical for very large codebases (>1M lines). On a
10M-line monorepo, a 20-minute interrupted index that has to restart from zero
is a severe UX problem.

**Why it's deferred:** For cockpit's primary targets (typical open-source
projects, not enterprise monorepos), SQLite-backed incremental indexing (re-index
only changed files, tracked by mtime hash) already covers the common case. True
batch-checkpoint resumability adds meaningful complexity. The cockpit session DB
can store indexing progress state at modest implementation cost when needed.

**Recommendation:** Implement mtime-based incremental re-index first. Add batch
checkpointing when there's evidence of users hitting the 1M+ line threshold.

---

### M6. Multi-Project / Cross-Project Search

**What it does:** Maintain indexes for multiple projects simultaneously and
optionally search across all of them in a single query.

**Why it's compelling:** Monorepo-adjacent setups, shared library repos, and
microservice workflows all benefit from cross-project symbol search.

**Why it's deferred:** The single-project case needs to work well first. The
SQLite schema should be designed from the start to be project-scoped (prefix all
tables with a project ID or use separate database files) so multi-project is
an additive change. But building the query layer, result merging, and
project discovery UI is non-trivial work.

**Recommendation:** Design the storage schema to be multi-project-ready from
day one. Implement cross-project queries in v2.

---

### M7. Branch-Aware Indexing

**What it does:** Maintain separate indexes per git branch. Switching branches
(via git checkout hook) automatically activates the correct index.

**Why it's compelling:** PR review workflows, feature branch development, and
diff-based planning all benefit from a branch-specific index.

**Why it's deferred:** Requires git hook integration (pre-checkout / post-checkout)
and per-branch index storage that can grow large on repos with many branches.
The index invalidation strategy on merge/rebase is non-trivial.

**Recommendation:** Add to v2 design alongside multi-project support (§M6) —
they share the project-scoped storage model.

---

## Implementation Roadmap

### Phase 1 — Structural foundation (v1)

Ship in order:

1. Outline index (`tree-sitter` + SQLite cache) — §2
2. `tree` tool — §1 (depends on outline index for symbol counts)
3. `symbol_find` tool — §3 (free given outline index)
4. `word` inverted index + tool — §5
5. `deps` graph + tool — §6 (import data from outline)
6. `read` line-range extension — §9
7. `hot` tool — §8
8. `circular` detection — §10 (free given deps graph)
9. `search` tool (rg wrapper with budget cap) — §4 phase 1

### Phase 2 — Impact analysis + search quality

10. `impact` symbol blast radius — §7
11. Trigram index for `search` — §4 phase 2
12. Mermaid graph output — §M4 (quick win once dep graph exists)

### Phase 3 — Evaluate and extend

13. Semantic search decision point — §M1
14. Context artifacts (if semantic search is viable) — §M3
15. Call flow tracing — §M2
16. Multi-project + branch-aware — §M6, §M7

---

## Key Technology Decisions

| Need | Technology | Rationale |
|------|-----------|-----------|
| Structural parsing | `tree-sitter` + Rust bindings | Same quality as codedb's per-language parsers and SocratiCode's ast-grep; native Rust; no subprocess |
| Index storage | `rusqlite` (already bundled) | Zero new dep; recursive CTEs handle graph traversal; FTS5 available for BM25 search later |
| Gitignore filtering | `ignore` crate (already in scope) | Correct handling of nested ignore files, negation patterns, `core.excludesfile`; don't roll our own |
| Full-text search | `rg` subprocess → `grep-regex` crate | Start simple; trigram index layer added later |
| File watching | `notify` crate or polling | Notify for inotify/FSEvents/kqueue; polling fallback for remote FS |
| Semantic search (deferred) | `sqlite-vec` + local embedding | Keeps zero-external-process constraint if/when the extension matures |

---

## What We Explicitly Don't Take

| Feature | Source | Reason |
|---------|--------|--------|
| Docker/Qdrant container management | SocratiCode | Violates zero-system-deps constraint |
| Ollama/cloud embedding by default | SocratiCode | Adds process/API dependency; privacy risk before redact layer covers it |
| MCP protocol layer | Both | GOALS.md: `cockpit mcp` exits with a pointer |
| `codedb_remote` cloud API | codedb | External service; data-privacy tension; out of scope |
| Telemetry | codedb | Cockpit's redaction posture; no opt-out by design for the user's code |
| Node/npm runtime | SocratiCode | GOALS.md: no JS runtime required at runtime |
| SocratiCode Cloud / shared team index | SocratiCode | Out of scope; future `cockpit connect` is the v2 multi-user surface |
| Multi-agent shared index coordination | SocratiCode | v2+ concern; single-agent v1 first |
