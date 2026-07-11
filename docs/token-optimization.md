# Token optimization — `onebrain token`

Every search hit, note body, and `get` OneBrain hands to an agent spends that
agent's context window. A real vault note can run ~15k tokens; a `query`
response repeats the same envelope keys across every hit; nothing measured
any of it before v3.4.10. Token optimization is the layer that measures it
and — on request — shrinks it, honestly.

```bash
onebrain search get 03-knowledge/topic.md --output json   # agent-facing surfaces only
onebrain token gain                                        # what did it save?
```

## Why owning the producer beats a proxy

A generic token-saving proxy (RTK-style tools that sit in front of a CLI and
rewrite its output) can only reshape bytes that already left the producer —
it never knows *why* a document was included, so it can only guess at what's
safe to cut. OneBrain owns the producer: the same binary that runs the
search, ranks the hits, and knows which doc a snippet came from also shapes
the response. That means the shaping can be structural (drop an exact
duplicate chunk, cap a document on a real line boundary with a resumable
cursor) instead of textual (guess where whitespace is safe to strip).

Three things follow from owning the producer, all frozen by
[ADR 0028](decisions/0028-token-optimization-layer.md):

- **One shared crate, `onebrain-token`**, holds every transform as a pure
  function (no I/O). MCP tool responses, the daemon's HTTP routes, and the
  CLI's structured search output all shape through the same code — one
  implementation to trust, not three.
- **A single runner funnel** (`run_funnel`) is the only path an agent-facing
  response may travel: transform → `never_worse` guard → one recorded gain
  event. No surface can skip metering by construction.
- **Honesty is non-negotiable.** Every lossy transform emits a
  machine-readable signal — never a silent drop. See
  [Honesty signals](#honesty-signals) below.

**Human TTY output is never touched.** Token optimization only shapes
agent-facing surfaces: MCP tool responses, the daemon's HTTP routes, and CLI
search verbs' *structured* output (`--output json` / `--output yaml`, or the
`--json` shorthand some verbs offer). Run `onebrain search get notes/x.md` in
your terminal and you get the same full body v3.4.9 gave you — the ladder
only engages when a machine is the consumer.

## The level ladder

One config knob — `token_optimization.level` (or a per-call override) —
picks a rung. Each rung is a strict superset of the one below it: nothing
this release ever gets removed by a lower level.

| Level | Ordinal | What activates | Ledger |
|---|---|---|---|
| `off` | 0 | Nothing. Byte-for-byte today's (pre-v3.4.10) behavior. | — |
| `conservative` *(default)* | 1 | JSON compaction (`json_compact`) · cross-hit exact-duplicate dedup (`doc_dedup`) · whitespace compaction (`whitespace`) · `get` continuation cap with a truthful resume cursor (`get_cap`) — **all four are lossless** | off |
| `balanced` | 2 | + YAML frontmatter strip (`frontmatter`) · snippet cap (`snippet`) · **already-sent ledger turns on** | **on** |
| `aggressive` | 3 | + snippet-less query hits (`disclosure`) · `multi_get` head-only cap (`head_only`) | on |

> **The `get`/snippet caps are flat overrides, not per-level values.** The
> `onebrain-token` crate's internal `TransformCtx::for_level()` defines a
> per-level table (get cap 6000/4000/4000 tokens, snippet 200/150/120 chars
> for conservative/balanced/aggressive) — but every real surface (MCP, CLI,
> daemon) builds its context through `ctx_for()`, which — at any level above
> `off` — always applies your **configured** `token_optimization.get_max_tokens`
> / `snippet_max_chars` (flat values, default `6000` / `200`) instead of that
> per-level table. Verified against `crates/onebrain-cli/src/commands/token_runner.rs`
> (`ctx_for`) and its call sites in `mcp.rs`/`search_query.rs` — none of them
> use the crate's per-level defaults directly. In practice this means: with a
> fresh, unedited `onebrain.yml`, the `get` cap is 6000 tokens and the
> snippet cap is 200 chars at **every** active level, including `balanced`
> and `aggressive` — the tighter numbers only apply once you set
> `get_max_tokens`/`snippet_max_chars` explicitly. The same applies to
> `head_only`'s 500-token internal default on `multi_get`: it's only reached
> when `get_max_tokens` is genuinely unset, which never happens once a level
> is active — set `get_max_tokens` explicitly to control it.

Set it in `onebrain.yml`:

```yaml
token_optimization:
  level: conservative        # off | conservative | balanced | aggressive
```

...or per call — every surface accepts an override, and precedence is always
**per-call > config > product default (`conservative`)**:

```bash
onebrain search get notes/x.md --output json --opt-level aggressive
```

```json
{"searches": [...], "optLevel": "balanced"}
```

(the `optLevel` field on the MCP `query`/`get`/`multi_get` tool params — see
[MCP reference](reference/mcp.md) for the full tool schema). An unparseable
level string is a hard error on every surface — never silently downgraded to
the config value.

### Which transform runs where

A transform is gated by **both** the ladder rung (when) and the surface (where)
— a hit-list transform like `snippet`/`disclosure` must never touch a `get`
document body, or it would truncate or empty it instead of trimming a
snippet. This matrix is enforced structurally, not by convention:

| Transform | Min level | Runs on | Lossy? |
|---|---|---|---|
| `json_compact` | conservative | `query` (MCP/CLI/daemon) | no |
| `doc_dedup` | conservative | `query` (MCP/CLI/daemon) | no |
| `whitespace` | conservative | `get` / `multi_get` (MCP) | no |
| `get_cap` | conservative | `get` (MCP) only — has a real `path:N` cursor | no |
| `frontmatter` | balanced | `get` / `multi_get` (MCP) | yes |
| `snippet` | balanced | `query` (MCP/CLI/daemon) | yes |
| `disclosure` | aggressive | `query` (MCP/CLI/daemon) | yes |
| `head_only` | aggressive | `multi_get` (MCP) only — no per-doc cursor | yes |

Every registered transform ships fixture tests captured from real output,
enforced by a guard test that fails if a new transform lands without them —
the same completeness-guard pattern `onebrain.yml`'s
`config_key_docs()` uses.

## Two cache layers

Two independent redb tables live under `<collection cache>/token/token.redb`
— a sibling of the `models/` and `index/` directories the search engine
already keeps, owned by the warm daemon the same way it owns the search
index (in `Backend::Direct` fallback mode, with no daemon running, the CLI
opens `token.redb` itself — safe because Direct mode only exists when no
daemon holds it). See [ADR 0029](decisions/0029-token-cache-redb.md) for the
full storage rationale (redb over SQLite, generation-counter design).

### Query-result memoization (lossless, every level)

Repeating an identical query — same text, same mode, same knobs — skips
embedding + cross-encoder rerank entirely (~70ms/candidate on CPU) and
returns the cached hit set instantly. The cache key folds in an internal
`generation` counter that bumps on **every** reindex (full or `--lex-only`),
so a stale entry simply stops matching the moment the index changes — there
is no explicit invalidation logic to get wrong. This layer is invisible day
to day: it never changes what you see, only how fast the second identical
call answers.

### Already-sent ledger (lossy, signaled, `balanced`+ only)

Once the ledger is active, each session tracks `(session_token, doc_path) →
content hash last delivered`. Deliver the same doc twice, unchanged, in the
same session, and the second delivery becomes a small reference instead of
the full body:

```bash
$ onebrain search get 03-knowledge/topic.md --output json
{"version":"3.4.10","command":"search.get","ok":true,"vault":{...},
 "data":{"doc_path":"03-knowledge/topic.md","content":"...(15,240 bytes)..."},
 "warnings":[],"error":null}

# same doc, same session, unchanged content, level balanced+:
$ onebrain search get 03-knowledge/topic.md --output json
{"version":"3.4.10","command":"search.get","ok":true,"vault":{...},
 "data":{"doc_path":"03-knowledge/topic.md","content":"",
   "reference":{"doc_path":"03-knowledge/topic.md",
     "hash":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
     "sent_earlier":true,"bytes_saved":15240,
     "rematerialize":"onebrain search get 03-knowledge/topic.md --force"}},
 "warnings":[],"error":null}
```

Edit the doc on disk (the hash changes) and the very next check reads as
`changed` — fresh content, no reference, ledger re-recorded. Every reference
carries its own recovery instruction in `rematerialize`, so an agent that
compacted its own context away never has to guess how to get the full body
back.

### `get --force` — re-materialize

`--force` (CLI `search get --force`, MCP `get`'s `force: true` param)
unconditionally bypasses **both** the ledger and the size cap and returns
the complete document — the exact command every reference's `rematerialize`
field points back to:

```bash
onebrain search get 03-knowledge/topic.md --output json --force
```

This is the one honest way out of a reference: it never guesses, it always
delivers the full current body.

## `onebrain token gain`

`token gain` reports what the ladder actually saved — never inferred, always
read back from what was recorded. Every count below is **byte-exact**; the
token figures derived from those bytes are always labeled an *estimate*
(a calibrated, char-aware, per-model-family heuristic — not a real
tokenizer; see [Estimates are estimates](#estimates-are-estimates)).

### Default: current-epoch summary

```bash
$ onebrain token gain
Token Optimization — Gain Summary
  Calls:        128
  Bytes before: 500000
  Bytes after:  150000
  Bytes saved:  350000 (70.0%)
  [██████████████░░░░░░] 70%
Note: byte counts are exact; per-model token figures are an estimate — see `docs/token-optimization.md`.
```

With no flags, the report is scoped to the **current epoch** — traffic since
the last `--reset` (or everything, if you've never reset). Pass `--all-time`
to sum every epoch including anything archived by a past `--reset`, or
`--since YYYY-MM-DD` for a custom lower bound (both of those read the
precomputed Tier-2 rollups instead of the epoch's raw log, so they stay fast
regardless of history length).

### `--by <time>[,<dim>]` — pivots

Time axis (`day|week|month|year`) and dimension (`surface|transform|level|cache`)
combine in either order, either alone:

```bash
$ onebrain token gain --by month,surface --all-time
time         dim                  before        after        saved    calls
2026-07      cli_search           210000        96000       114000      340
2026-07      mcp_query            180000        42000       138000       96
2026-07      mcp_get              110000        12000        98000       46

Token Optimization — Gain Summary
  Calls:        482
  Bytes before: 500000
  Bytes after:  150000
  Bytes saved:  350000 (70.0%)
  [██████████████░░░░░░] 70%
  (scope: all-time — includes traffic before the reset at 2026-07-04; omit --all-time for the current epoch only)
Note: byte counts are exact; per-model token figures are an estimate — see `docs/token-optimization.md`.
```

`surface` values are `mcp_query | mcp_get | mcp_multi_get | cli_search |
daemon_http | read_hook`; `cache` values are `none | memo_hit | ledger_ref |
hook_failopen`.

### `--history` — the raw per-call tail

Tails the current epoch's raw JSONL log directly (the only mode that reads
raw events instead of a rollup), capped at the 200 most recent, oldest first:

```bash
$ onebrain token gain --history
ts                   surface      transform            level            before      after  cache
2026-07-11           mcp_query    whitespace,json_compact conservative       4820       2110  none
2026-07-11           mcp_get      none                 balanced          15320         48  ledger_ref
2026-07-11           cli_search   snippet              balanced           6100       1830  none
```

### `--reset [--label <name>]` — baseline testing

`--reset` never deletes: it moves the current epoch's raw log to
`token/gain/archive/<ts>-<label>/` and starts counting fresh. This is the
baseline-comparison workflow:

```bash
$ onebrain token gain --reset --label off
Archived current window to /vault/.cache/token/gain/archive/1752345678-off (never deleted).
Counting fresh from now — "since reset" starts at 2026-07-11.

# ... run a while at level: off ...
$ onebrain token gain
  (scope: since reset 2026-07-11)

$ onebrain token gain --reset --label balanced
# ... switch onebrain.yml token_optimization.level to balanced, run a while ...
$ onebrain token gain
  (scope: since reset 2026-07-11)

# compare the two windows head-to-head:
$ onebrain token gain --all-time --by level
```

An archived epoch is never lost — `--all-time` and `--since` both reach it
through the cumulative rollup, and `--rebuild` (below) walks it too.

### `--rebuild` — rollup recovery

The Tier-2 rollup tables (`GAIN_DAILY`/`GAIN_MONTHLY`/`GAIN_YEARLY` in
`token.redb`) are derived state — the raw JSONL is the single source of
truth. If a rollup ever drifts (`onebrain doctor` checks this),
`--rebuild` replays every raw event and reconstructs them from scratch:

```bash
$ onebrain token gain --rebuild
Rebuilt rollups from 610 raw event(s).

Token Optimization — Gain Summary
  Calls:        610
  ...
```

### Estimates are estimates

Byte counts in every report above are exact — they're the literal
`bytes_before`/`bytes_after` recorded by the funnel. The *token* figure a
consumer derives from those bytes is a calibrated heuristic
(`onebrain_token::estimate_tokens`), walking Unicode scalar values rather
than raw UTF-8 bytes so Thai/CJK text — 3 bytes/char in UTF-8 — isn't
inflated the way a naive `bytes.len() / 4` heuristic would inflate it (that
naive form is exactly what earlier tools like RTK use, and it's a systematic
overcount on multi-byte scripts). Every text render appends the estimate
disclaimer; `--json` carries plain byte counts with no derived token field at
all, leaving token/cost math to the consumer.

## The vault-read ledger gate hook

Agents also read vault notes directly with the host's `Read` tool — a path
that bypasses OneBrain (and the ledger) entirely: uncapped, un-ledgered,
invisible to `token gain`. The hook closes that gap for the one case that's
pure waste: **a repeat, unchanged read of the same doc in the same
session.**

- **What it gates.** A PreToolUse hook (registered by the *plugin*, a
  separate repo — see below) calls `onebrain token check <path>` before
  letting `Read` touch a vault `.md` file.
  - First-time read, or the doc's content changed since it was last sent →
    **exit 0, allow, stdout empty.** The common case has zero compat
    surface.
  - Repeat read of unchanged content → **exit 2, deny.** The
    [reference envelope](#already-sent-ledger-lossy-signaled-balanced-only)
    JSON is on stdout, so the hook can hand the agent the `--force` recovery
    path instead of the tool ever running.
- **Three bypasses:**
  1. `token_optimization.read_hook: off` in `onebrain.yml` (the CLI answers
     allow immediately — no ledger lookup at all).
  2. `ONEBRAIN_HOOK_BYPASS=1` per session — honored by the **plugin's** hook
     script before it ever calls `token check`; the CLI verb itself has no
     env-var bypass of its own.
  3. Per-call `onebrain search get <path> --force` — the recovery path
     every deny reason already points at.
- **Fail-open, always.** Any trouble getting a trustworthy verdict — no
  vault, no daemon, a daemon too old to have the route (version skew), a
  transport error, an unresolvable session token, or the whole round-trip
  exceeding a client-enforced **200ms** budget — resolves to **exit 0
  (allow)**. A read is never blocked by infrastructure trouble. Every
  fail-open records a `hook_failopen` gain event (visible in `token gain
  --by cache`) so `onebrain doctor` can surface a dead daemon instead of the
  hook silently degrading.
- **Default `off`.** Shipping default for v3.4.10 is `off` — the hook, if
  registered, always allows. `onebrain token discover` (below) is the
  measurement instrument deciding whether a future release flips the
  default.
- **Where it's registered.** The CLI ships only the `token check` verb over
  the daemon's ledger route — the actual PreToolUse hook *registration*
  lives in the vault plugin (`onebrain-ai/onebrain`, a separate repo with
  its own version bump). This guide documents the CLI side; see that
  repo's plugin docs for the hook wiring itself.

See [ADR 0031](decisions/0031-vault-read-ledger-gate-hook.md) for the
full design rationale, including why RTK's rewrite-in-place mechanic doesn't
transfer to a `Read` tool call.

## `token check` — the hook's verdict

```bash
$ onebrain token check 03-knowledge/topic.md
$ echo $?
0            # allow — nothing on stdout

$ onebrain token check 03-knowledge/topic.md   # same doc, same session, unchanged
{"doc_path":"03-knowledge/topic.md","hash":"9f86d081...","sent_earlier":true,"bytes_saved":15240,"rematerialize":"onebrain search get 03-knowledge/topic.md --force"}
$ echo $?
2            # deny — reference envelope on stdout
```

The 0/2 exit protocol is frozen: **0 = allow, 2 = deny.** Nothing else is
ever returned — an unrecognized future verdict, a malformed daemon response,
or any other surprise resolves to allow (fail-open), never to a guessed
deny. The whole round-trip is budgeted at 200ms client-side, independent of
the daemon client's own (much longer) HTTP timeouts, so a wedged daemon
can't stall the read it's supposed to be gating.

## `token discover` — measuring what the hook would have caught

With the hook off (the default), nothing measures direct `Read`/`Grep`
bypass traffic. `token discover` retroactively estimates it by scanning
Claude Code's own session transcripts (`~/.claude/projects/**/*.jsonl`) for
a vault `.md` path touched a **second** time within the same session file —
exactly the traffic the ledger would have denied had the hook been active.
(A `Grep` on a path already `Read` in the same session counts as a repeat
too — both deliver the doc's bytes into the agent's context.)

```bash
$ onebrain token discover
Token Discover — direct-read field test
  Transcripts scanned: 42
  Bypassed reads:      17 (repeat Read/Grep the ledger could have denied)
  Est. tokens missed:  38200 (estimate)
  Top paths:
      4  01-projects/onebrain/cli/2026-07-11-v3.4.10-design.md
      3  05-agent/MEMORY.md
      2  03-knowledge/rust/ownership.md
```

`--since-days N` limits the scan to transcript files modified in the last
`N` days; `--json` emits the same data through the canonical envelope. This
is read-only — it never writes to a transcript, a vault doc, or
`token.redb` — and it's the instrument whose numbers decide whether a future
release flips `read_hook`'s default from `off`.

## Honesty signals

Every lossy transform emits a machine-readable signal — the non-negotiable
half of [ADR 0028](decisions/0028-token-optimization-layer.md). Nothing is
ever silently dropped:

| Signal | Emitted by | Meaning |
|---|---|---|
| `Truncated { next }` | `get_cap`, `head_only`, frontmatter strip | Content was cut. `next` is a real line-index cursor (resumable via `fromLine`) for a `get` cap, or the literal tag `"body"` for a `multi_get` head-only cut (no per-doc cursor exists there), or `"frontmatter"` for a stripped block. |
| `SnippetOmitted` | `snippet`, `disclosure` | A per-hit snippet was shortened or removed entirely. |
| `ChunksCollapsed(N)` | `doc_dedup` | `N` exact-duplicate chunks were collapsed into one — lossless (no distinct information was in the duplicates). |
| `Reference { doc_path, hash, bytes_saved, rematerialize }` | the already-sent ledger | The body was replaced by a reference — see [the ledger](#already-sent-ledger-lossy-signaled-balanced-only) above. |

On the MCP `get`/`multi_get` text surfaces, a signal becomes a plain-text
marker appended to the response body, so an agent reading raw text (not
JSON) still sees the honesty note:

```text
[truncated at line 240: continue with `fromLine: 240`, or get the full document with `onebrain search get notes/x.md --force`]
[snippet omitted]
[3 duplicate chunk(s) collapsed]
```

Underneath every signal sits one structural backstop: `never_worse`. Before
any transformed payload is returned, the funnel compares its **estimated
token cost** against the original's — if the transform would have made the
response cost more (a bug in a transform, or a pathological input), the
original is returned unchanged instead. This is a token comparison, not a
byte comparison, deliberately: on a Thai/CJK-heavy vault, byte length and
token count can point in opposite directions (a multibyte script is ~3
bytes/char but nowhere near 3 tokens/char), and a byte-only guard would
revert a genuinely token-smaller compaction, or keep a genuinely
token-costlier one.

## `token_optimization:` config reference

```yaml
token_optimization:
  level: conservative        # off | conservative | balanced | aggressive · default: conservative
  get_max_tokens: 6000       # `get` continuation cap in estimated tokens, 0 = unlimited · default: 6000
  snippet_max_chars: 200     # per-hit snippet length cap in characters · default: 200
  strip_frontmatter: auto    # auto | always | never · default: auto
  model: auto                # model family hint for token estimation + pricing · default: auto
  read_hook: off              # off | ledger · default: off
```

| Key | What it is | Default | Valid values |
|---|---|---|---|
| `token_optimization.level` | Optimization ladder rung (see [the ladder](#the-level-ladder) above) | `conservative` | `off`, `conservative`, `balanced`, `aggressive` |
| `token_optimization.get_max_tokens` | `search get` / MCP `get` continuation cap, in estimated tokens. A flat cap applied at every active level (see the [caps note](#the-level-ladder) above) — not level-specific; `0` means unlimited | `6000` | integer ≥ 0 |
| `token_optimization.snippet_max_chars` | Per-hit query snippet length cap, in characters. A flat cap applied at every active level (see the [caps note](#the-level-ladder) above) — not level-specific | `200` | integer ≥ 1 |
| `token_optimization.strip_frontmatter` | When to strip YAML frontmatter from `get`/`multi_get` doc bodies. `auto` follows the ladder (strips at balanced+) | `auto` | `auto`, `always`, `never` |
| `token_optimization.model` | Model-family hint for token estimation calibration and pricing. `auto` sniffs a hint from `settings.json`; anything starting with `gpt`/`o1`/`o3` or containing `openai` resolves to the GPT-4-class calibration table, everything else (including `auto`) resolves to the Claude-family table | `auto` | any string |
| `token_optimization.read_hook` | Vault-read ledger-gate hook mode (see [the hook](#the-vault-read-ledger-gate-hook) above). `off` = the hook, if the plugin registers it, always allows; `ledger` = repeat reads of an already-sent doc are denied with a reference | `off` | `off`, `ledger` |

`onebrain init` scaffolds this whole block, fully commented, into every
fresh `onebrain.yml` — token optimization is a headline feature, not an
opt-in block hidden from new vaults. Every key above has a
`config_key_docs()` entry (`onebrain-fs/src/init/onebrain_yml.rs`), the same
completeness-guard mechanism that backs every other section of
`onebrain.yml` — see [Configuration](configuration.md).

## Design decisions

- [ADR 0028 — Token optimization layer](decisions/0028-token-optimization-layer.md): the `onebrain-token` crate, the level ladder, the honesty contract, the single runner funnel.
- [ADR 0029 — Token cache on redb](decisions/0029-token-cache-redb.md): why redb over SQLite, the memoization key, the generation counter, the already-sent ledger.
- [ADR 0030 — Gain telemetry](decisions/0030-gain-telemetry-raw-plus-rollups.md): raw JSONL + precomputed rollups, epoch reset, the one shared pivot engine.
- [ADR 0031 — Vault-read ledger gate hook](decisions/0031-vault-read-ledger-gate-hook.md): why RTK's rewrite-in-place mechanic doesn't transfer, the fail-open contract, the default-off decision.
