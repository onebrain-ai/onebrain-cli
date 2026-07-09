# 0026 — Self-documenting onebrain.yml + doctor validate/reset-to-default

Status: accepted (v3.4.8)

## Context

`onebrain.yml` was scaffolded by `render_onebrain_yml` through
`serde_yaml::to_string`, which cannot emit comments — the file carried bare
keys with no explanation and no record of what the defaults are. A user who
mistuned a value (e.g. `reranker.min_score: 7.5`, or an `embed_model` typo)
had no in-file way to discover the valid range or revert, and `doctor` only
validated key *presence* plus two checkpoint numbers — most out-of-range
values sailed through silently until they degraded search or broke config
parsing entirely.

The scaffold also wrote no `search:` block at all; the v3.4.7 reranker keys
(`default_top_k`, `reranker.*`) were invisible until a `reindex`/`model set`
first touched the file.

## Decision

1. **Hand-authored commented template.** `render_onebrain_yml` emits a
   hand-written template: every key preceded by
   `# <what it is> · default: <value>`. Values are interpolated from the same
   `onebrain_core` default fns the runtime falls back to (plus the shared
   `VALID_UPDATE_CHANNELS` / `DEFAULT_UPDATE_CHANNEL` constants in
   `onebrain-fs::vault_sync::branch`), so the template cannot drift from the
   runtime — round-trip tests assert the equality. The `schedule:` block is
   still serialized from the chosen preset's entries and appended.
   - The **full `search:` block is scaffolded active** with real defaults;
     `collection` is written as a **commented placeholder** — absent =
     search-disabled semantics preserved (`search reindex` activates it).
   - `min_score` is scaffolded active at the engine-calibrated default. The
     value is mirrored as `TEMPLATE_RERANK_MIN_SCORE` (onebrain-fs cannot
     depend on onebrain-search) and pinned to
     `onebrain_search::engine::DEFAULT_RERANK_MIN_SCORE` by a cross-crate
     test in onebrain-cli.

2. **New `config-values` doctor check (CLI layer).** Validates every PRESENT
   tunable against the runtime source of truth — `CheckpointPolicy` /
   `SearchConfig` / `RerankerConfig` default fns, `model_registry()`,
   `reranker_registry()`, `VALID_UPDATE_CHANNELS` — no duplicated range
   literals. Absent keys are fine (serde falls back to the default). Lives in
   `onebrain-cli/src/commands/doctor.rs` next to `native_search_check`
   because it needs registry access that `onebrain-fs` doesn't have. All
   findings are advisory (`warn`).

3. **Reset policy: tunables auto / structural report-only** (design lock
   2026-07-09). `doctor --fix` auto-resets `checkpoint.messages/minutes`,
   `search.default_top_k`, `search.embed_model`,
   `search.reranker.{enabled,model,min_candidates,min_score}`, and
   `update_channel` to their documented defaults. An `embed_model` reset
   additionally prints a **reindex-required** warning (the old model's
   vectors are stale). `folders.*` and `search.collection` are **never
   auto-reset** — renaming folders orphans notes; changing the collection
   detaches the index — they are reported with explicit
   "never auto-reset — edit manually" wording. Every reset is itemised in the
   fix footer (`key → default`).

4. **Comment-preserving reset writer.** `reset_config_value` is a line editor
   modeled on `upsert_doctor_stats`: it walks block-form section headers,
   replaces only the value portion of the target key line, and preserves
   indentation, surrounding comment lines, inline `# …` comments on the key
   line, key order, and the file's CRLF/LF style. Unsupported shapes (inline
   mappings like `checkpoint: {messages: 0}`) are refused and reported as
   un-fixable rather than guessed at. A trailing `# …` comment on a section
   header (`search:  # my search config`) is accepted as block form. When a
   `--fix` pass lands SOME resets while others are un-fixable, the recipe
   reports the honest tri-state **`partial`** — a new `FixOutcome` value
   (JSON `fix[].outcome: "partial"`, text glyph `◐`) added alongside
   `fixed`/`failed`/`manual`; it counts toward a non-zero exit like `failed`
   because a manual edit is still required, but the message itemises the
   resets that DID land.

## Consequences / migrations

- **Value validation moved out of `onebrain.yml-keys`.** The fs-layer
  `VaultYmlKeysCheck` no longer warns on non-positive
  `checkpoint.messages/minutes`, and its `--fix` recipe
  (`fix_vault_yml_keys`) no longer repairs them — that recipe re-serializes
  through serde_yaml (comments destroyed), and it runs before the new
  recipe, so leaving the old repair in place would have wiped the comments
  this epic exists to protect. The check keeps key-presence, deprecated-key,
  and `update_channel` enum validation (now sourced from
  `VALID_UPDATE_CHANNELS`); an invalid `update_channel` is therefore
  reported by both checks, but only `fix_config_values` repairs it and the
  post-fix re-check clears both rows.
- **Doctor's search check is now strictly read-only.** It resolves the
  collection via `collection_name_readonly` instead of `collection_for`;
  the latter persisted a generated collection name through a serde
  re-serialization on never-configured vaults — which would have stripped
  the new template's comments on the very first `doctor` run. The engine
  open on the index-exists path goes through the new
  `open_engine_with_collection` (never `collection_for`) for the same
  reason.
- **Hint behavior change (intentional):** value findings now always carry a
  `Run onebrain doctor --fix …` hint when at least one finding is
  auto-resettable; pre-v3.4.8, checkpoint-only warnings from
  `onebrain.yml-keys` carried no hint at all.
- **Vault-sync's config writer is comment-preserving and change-detecting**
  (R3 fix). `update_vault_yml` (sync step 7 — runs on default `onebrain
  init`, every `vault-sync`/plugin update, and `doctor --fix`'s plugin-files
  repair) previously whole-file-serialized the config, which destroyed the
  template's comments on first contact with the DEFAULT install path. It
  now (a) does not write at all when `update_channel` already carries the
  resolved channel — the fresh-template and re-sync cases — and (b) applies
  a needed change via the shared comment-preserving line editor
  (`onebrain_fs::yaml_edit`, extracted from doctor's reset machinery so
  both crates use the identical editor). Only degenerate shapes (non-mapping
  or flow-style roots, which carry no meaningful comments) keep the legacy
  serde rewrite.
- **Known limitation (follow-up: issue #200):** exactly three structural
  writers still whole-file-serialize and drop comments —
  `fix_vault_yml_keys`, `fix_legacy_qmd_collection`, and `onebrain-fs`'s
  `persist_search_key` (the first `search reindex`/`model set` on a fresh
  vault). Each discloses the comment loss in its output; migrating them
  onto `yaml_edit` is deferred to issue #200.
- Doctor is now 12 checks (was 11); `config-values` renders in the ⚙️ Config
  section as "config values".
- Existing vaults keep their uncommented files until scaffolded anew —
  `doctor --fix` deliberately does NOT rewrite a healthy config to add
  comments (minimal footprint; the reference doc covers every key instead).

## Related

- [ADR 0025](0025-tier2-cross-encoder-reranker.md) — the reranker keys this
  template documents.
- `docs/configuration.md` — the full key-by-key reference.
- GitHub issue #196.
