# onebrain.yml — configuration reference

`onebrain.yml` at the vault root is the single config file. `onebrain init`
scaffolds it as a **self-documenting template**: every key is preceded by a
`# <what it is> · default: <value>` comment, so the file itself always tells
you what a knob does and what to put back if you mistune it (v3.4.8,
[ADR 0026](decisions/0026-config-self-documentation.md)).

Two safety nets back the comments:

- **`onebrain doctor`** validates every *present* value against the same
  defaults and model registries the runtime uses, and reports out-of-range
  values per key (the `config-values` check). Absent keys are always fine —
  the runtime falls back to the defaults below.
- **`onebrain doctor --fix`** resets out-of-range **tunables** to their
  documented defaults through a comment-preserving line edit — your comments,
  key order, and other values survive. **Structural keys are never
  auto-reset**: `folders.*` (renaming a folder here doesn't move notes on
  disk) and `search.collection` (changing it detaches the index) are
  report-only; edit them deliberately.

**Existing vaults:** configs created before v3.4.8 have no comments —
`onebrain doctor` reports the undocumented keys and `onebrain doctor --fix`
inserts the same per-key comment lines the fresh template ships, without
touching your values, your key order, or any comment you wrote yourself.

## Section layout

The template groups keys under labelled banners in a fixed order:

```
# ── General ──…          update_channel
# ── Vault layout ──…     folders
# ── Agent behavior ──…   checkpoint, recap
# ── Search ──…           search
# ── Token optimization ──… token_optimization
# ── Automation ──…       schedule
# ── System ──…           stats   (managed by OneBrain — do not edit)
```

`onebrain doctor` reports "layout differs from template" when an existing
config's top-level blocks are out of this order or missing their banners;
`onebrain doctor --fix` restructures it — reordering the blocks and inserting
the banners while moving each block as opaque bytes, so every value and comment
survives. The restructure is idempotent (a second `--fix` changes nothing).
Unknown top-level keys keep their relative order and are placed after the
known blocks, before `stats`.

The restructure declines — leaves the file untouched, and never reports layout
drift — exactly these root shapes, each surfaced by the existing `onebrain.yml`
validity checks instead: invalid YAML (including duplicate top-level keys), a
non-mapping root (sequence/scalar/empty), a flow-style root mapping
(`{a: 1, b: 2}`), and a block mapping with no recognisable top-level keys.

Known cosmetic limitations (exotic shapes only): a keep-chomped block scalar
(`|+`) under an *unknown* top-level key can lose trailing blank value lines
when blocks are separated (template-known keys never carry block scalars), and
a comment separated from its key by a blank line travels with the *preceding*
block rather than the key below it (position shifts; nothing is lost).

Legacy note: v3.0 named this file `vault.yml`. The CLI still reads the old
name with a deprecation warning; `onebrain doctor --fix` migrates it.

## Keys

Adding a config key requires a `config_key_docs` entry (comment + default);
a completeness test enforces this against the config structs.

| Key | What it is | Default | Valid values | `--fix` resets? |
|---|---|---|---|---|
| `update_channel` | Release channel for plugin updates | `stable` | `stable`, `next` | yes |
| `folders.inbox` | Raw braindumps and quick captures | `00-inbox` | non-empty folder name | **no — report-only** |
| `folders.projects` | Active projects with tasks and notes | `01-projects` | non-empty folder name | **no — report-only** |
| `folders.areas` | Ongoing responsibilities | `02-areas` | non-empty folder name | **no — report-only** |
| `folders.knowledge` | Your own synthesized thinking | `03-knowledge` | non-empty folder name | **no — report-only** |
| `folders.resources` | External info: research, summaries, reference | `04-resources` | non-empty folder name | **no — report-only** |
| `folders.agent` | AI-specific context and memory | `05-agent` | non-empty folder name | **no — report-only** |
| `folders.archive` | Completed projects and archived areas | `06-archive` | non-empty folder name | **no — report-only** |
| `folders.logs` | Session logs, checkpoints, system logs | `07-logs` | non-empty folder name | **no — report-only** |
| `checkpoint.messages` | Message count between Stop-hook checkpoint emissions | `15` | integer ≥ 1 | yes |
| `checkpoint.minutes` | Minutes between Stop-hook checkpoint emissions | `30` | integer ≥ 1 | yes |
| `search.collection` | Collection name binding the vault to its index — absent = search disabled; `onebrain search reindex` sets it | *(unset)* | non-empty name | **no — report-only** |
| `search.embed_model` | Embedding model (see `onebrain search model list`) | `multilingual-e5-small` | model-registry name | yes — **prints a reindex-required warning** (old vectors are stale) |
| `search.default_top_k` | Result count when a caller doesn't pass `top_k` | `10` | integer ≥ 1 | yes |
| `search.exclude` | Extra index-exclusion patterns on top of the built-ins | `["attachments"]` | list of path prefixes / dir names | not validated |
| `search.embed.auto` | Auto-embed changed docs on/off (gate parsed; enforcement pending) | `true` | `true`, `false` | not validated |
| `search.embed.threshold` | Changed docs required before an auto-embed run triggers | `10` | integer ≥ 1 | not validated |
| `search.embed.debounce_seconds` | Debounce window before an auto-embed run fires | `45` | integer ≥ 1 | not validated |
| `search.embed.max_batch` | Max docs embedded per batch | `200` | integer ≥ 1 | not validated |
| `search.embed.schedule` | Cron schedule for a periodic full re-embed | *(unset)* | cron expression | not validated |
| `search.reranker.enabled` | Tier-2 cross-encoder rerank stage on/off | `true` | `true`, `false` | yes |
| `search.reranker.model` | Reranker model (see `onebrain search model list`) | `onebrain-rerank-v1` | reranker-registry name | yes |
| `search.reranker.min_candidates` | Minimum candidate pool to rerank (a floor, not a ceiling) | `10` | integer ≥ 1 | yes |
| `search.reranker.min_score` | Score gate: hits below this calibrated 0–1 score are dropped | `0.0` (engine-calibrated — drops nothing by default as of v3.4.16, [ADR 0034](decisions/0034-heading-search-schema-selfheal-rerank-gate-decouple.md); key may also be omitted) — vaults initialized on v3.4.7–v3.4.15 have `0.30` written out explicitly, flagged as **superseded** by `doctor` (a legal value, but no longer the recommended default) | number in `[0, 1]` | yes |
| `token_optimization.level` | Token-optimization ladder rung — see [`token-optimization.md`](token-optimization.md) | `conservative` | `off`, `conservative`, `balanced`, `aggressive` | not validated |
| `token_optimization.get_max_tokens` | `search get` / MCP `get` continuation cap, in estimated tokens. Unset → per-level ladder (6000/4000/4000); a set value pins a fixed cap at every active level; `0` = unlimited | *unset* (per-level) | integer ≥ 0 | not validated |
| `token_optimization.snippet_max_chars` | Per-hit query snippet length cap, in characters. Unset → per-level ladder (200/150/120); a set value pins a fixed cap at every active level | *unset* (per-level) | integer ≥ 0 | not validated |
| `token_optimization.strip_frontmatter` | When to strip YAML frontmatter from `get`/`multi_get` doc bodies. `auto` strips at balanced+ per the ladder; `always` strips from conservative up; `never` never strips | `auto` | `auto`, `always`, `never` | not validated |
| `token_optimization.model` | Model-family hint for token estimation calibration + pricing | `auto` | any string | not validated |
| `token_optimization.read_hook` | Vault-read ledger-gate hook mode (plugin registers the PreToolUse hook; this key only gates the CLI's ledger check) | `off` | `off`, `ledger` | not validated |
| `recap.min_sessions` | Unrecapped session logs required before `/recap` runs (plugin key) | `6` | integer ≥ 1 | not validated (plugin-level) |
| `recap.min_frequency` | Sessions a topic must recur in to be promoted to memory (plugin key) | `2` | integer ≥ 1 | not validated (plugin-level) |
| `schedule` | Scheduled skill/command entries compiled by `onebrain schedule register` | *(none)* | see `schedule register` docs | not validated |
| `stats.*` | Doctor run timestamps, stamped by `onebrain doctor`, plus `qmd_cleanup_declined`, a flag written by `doctor --fix` when the user declines legacy-qmd cleanup | — | managed automatically | — |

Validation sources: the defaults above are the Rust runtime's own default
fns (`onebrain-core`), the embedding/reranker model registries
(`onebrain-search`), and the shared update-channel constants (`onebrain-fs`)
— doctor never keeps a second copy of a range, so the table cannot drift
from the binary you're running.

## Recovering from a bad edit

```console
$ onebrain doctor            # names each out-of-range key + its default
$ onebrain doctor --fix      # resets the tunables listed above; comments survive
```

Unsupported YAML shapes (e.g. inline mappings like
`checkpoint: {messages: 0}`) are refused rather than guessed at — doctor
reports the key as un-fixable and you edit it by hand. Every write is
preceded by a timestamped backup under `<vault>/.onebrain-backups/`.

## Comments always survive (v3.4.8)

Every writer that touches `onebrain.yml` is comment-preserving as of v3.4.8
(issue #200): the key-backfill, legacy `qmd_collection` → `search.collection`
migration, and the first `search reindex`/`model set` config write all apply
surgical line edits, so your comments, key order, and CRLF/LF style survive a
full `doctor --fix` on a legacy vault — regardless of which recipe runs first.
The only `onebrain.yml` writes that don't preserve comments are the initial
template scaffold (there are no comments to keep) and a fallback for
degenerate non-mapping / flow-style roots.

One caveat when a `--fix` recipe removes a deprecated or legacy key (e.g. the
`onebrain.yml-keys` backfill stripping `onebrain_version`/`method`): the
`delete_key` primitive that performs the removal takes the key's own
continuation lines *and* its lead comment — the comment block sitting
directly above it — on the theory that a doc comment left dangling above
nothing reads worse than losing it. If you've written your own note above a
key that later becomes deprecated, that note is removed along with the key,
not preserved elsewhere. Position durable comments so they aren't read as a
single key's lead comment — e.g. on their own line with a blank line
separating them from the key below, or attached to a key you don't expect to
deprecate.

## Reading the `doctor` report

`onebrain doctor` opens with a `🩺 Doctor · <vault> · onebrain <version>`
header, lists the checks grouped into Config / Vault structure / Integration /
Index & state (the two legacy-migration checks collapse into one `migration`
row in the text view — the JSON keeps them separate), and closes with a boxed
**Summary**: the ok/warning/fail tally, the non-ok findings (failures first),
and deduplicated `💡 command → outcome` next-actions. When a warm daemon
(`onebrain mcp`) is holding the search index, the search check reads its
doc/pending counts from the daemon instead of reporting the index as locked.

**Index & state checks** include:

- **search** — Is there an index, and is it current? Checks collection name, presence on disk, total doc count, pending/unembedded count.
- **lex-index** — Is the keyword (tantivy BM25) index actually alive? Detects the silent failure mode where an interrupted schema migration (v3.4.16 upgrade) leaves the tantivy index empty while the chunk metadata table still holds chunks — every keyword search would return nothing. Touches no vault files, but opening the engine is itself the rebuild self-heal. Resolution failures (no collection, no index on disk) degrade to `ok/"skipped"` rather than false-positive warnings. A busy engine is the exception: a running daemon holds the collection lock for its whole lifetime, and that is exactly the configuration in which this is the *only* check that can see a dead index — so it reports `warn` "could not verify the keyword index" (never auto-fixable) rather than an ok-rendered skip. The complete set of outcomes:

  | Outcome | Status | Repair |
  |---|---|---|
  | healthy, or resolution skipped (no collection / no index on disk) | `ok` | — |
  | **could not verify** — engine busy (daemon holds the lock) | `warn` | not auto-fixable; stop the daemon and re-run |
  | **dead** — keyword index empty while the collection holds N chunk(s) | `error` | `doctor --fix` (preferred; rebuilds from stored metadata only), or `search reindex --force` |
  | **orphaned** — stored chunk metadata empty while the keyword index still holds N doc(s) | `error` | **`search reindex --force` only** — `doctor --fix` cannot repair it, because the metadata a rebuild would read is itself gone |
  | **excess docs** — keyword index holds more docs than the collection has chunks (duplicates/orphans skew BM25 ranking) | `warn` | `doctor --fix`, or `search reindex --force` |
  | **rebuild still pending** after the automatic retry | `warn` | `doctor --fix`, or `search reindex --force` |
