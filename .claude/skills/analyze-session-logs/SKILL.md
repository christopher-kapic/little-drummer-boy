---
name: analyze-session-log
description: Audit a cockpit session export (events.json + inference_requests/) for behavior, safety, design-rule, and token-economy problems, then produce a prioritized findings report. Use when the user wants to analyze/review/diagnose an exported session log or understand how a model behaved in a session.
---

# Analyze a cockpit session log

A cockpit session export is a directory (produced by `cockpit export`,
unzipped) holding the full record of one session. Your job is to read it,
reconstruct what happened, and report **problems** — model misbehavior,
safety gaps, violations of cockpit's own design rules, and token waste —
ranked by severity. This is a *diagnostic*, not a summary: lead with what
is wrong and why it matters, cite event `seq` numbers, and back claims
with numbers.

Prefer **`jq`** for every query below. Only fall back to Python (see
[Python fallback](#python-fallback)) if `jq` is not installed
(`command -v jq`).

## Export layout

```
<export-dir>/
  manifest.json                         # session metadata + fork tree (may be absent in older exports)
  events.json                           # JSON ARRAY of every event, in seq order
  inference_requests/
    {seq}_{short_id}_{call_id}.json     # one file per LLM request: full system+history+prompt+tools
```

`events.json` — array of objects. Common fields: `seq`, `ts_ms`,
`type`, `agent`, `session_id`, `short_id`, `call_id`, `data`.
`type` is one of:

| type | key `data` fields |
|------|-------------------|
| `user_message` | `text` |
| `assistant_message` | `text` |
| `tool_call` | `tool`, `wire_input`, `original_input`, `output`, `duration_ms`, `hard_fail`, `truncated`, `recovery_kind`, `recovery_stage` |
| `inference_request` | (pointer; the full payload is the matching file under `inference_requests/`) |

`wire_input` is what the model actually sent (post-repair); `original_input`
is the raw model output before the repair layer (GOALS §12, §14). Tool
output embeds the exit code as the text `exit: <N>`.

Each `inference_requests/*.json`: `model`, `provider`, `params`, `system`
(full system prompt), `history` (prior turns), `prompt` (this turn),
`tools` (advertised tool schemas).

Run all commands below from inside `<export-dir>`.

## Step 1 — Inventory

```bash
# Event-type counts
jq -r 'group_by(.type)[] | "\(length)\t\(.[0].type)"' events.json

# Agents that appear, session id(s)
jq -r '[.[].agent] | group_by(.)[] | "\(length)\t\(.[0])"' events.json
jq -r '[.[].session_id] | unique[]' events.json

# Wall-clock span and duration
jq '[.[].ts_ms] | {start_ms:min, end_ms:max, duration_min:((max-min)/60000)}' events.json
```

Note the model up front — it frames every judgement. cockpit's primary
target is open-source ~120k-context models (CLAUDE.md priority #1), so a
small model thrashing is a *harness* finding, not just "the model is dumb":

```bash
jq -r '.model' "$(ls inference_requests/*.json | sort | head -1)"
```

## Step 2 — Reconstruct the conversation arc

```bash
jq -r '.[] | select(.type=="user_message" or .type=="assistant_message")
  | "[\(.seq)] \(.type|ascii_upcase): \(.data.text|gsub("\n";" ")[0:200])"' events.json
```

Read it as a narrative: what did the user actually ask, and did the
session ever accomplish it? Flag **non-convergence** — many turns, no
resolution, user goal never met.

## Step 3 — Tool usage and delegation

```bash
# Tool-name histogram
jq -r '[.[]|select(.type=="tool_call")|.data.tool] | group_by(.)[]
  | "\(length)\t\(.[0])"' events.json

# Every bash command with exit code + hard_fail
jq -r '.[] | select(.type=="tool_call" and .data.tool=="bash")
  | "seq\(.seq)\texit=\(.data.output|capture("exit: (?<e>-?[0-9]+)").e // "?")\thard_fail=\(.data.hard_fail)\t\(.data.wire_input.command|gsub("\n";" ")[0:140])"' events.json
```

**Delegation check (design-rule):** the bundled cast is
`orchestrator-build`, `orchestrator-plan`, `explore`, `coder`, `docs`, and
**only `coder` writes/edits and holds locks** (CLAUDE.md "Multi-agent file
locking, single writer"). An orchestrator's job is to delegate via `task`.
So:

```bash
# How many task delegations? Zero from an orchestrator that did lots of bash = red flag.
jq '[.[]|select(.type=="tool_call" and .data.tool=="task")] | length' events.json
```

If a non-`coder` agent ran `bash` heavily and never called `task`, the
delegation architecture was bypassed — call it out.

## Step 4 — Failure and thrash signals

```bash
# Repeated identical bash commands (retrying the same failing thing)
jq -r '[.[]|select(.type=="tool_call" and .data.tool=="bash")|.data.wire_input.command]
  | group_by(.)[] | select(length>1) | "\(length)x  \(.[0]|gsub("\n";" ")[0:120])"' events.json

# Nonzero exits
jq -r '.[] | select(.type=="tool_call" and .data.tool=="bash")
  | select((.data.output|capture("exit: (?<e>-?[0-9]+)").e // "0") != "0")
  | "seq\(.seq)\texit=\(.data.output|capture("exit: (?<e>-?[0-9]+)").e)\t\(.data.wire_input.command[0:120])"' events.json

# Repair-layer activity (model emitted malformed tool input)
jq -r '.[] | select(.type=="tool_call" and (.data.recovery_kind!=null))
  | "seq\(.seq)\t\(.data.tool)\t\(.data.recovery_kind)/\(.data.recovery_stage)"' events.json
```

Other things to watch for by skimming the bash list and arc:
- **Recursive self-invocation** — the harness running its own binary
  (`cockpit run`, `cockpit daemon`, …) inside a session; risks nested
  daemons / socket contention.
- **Stale-binary fallback** — `cargo run` blocked, so the agent runs a
  previously-installed binary (`~/.cargo/bin/cockpit`); every later
  conclusion about behavior is then drawn from stale code, not the
  working tree.
- **`hard_fail=false` on genuinely failed commands** — nonzero exits
  (126, 64, 1) not surfaced as failures, so the model barrels past them.

## Step 5 — Safety / escalation

The user cares a lot about whether dangerous actions ran without a gate.

```bash
# Escalation / destructive verbs (high signal)
jq -r '.[] | select(.type=="tool_call" and .data.tool=="bash")
  | select(.data.wire_input.command
      | test("\\bsudo\\b|\\bchmod\\b|\\bchown\\b|\\brm +-[rf]|\\bmkfs|\\bdd +if=|\\b777\\b|\\bgit +(push|reset +--hard|clean +-[a-z]*f)|\\bkillall\\b|\\bkill +-9"))
  | "seq\(.seq)\t\(.data.wire_input.command|gsub("\n";" ")[0:160])"' events.json

# Filesystem writes via bash (single-writer violation if agent != coder).
# Excludes 2>/dev/null noise; eyeball results — heredocs/redirects can false-positive.
jq -r '.[] | select(.type=="tool_call" and .data.tool=="bash")
  | select((.data.wire_input.command|test("\\b(mkdir|touch|tee|mv|cp|rm)\\b|[^&0-9]> "))
           and (.data.wire_input.command|test("/dev/null")|not))
  | "seq\(.seq)\t\(.data.wire_input.command|gsub("\n";" ")[0:160])"' events.json
```

Treat as findings: `sudo`, `chmod 777`/world-writable perms, hand-editing
or seeding the production DB, creating a regular file where a Unix socket
must bind, any write by a non-`coder` agent — **especially** if no
approval/permission gate is visible in the event stream.

## Step 6 — Token economy

Token economy is "non-negotiable" (CLAUDE.md priority #2): base system
prompt budget is **~400 tokens**; CI is supposed to fail past that.

**Use real usage, never char counts, and never sum per-request payloads.**
Every `inference_request` event in `events.json` carries `data.usage`
(`input_tokens`, `cached_input_tokens`, `output_tokens`). Read those.

```bash
# Real per-session token aggregates
jq -r '[.[] | select(.type=="inference_request") | .data.usage] as $u | {
    requests: ($u|length),
    billed_input_tokens: ($u|map(.input_tokens)|add),    # provider re-processes the prefix each call
    cached_input_tokens: ($u|map(.cached_input_tokens)|add),
    output_tokens: ($u|map(.output_tokens)|add),         # what the model actually generated
    peak_context_tokens: ($u|map(.input_tokens)|max),    # high-water mark = how full the window got
    cache_hit_rate: (($u|map(.cached_input_tokens)|add)
                     / (($u|map(.input_tokens + .cached_input_tokens)|add) // 1))
  }' events.json
```

These are **three different numbers answering three different questions** —
report whichever the user cares about; do not collapse them into one
"total":

- **Peak context** (`peak_context_tokens`, ≈ the last request before a
  reset) — how close the session came to the context limit. Right number
  for "did we blow the window." Does **not** grow with request count.
- **Billed / processed input** (Σ `input_tokens`, minus cache hits) — the
  cost axis. This *does* sum across requests, because the provider
  re-processes the prefix on every call unless it's a cache hit.
- **Output** (Σ `output_tokens`) — what the model wrote, usually tiny next
  to input.

> **Never sum per-request payload bytes to get a "session total."** Each
> request's `history` re-contains every prior turn, so summing payloads
> counts the same tokens over and over (prefix-sum inflation). That
> double-counting comes from **accumulation, not caching** — it's wrong even
> at a 0% cache rate. Caching is a separate, *cost-only* lever: it lowers
> billed input, not the amount of unique context. Keep the two axes
> distinct.

**Sessions with resets (prune / compaction / subagent).** History isn't
always monotonic — a compaction or prune drops it, and each subagent runs
its own fresh history. So "unique context volume" ≈ the sum of each
*segment's* peak, not the global peak. Find the reset points (where
`input_tokens` falls vs the prior request):

```bash
jq -r '[.[] | select(.type=="inference_request") | {seq, in:.data.usage.input_tokens}] as $a
  | range(1; ($a|length)) as $i
  | select($a[$i].in < $a[$i-1].in)
  | "reset at seq\($a[$i].seq): \($a[$i-1].in) -> \($a[$i].in)"' events.json
```

Per-request system-prompt size, and whether a large doc got embedded
verbatim and re-sent every call (`approx_system_tokens` via char/4 is fine
here — this is a per-request *size* check, not a session total):

```bash
first=$(ls inference_requests/*.json | sort | head -1)
jq -r '{model, system_chars:(.system|length), approx_system_tokens:((.system|length)/4|floor),
        claude_md_embedded:(.system|contains("agent guide")), n_tools:(.tools|length)}' "$first"
```

Findings to compute: system-prompt tokens vs the ~400 budget; whether
CLAUDE.md (or any large doc) is embedded in `.system` and re-sent on every
request (`system_chars` × request count); peak context vs the model's window;
billed input vs peak (a huge gap with `cache_hit_rate == 0` means the prefix
was re-processed every call uncached — a finding in itself).

## Step 7 — Redaction / secret-leak scan

Redaction is a non-bypassable chokepoint (CLAUDE.md / GOALS §7). Spot-check
that no secrets rode along in the outbound payloads:

```bash
grep -rEi 'api[_-]?key|secret|bearer |password|AKIA[0-9A-Z]{16}|sk-[A-Za-z0-9]{20,}|-----BEGIN [A-Z ]*PRIVATE KEY' inference_requests/ | head
```

Home paths (`/Users/<name>/…`) appearing is expected and not a leak. Real
secrets (live keys, tokens, private-key blocks) are.

## Step 8 — Meta-analysis (the part that compounds)

The point of this skill isn't one report — it's that you, cockpit, and the
user get sharper with every log. So **every** analysis ends with two
buckets of improvement suggestions. Both must be **grounded in friction you
actually hit this run** — not a generic wishlist. If a run produced no real
friction in a bucket, say "nothing this run" rather than inventing items.

**(a) What cockpit could add to the export to make analysis sharper.**
Track, as you work through steps 1–7, every place you had to *infer*,
*regex-scrape*, or *guess* something the harness could have recorded
directly. Each becomes a suggestion. Patterns seen so far:
- A field you had to parse out of free text → ask for it structured. (E.g.
  exit codes are scraped from `output` via `exit: N`; a real `exit_code`
  field would be unambiguous.)
- A judgement you could only make from *absence* of data → ask for an
  explicit event. (E.g. you can't prove a dangerous command was *ungated*;
  only that no approval event exists. An explicit permission-decision event
  — requested / auto-allowed / granted / denied + reason — would make safety
  auditing definitive.)
- Data that lives in the DB but isn't in the export (e.g. `cost_usd_micros`,
  per-call latency, the fork tree from `manifest.json` if absent).
- Linkage you needed but couldn't reconstruct (e.g. a `parent_call_id` /
  delegation-tree edge so subagent work maps to its spawning `task`).
- Things that should be separable but are merged (e.g. framework-injected
  `system-reminder` text living inside a `user_message`, inflating the
  user-message count).
  Frame these as candidates for `GOALS.md` / `plan.md` / the export code —
  don't edit those docs yourself; surface them for the user to graduate.

**(b) What would make this skill better.** Capture what *this* log taught
the skill:
- A recipe that broke or false-positived, and the fix (e.g. the escalation
  regex once matched `2>/dev/null`; tightened to verb-anchored).
- A failure pattern this log exhibited that the skill had no recipe for →
  propose the recipe.
- A recipe that's still **unvalidated** because no log has exercised it yet
  (subagent trees, compaction, repair-layer activity) → note it as untested.
- A heuristic that proved misleading (e.g. char/4 as a token proxy when real
  `usage` exists) → generalize the lesson ("check the schema for a field
  before computing it by hand").

If the user approves a skill improvement, **edit this `SKILL.md` directly**
and append a line to the coverage ledger at the bottom — that's the
mechanism by which the skill compounds. Cockpit-side suggestions are for the
user to act on; don't touch the repo's design docs unprompted.

## Output: prioritized findings report

Write the report straight into the reply (don't create a file unless the
user asks). Open with one or two lines naming the export, the model, event
count, and duration. Then group findings by severity, highest first, e.g.:

- **Architecture violations** — design rules the harness failed to enforce
  (delegation bypassed, non-`coder` writes, single-writer broken).
- **Safety / approval** — ungated dangerous actions.
- **Token economy** — budget blown, doc embedded per-request, runaway
  history.
- **Weak-model behavior the harness should defend against** —
  non-convergence, stale-binary contamination, recursive self-invocation.
- **Minor** — redundant tokens, tool-description nits, mislabeled failures.

Each finding: one line stating the problem, a `seq`/number citation, and a
short why-it-matters tied to a cockpit priority or design rule.

Then a final **Meta-analysis** section (Step 8) with the two buckets:
*(a) export improvements for cockpit* and *(b) skill improvements* — each
grounded in friction from this run. End by offering to (1) write the report
to a file, (2) apply any approved skill improvements to this `SKILL.md`, and
(3) dig into a single finding (e.g. trace a root cause through the
working-tree code).

## Python fallback

Only if `jq` is absent. Mirrors steps 1, 3, 5:

```python
import json, re, collections
d = json.load(open("events.json"))
print("events:", len(d))
print("types:", collections.Counter(e["type"] for e in d))
print("agents:", collections.Counter(e.get("agent") for e in d))
tc = [e for e in d if e["type"] == "tool_call"]
print("tools:", collections.Counter(e["data"]["tool"] for e in tc))
print("task delegations:", sum(1 for e in tc if e["data"]["tool"] == "task"))
DANGER = re.compile(r"\bsudo\b|\bchmod\b|\bchown\b|\brm +-[rf]|\bmkfs|\bdd +if=|\b777\b|\bgit +(push|reset +--hard)|\bkill +-9")
for e in tc:
    if e["data"]["tool"] != "bash":
        continue
    cmd = e["data"]["wire_input"]["command"]
    m = re.search(r"exit: (-?\d+)", e["data"].get("output", ""))
    ec = m.group(1) if m else "?"
    flag = "  <-- ESCALATION" if DANGER.search(cmd) else ""
    print(f'seq{e["seq"]} exit={ec} hard_fail={e["data"]["hard_fail"]} | {cmd.splitlines()[0][:140]}{flag}')
```

## Coverage ledger

Append a dated line per log analyzed: what kind of session it was, which
recipes it exercised, and any recipe still **unvalidated** against real
data. This is how the skill earns confidence — an untested recipe is a
hypothesis, not a tool. Update it whenever you apply a skill improvement.

- **2026-05-28 · `c3pp74`** — single-agent (`orchestrator-build`),
  `openai-compatible`/`qwen3.5-9b`, 0% cache, 0 `task` delegations, no
  compaction, no repair-layer activity, no `manifest.json` in export.
  *Validated:* inventory, conversation arc, tool histogram, bash
  exit/`hard_fail`, escalation regex (tightened to drop `2>/dev/null` false
  positives), repeated-command, real `usage` aggregates, system-prompt
  budget, redaction grep. *Lesson baked in:* read real `usage`, never
  char/4 or summed payloads, for session totals.
- **Still unvalidated (no log has exercised these yet):** subagent /
  delegation-tree mapping; reset segmentation (detector ran, found none —
  needs a log with a real prune/compaction); `recovery_kind`/`recovery_stage`
  interpretation; `manifest.json` / fork-tree parsing. Treat these recipes as
  hypotheses until a log proves them.

Token economy — read real usage from the events, do **not** sum payload
sizes (see the warning in Step 6):

```python
u = [e["data"]["usage"] for e in d if e["type"] == "inference_request"]
billed = sum(x["input_tokens"] for x in u)
cached = sum(x["cached_input_tokens"] for x in u)
print("requests:", len(u))
print("billed_input:", billed, "cached:", cached,
      "cache_rate:", round(cached / max(billed + cached, 1), 3))
print("output:", sum(x["output_tokens"] for x in u))
print("peak_context:", max(x["input_tokens"] for x in u))   # high-water mark, not the sum
```

For the system-prompt budget check, `json.load` the first file under
`inference_requests/` and read `len(d["system"])` (÷4 ≈ tokens) and
`d["model"]`.
