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
  (`fix_vault_yml_keys`) no longer repairs them — at the time, that recipe
  re-serialized through serde_yaml (comments destroyed), and it ran before
  the new recipe, so leaving the old repair in place would have wiped the
  comments this epic exists to protect. (`fix_vault_yml_keys` was later
  migrated onto the comment-preserving `yaml_edit` editor in #208 — see the
  "All config writers are comment-preserving" entry below — but the
  validation split documented here was not reverted.) The check keeps
  key-presence, deprecated-key, and `update_channel` enum validation (now
  sourced from `VALID_UPDATE_CHANNELS`); an invalid `update_channel` is
  therefore reported by both checks, but only `fix_config_values` repairs it
  and the post-fix re-check clears both rows.
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
- **All config writers are comment-preserving as of v3.4.8 (issue #200,
  historical note).** The four structural writers that used to whole-file
  re-serialize through `serde_yaml::to_string` and drop every comment —
  `fix_vault_yml_keys`, `fix_legacy_qmd_collection`, `onebrain-fs`'s
  `persist_search_key` (the first `search reindex`/`model set` on a fresh
  vault), and `onebrain-fs`'s `remove_search_key` (fired by
  `reconcile_missing_model`) — were migrated onto `onebrain_fs::yaml_edit`.
  Each classifies its change with a **read-only** serde parse, then applies it
  as comment-preserving line edits: `append_top_level`/`upsert_child` for
  backfills and search-key writes, and a new `delete_key` primitive for
  removing deprecated/legacy keys (top-level and nested, exact indent-level
  match, refuses inline mappings; it takes the key's continuation lines and
  its lead doc-comment — a doc comment dangling above nothing reads worse than
  losing it). Their "YAML comments not preserved" disclosures were removed
  with them, and `remove_search_key` now returns whether it removed a key so
  `reconcile_missing_model` logs the config mutation instead of discarding it.
  Because `fix_vault_yml_keys` no longer drops comments, recipe **ordering**
  ceased to matter — it runs before the `config-values` recipe, but a legacy
  vault's user comments survive the whole `--fix` pass regardless. The only
  remaining `serde_yaml::to_string` writes to `onebrain.yml` are
  `render_onebrain_yml`'s schedule-block generation (template creation, not an
  edit) and vault-sync's documented degenerate-root fallback (non-mapping /
  flow-style roots, which carry no meaningful comments). `stamp_doctor_run`
  now also declines flow-style roots the same way `restructure_config` does.
- Doctor is now 12 checks (was 11); `config-values` renders in the ⚙️ Config
  section as "config values".
- **Existing vaults get the self-documentation via `doctor --fix`**
  (sanctioned scope addition, 2026-07-09). The `config-values` check counts
  template-known keys that exist without a comment line directly above them
  and reports "N undocumented key(s)" (read-only — zero writes on a plain
  run); `--fix` inserts the template's own `# <what it is> · default: …`
  line above each, at the key's indentation. Guarantees: a key under the
  user's OWN comment is never touched (no replace, no dedupe), missing keys
  are never added (absent = defaults by design; the `# collection:`
  placeholder is never injected next to an active `collection:`), nothing
  is reordered, and the pass is idempotent. Single source of truth: the
  comment strings come from `config_key_docs()` in `onebrain-fs`, the SAME
  table `render_onebrain_yml` interpolates into the fresh template — the
  two cannot drift (plus a template↔table pin test).

## Addendum (2026-07-09) — section layout + full restructure

Live-testing #199's comment backfill on a real vault surfaced three gaps: the
plugin-level `recap:` and `schedule:` keys were undocumented, `stats:` sat
mid-file, and the config had no visual grouping. Design locked **Style A —
section banners, English only**.

1. **Locked section order + banners.** The config is grouped under
   `# ── <Section> ──…` banners in the order **General → Vault layout →
   Agent behavior (checkpoint, recap) → Search → Automation (schedule) →
   System**. `stats:` is the sole member of System and always lands last,
   under a `# Managed by OneBrain — do not edit.` note. The order, banner
   format, and assembler live in ONE module — `onebrain-fs::config_layout` —
   consumed by both `render_onebrain_yml` (fresh template) and
   `doctor --fix` (existing vaults), so the two cannot drift. A round-trip
   test asserts `restructure(render(preset)) == render(preset)` for every
   preset (the fresh template is already canonical, so a restructure of a
   freshly-init'd vault is a byte-identical no-op).

2. **`config_key_docs` gains `recap.min_sessions`/`min_frequency`.** These are
   PLUGIN-level keys, not part of `VaultConfig` — the fresh template never
   emits a `recap:` block (absent = the plugin uses its own defaults, so CLI
   and plugin can't drift). The docs exist only so `doctor --fix` backfills the
   comments for EXISTING vaults that already carry a recap block. Defaults
   (min_sessions 6, min_frequency 2) verified against the plugin source
   `skills/recap/SKILL.md`. The `schedule:` block gains a header comment
   documenting the entry shape (a cron expression plus either `skill: /name`
   or `command` + `args`) — never per-entry comments. Adding a config key
   requires a `config_key_docs` entry: a completeness guard test walks every
   key path of the serde config structs and fails naming any path missing
   from the table (explicit allowlist: `qmd_collection`, legacy-deprecated).

3. **Full restructure for existing vaults** (maintainer chose full over
   comments-only). `restructure_config` reorders the top-level blocks into the
   section order and inserts the banners **without reparsing YAML**: it strips
   the structural lines (banners + managed note), segments the file into
   opaque top-level blocks (each block = its attached lead comments + header +
   indented/sequence body, trimmed of surrounding blanks), reorders them
   (known blocks by section order, unknown top-level keys kept in their
   original relative order after the known blocks and before `stats`), and
   re-assembles with canonical banners. Every value, user comment, inline
   comment, and the file's CRLF/LF style survive byte-for-byte. Stripping the
   structural lines before re-segmentation makes a second `--fix`
   byte-identical (idempotent). The restructure runs as the LAST step of the
   `config-values` recipe, after the value resets and comment backfill, so it
   canonicalises the fully-corrected text.

4. **Plain `doctor` reports drift read-only.** `config_layout_matches` (a
   restructure-equals-input check) drives a "config layout differs from
   template — doctor --fix will restructure" finding on the `config-values`
   row. `config_layout_matches` is conservative: the declined shapes (below)
   never report drift, so plain doctor can never promise a restructure that
   `--fix` would then decline — a unit test pins that agreement (the fix
   recipe still carries a defense-in-depth "restructure declined" un-fixable
   line for the theoretical mismatch).

5. **The stats stamp stays canonical.** `stamp_doctor_run` appends a bare
   `stats:` block when none exists; on a config that was ALREADY in canonical
   layout (e.g. a freshly-init'd vault) it re-runs the byte-preserving
   restructure after the stamp so the appended block gets its System banner +
   managed note — otherwise the next plain `doctor` would report drift the
   stamp itself introduced. A config that was already drifted is left as-is
   (plain doctor never restructures a user's legacy layout — that stays
   `--fix`'s job).

6. **Declined shapes and known limitations.** `restructure_config` declines —
   returns the file untouched, and never reports drift — exactly these root
   shapes, each already surfaced by the existing `onebrain.yml` validity
   checks rather than the layout check:
   - **invalid YAML** (parse error, including duplicate top-level keys, which
     `serde_yaml` rejects);
   - **a non-mapping root** (sequence / scalar / empty document);
   - **a flow-style root mapping** (`{a: 1, b: 2}` — a valid mapping, but it
     has no block-form top-level key lines to move; declined explicitly, not
     by accident of segmentation);
   - **a block mapping with no recognisable top-level keys**.

   Known cosmetic limitations of the line-oriented block mover (accepted;
   exotic shapes only):
   - a keep-chomped block scalar (`|+`) under an UNKNOWN top-level key can
     lose trailing blank value lines during block separation (blank trimming
     at block edges); template-known keys never carry block scalars;
   - a comment separated from its key by a blank line is not "attached lead
     comments" and travels with the PRECEDING block (position shifts;
     nothing is lost).

## Related

- [ADR 0025](0025-tier2-cross-encoder-reranker.md) — the reranker keys this
  template documents.
- `docs/configuration.md` — the full key-by-key reference.
- GitHub issue #196.
