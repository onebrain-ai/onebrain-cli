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
| `search.reranker.min_score` | Score gate: hits below this calibrated 0–1 score are dropped | `0.30` (engine-calibrated; key may also be omitted) | number in `[0, 1]` | yes |
| `recap.min_sessions` | Unrecapped session logs required before `/recap` runs (plugin key) | `6` | integer ≥ 1 | not validated (plugin-level) |
| `recap.min_frequency` | Sessions a topic must recur in to be promoted to memory (plugin key) | `2` | integer ≥ 1 | not validated (plugin-level) |
| `schedule` | Scheduled skill/command entries compiled by `onebrain schedule register` | *(none)* | see `schedule register` docs | not validated |
| `stats.*` | Doctor run timestamps, stamped by `onebrain doctor` | — | managed automatically | — |

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
