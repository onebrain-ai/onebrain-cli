# 0027 — Collection cache layout split (`index/` + `models/`) with eager migration

- **Status:** accepted
- **Date:** 2026-07-11

## Context

A search collection's cache dir historically held everything **flat at its root**: the `tantivy/` BM25 index, the `vectors/` store, `engine.redb` metadata, hf-hub's `models--org--name` model dirs, and transient markers, all side by side. That made the two very different kinds of state indistinguishable to every consumer: the *index* (cheap to rebuild, wiped by `--force`, sized in `search status` as "your index") versus the *models* (multi-GB downloads, expensive to refetch, deliberately preserved across wipes). Sizing, wiping, backup guidance, and the planned cache-management commands (#201) all had to enumerate artifact names to tell them apart, and every new artifact risked being mis-classified by one of the consumers.

v3.4.9 (PR-5, #201) splits the collection root into `index/` (search artifacts) and `models/` (the hf-hub download cache base), with `reindex-progress.json` and other transient markers staying at the root. Existing installs have flat collections that must keep working — across mixed binary versions, live daemons holding the engine, and concurrent opens.

## Decision

- **One resolver owns the layout**: `onebrain_search::layout::CollectionLayout`. No production code joins `"tantivy"` / `"vectors"` / `"engine.redb"` / `models--*` paths by hand (a repo-wide sweep enforces zero stray joins); everything routes through `index_artifact()`, `model_dir()`, `models_base()`, `models_size_bytes()`, `index_size_bytes()`, `detect()`, `migrate()`.
- **Eager migration on every engine open** (the write path): `Engine::open_inner` runs `CollectionLayout::migrate()` before opening the stores. Per-entry same-volume `fs::rename` — instant, no re-embed, no data copy. `migrate()` also unconditionally creates `models/` + `index/`, because redb's `Database::create` does not create parent directories.
- **Per-artifact fallback resolution, used by every consumer** (`CollectionLayout::index_artifact` / `model_dir`): new location if present, else legacy root — and the resolvers themselves **never call `migrate()`**. This is per *artifact*, and for models per *model dir* (`model_dir(cache_dir_name)`): a "split-brain" model cache (model A still legacy at the root, model B already under `models/`) resolves each model to where it actually lives. A single shared probe base cannot represent that state — an earlier draft that fell back to the root "if any legacy model remains" misreported migrated models as not-downloaded (re-download risk) and was replaced by per-model resolution. That said, only paths that bypass `Engine::open` entirely are truly migration-free end to end — the model TUI/`search model list`, the hook lex gates, and the MCP/daemon lex-only paths. Consumers that *look* read-only but still call `Engine::open` — `search status`, `doctor`, and `session init`'s no-daemon fallback probe — DO trigger migration on first touch (see the tradeoff below).
- **`models_base()` is a write-path resolver only**: always `<root>/models`, used for hf-hub *downloads* (engine lazy embedder/reranker construction, TUI forced downloads) so fresh downloads always land in the new layout. Probes must use `model_dir()`.
- **`--force` wipes BOTH candidate locations** for each index artifact (split and legacy root). A lost migration race can leave an orphaned legacy duplicate; wiping only the resolved copy would let the next open resurrect the stale one via fallback.
- **SessionStart probe is daemon-first**: `session init`'s `native_pending` asks a live matching daemon for `pending_total` (no-retry, no-respawn probe) and reports unknown on any probe failure — it never does a local `Engine::open` (and therefore never migrates) while a daemon owns the collection.

## Accepted tradeoffs

- **Race semantics: skip-if-target-exists.** Concurrent `migrate()`s never error or clobber — the loser leaves its source entry in place. That can strand a legacy duplicate (see the dual-location wipe above); the duplicate is inert (resolution prefers the new location) and reaped by `--force`.
- **Partial failure stays functional.** A rename error aborts `migrate()` mid-loop with no rollback; the collection may sit in any mixed state. This is safe by construction: every read path resolves per-artifact, and retrying `migrate()` (the next engine open) is idempotent and picks up where it left off.
- **Silent migration triggers.** Any `Engine::open` migrates: `search reindex`, `search query`/`vsearch`/`get` (direct path), `onebrain mcp`, the daemon's held-engine open, `doctor`'s engine probe, `search status`'s read-only open, and — when no daemon is live — the SessionStart `session init` probe. The user never runs a "migrate" command; the first post-upgrade touch of a collection converts it. This is deliberate (zero-ceremony upgrade) and bounded by the rename-only cost.
- **Partial-failure re-download.** If a model dir is stuck at the legacy root after a failed migrate, downloads (which target `models/`) refetch rather than reuse the stranded copy — wasteful but correct, self-healing on the next successful migrate.
- **Mixed-binary hazard window.** A pre-v3.4.9 binary pointed at a migrated collection does not know about `index/`/`models/` and would see an "empty" collection (and could re-create flat artifacts next to the split ones, i.e. a new split-brain). One machine normally runs one CLI version, and the flat leftovers are healed by the next new-binary open + dual-location wipe; Task 9's real-vault audit documents this window. Downgrades across the split boundary are not supported beyond that self-healing.

## Consequences

- `search status` gains an honest Cache section: `models/` vs `index/` sizes are computed by the layout (each artifact counted wherever it lives, correct mid-migration) plus a `CacheLayoutState` (`split` / `legacy` / `partial`) field.
- Future cache-management commands (`cache ls/rm`, #201) get a stable, name-agnostic boundary: "the index" is `index/`, "the models" is `models/`, no artifact-name enumeration.
- The sweep rule (zero literal artifact-path joins outside `layout.rs` and test fixtures) is part of the contract: new consumers must go through `CollectionLayout`, keeping the layout changeable in exactly one place.
- Docs: [architecture/search.md](../architecture/search.md) §1 describes the on-disk layout and migration semantics.
