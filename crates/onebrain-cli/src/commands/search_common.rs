//! Shared helpers for the `search` verb group (`query` / `search` / `vsearch`
//! / `get` / `status` / `reindex`) — all backed by `onebrain_search::Engine`.
//!
//! ## Collection resolution
//!
//! The engine's on-disk state (tantivy index, vector store, redb metadata)
//! lives under `<data_dir>/onebrain/search/<collection>/`, keyed by the
//! `search.collection` config value (falling back to the legacy
//! `qmd_collection` — see `onebrain_core::load_vault_config`).
//!
//! When neither is set, the collection name is **auto-generated** as
//! `<vault-dir-name>-<short-hash>` — where `<short-hash>` is the first 6 hex
//! chars of sha256 of the vault's absolute path (see
//! [`onebrain_search::engine::short_path_hash`]) — and **persisted** to
//! `onebrain.yml` under `search.collection`. This is:
//! - **deterministic** per vault path (same vault → same name across runs),
//! - **stable** once written (the persisted value is read on the next run, so
//!   moving/renaming the vault won't silently re-derive a different name and
//!   split the index),
//! - **headless-safe** (no prompt) so agents and cron "just work" on a fresh
//!   machine, and
//! - **visible + editable** in `onebrain.yml` afterwards.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use onebrain_core::{
    load_vault_config, load_vault_config_at, RerankerConfig, ResolvedVault, VaultRoot,
};
use onebrain_search::engine::{short_path_hash, Engine, RerankSettings, DEFAULT_RERANK_MIN_SCORE};
use onebrain_search::lex::LexIndex;
use onebrain_search::CollectionLayout;

use crate::vault_ctx;

/// Base directory for all native-search state:
/// `<data_dir>/onebrain/search/`. Shares the same root as
/// `crate::migration::default_state_dir` (`~/Library/Application Support/onebrain/`
/// on macOS · `$XDG_DATA_HOME/onebrain/` or `~/.local/share/onebrain/` on Linux ·
/// `%APPDATA%\onebrain\` on Windows — relocated out of the OS-purgeable cache
/// dir in v3.4.5, issue #114), honouring the same `ONEBRAIN_CACHE_DIR` test
/// override so search-engine tests can redirect index state into a tempdir
/// exactly like the migration-notice tests do.
pub fn search_cache_root() -> PathBuf {
    crate::migration::default_state_dir().join("search")
}

/// Resolve the vault and its search collection name, without opening the
/// engine. When no collection is configured (neither `search.collection` nor
/// the legacy `qmd_collection`), auto-generate `<dir>-<hash>` and persist it
/// to `onebrain.yml` so it is stable and editable — see the module doc.
///
/// Returns `Some(name)` in the normal case. It only returns `None` if the
/// config had no collection AND persisting the generated name failed (the
/// generated name is still returned to the caller in that case — the `None`
/// path is unreachable in practice; kept `Option` for signature stability).
pub fn resolve_collection(vault_flag: Option<PathBuf>) -> Result<(ResolvedVault, Option<String>)> {
    let resolved = vault_ctx::require(vault_flag)?;
    let collection = collection_for(&resolved)?;
    Ok((resolved, Some(collection)))
}

/// The effective collection name for a resolved vault: the configured value
/// if present, otherwise a freshly generated `<dir>-<hash>` name persisted to
/// `onebrain.yml`. Deterministic and headless-safe (never prompts).
///
/// The configured value comes from `search.collection`, falling back to the
/// deprecated top-level `qmd_collection` (v3.3 and earlier) via
/// [`load_vault_config`]. That legacy read-fallback stays so vaults that
/// haven't migrated keep working, but it is transitional: `onebrain doctor
/// --fix` migrates `qmd_collection` → `search.collection` and removes the old
/// key (see `LegacyQmdCollectionCheck`).
pub fn collection_for(resolved: &ResolvedVault) -> Result<String> {
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    if let Some(existing) = config.search.collection {
        return Ok(existing);
    }

    let name = generate_collection_name(resolved.root.name(), resolved.root.as_path());
    persist_collection(resolved.root.as_path(), &name)
        .with_context(|| format!("persisting search.collection = {name}"))?;
    Ok(name)
}

/// Read-only collection resolver for surfaces that must NOT mutate config
/// (the webui `GET /api/vault/search`). Returns the configured value
/// (`search.collection`, else the legacy `qmd_collection`), or the
/// deterministic auto-generated `<dir>-<hash>` name **without persisting**
/// it. The generated name matches what [`collection_for`] would later write,
/// so a webui search before the first `search reindex` uses the same
/// collection reindex will adopt.
pub fn collection_name_readonly(vault_root: &Path) -> Result<String> {
    let config = load_vault_config_at(vault_root).context("load vault config")?;
    if let Some(existing) = config.search.collection {
        return Ok(existing);
    }
    let root = VaultRoot::from_path(vault_root)
        .with_context(|| format!("resolving vault root at {}", vault_root.display()))?;
    Ok(generate_collection_name(root.name(), vault_root))
}

/// Build the auto-generated collection name `<dir>-<short-hash>` for a vault
/// whose directory name is `dir_name` and absolute path is `abs_path`.
fn generate_collection_name(dir_name: String, abs_path: &Path) -> String {
    format!("{dir_name}-{}", short_path_hash(abs_path))
}

/// Collection's on-disk cache dir: `<search_cache_root>/<collection>/`. This
/// is the collection ROOT — the parent of the split `index/` + `models/`
/// layout, unchanged by the split (see [`CollectionLayout`]).
pub fn collection_cache_dir(collection: &str) -> PathBuf {
    search_cache_root().join(collection)
}

/// The hf-hub model DOWNLOAD cache base for a collection whose root is
/// `cache_dir`: always `<cache_dir>/models` (see
/// [`CollectionLayout::models_base`]). Feed this to download-triggering
/// calls (`embed::new*` / `rerank::new`) so fresh downloads land in the
/// split layout. Read-only "is this model downloaded?" probes must NOT use
/// it — `model_download_status` / `reranker_download_status` take the
/// collection ROOT and resolve per model with legacy fallback internally.
pub(crate) fn models_cache_dir(cache_dir: &Path) -> PathBuf {
    CollectionLayout::new(cache_dir).models_base()
}

/// Resolve one of the collection's index artifacts (`"tantivy"`, `"vectors"`,
/// or `"engine.redb"`) through the split layout: `<cache_dir>/index/<name>`
/// once migrated, legacy `<cache_dir>/<name>` while it hasn't moved yet. This
/// is the read-only resolution every consumer OUTSIDE `Engine::open` (which
/// migrates eagerly) must use to find artifacts — presence checks and direct
/// `LexIndex::open`s alike. Pure path resolution; never mutates (no
/// `migrate()`).
pub(crate) fn index_artifact_path(cache_dir: &Path, name: &str) -> PathBuf {
    CollectionLayout::new(cache_dir).index_artifact(name)
}

/// Open a collection's lex index for the read-only fast paths (`search
/// search`, the MCP lex sub-query) that deliberately bypass redb and the
/// embedder.
///
/// Those paths cannot self-heal a stale tantivy schema on their own: refilling
/// an emptied lex index needs the chunk text, which lives in the engine's redb.
/// So on — and only on — a schema mismatch, this opens the engine once
/// (`Engine::open` wipes and repopulates the lex index, touching no vault files
/// and loading no model), drops it to release the collection lock, and retries.
/// Migration therefore stays invisible on every surface instead of only the
/// engine-backed ones. Any other error propagates untouched.
///
/// Uses [`open_engine_from_resolved`] for the healing open, so a vault with no
/// `search.collection` gets the generated name PERSISTED to `onebrain.yml` —
/// acceptable on these two CLI surfaces, which already resolve their collection
/// through the persisting [`collection_for`]. Surfaces that must not rewrite
/// config (the webui `GET /api/vault/search`) use
/// [`open_lex_migrating_with_collection`] instead.
pub(crate) fn open_lex_migrating(cache_dir: &Path, resolved: &ResolvedVault) -> Result<LexIndex> {
    open_lex_migrating_inner(cache_dir, &mut || {
        // The engine is dropped at the end of this closure, releasing the
        // collection lock before the retried `LexIndex::open`.
        open_engine_from_resolved(resolved).map(|_| ())
    })
}

/// Non-persisting twin of [`open_lex_migrating`] for read-only surfaces that
/// already resolved their collection through [`collection_name_readonly`] — the
/// webui `GET /api/vault/search` lex path (`server::search::run_lex`).
///
/// Same self-heal, but the healing open goes through
/// [`open_engine_with_collection`] rather than [`open_engine_from_resolved`],
/// so a vault whose index dir exists while `search.collection` is absent is NOT
/// silently rewritten: `collection_for`'s persist re-serializes the whole
/// `onebrain.yml` through serde and strips the commented template's comments.
/// Exactly the reasoning behind `open_engine_with_collection` itself — the
/// migration must be invisible on this surface in BOTH directions (it heals the
/// index without touching the config).
///
/// Takes the vault ROOT rather than a `ResolvedVault` so the resolution (a
/// config read + parse) happens INSIDE the heal closure — i.e. only on the rare
/// migration, never on the common as-you-type request this path exists to serve
/// fast.
pub(crate) fn open_lex_migrating_with_collection(
    cache_dir: &Path,
    vault_root: &Path,
    collection: &str,
) -> Result<LexIndex> {
    open_lex_migrating_inner(cache_dir, &mut || {
        let resolved = crate::vault_ctx::require(Some(vault_root.to_path_buf()))?;
        open_engine_with_collection(&resolved, collection).map(|_| ())
    })
}

/// How many times the self-heal attempts the engine open before giving up, and
/// the backoff before each retry. The migration takes redb's single-process
/// collection lock, so N processes racing the first post-upgrade search (the
/// agent's MCP lex sub-queries alongside the PostToolUse reindex hook) leave
/// N-1 losers staring at `E_ENGINE_BUSY` (exit 77) for what is a purely
/// transient, self-resolving condition.
///
/// Bounded and small on purpose: one wait per entry below, so the worst case
/// adds ~200 ms to one already-degraded request and NEVER loops unbounded. A
/// still-busy engine after that is surfaced honestly as `E_ENGINE_BUSY`, and
/// any non-busy failure is surfaced immediately, unretried.
const LEX_MIGRATE_RETRY_BACKOFF: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(50),
    std::time::Duration::from_millis(150),
];

/// Shared body of the two migrating opens. `heal` opens (and immediately
/// drops) the engine — `Engine::open` is what performs the wipe + repopulate,
/// and dropping it releases the collection lock. The caller's choice of
/// persisting / non-persisting engine open is the ONLY difference between the
/// two public wrappers; taking it as a closure also lets the retry policy be
/// unit-tested deterministically.
fn open_lex_migrating_inner(
    cache_dir: &Path,
    heal: &mut dyn FnMut() -> Result<()>,
) -> Result<LexIndex> {
    let tantivy_dir = index_artifact_path(cache_dir, "tantivy");
    match LexIndex::open(&tantivy_dir) {
        // B-R1: a plain open SUCCEEDS on the leftovers of an INTERRUPTED
        // migration — `open_or_reset` recreates the wiped dir immediately, so
        // what a crash leaves behind has a *current, matching* schema and is
        // simply EMPTY. No error path can tell that apart from a healthy
        // index; only the rebuild marker can, and only `Engine::open` acts on
        // it. So the read-only fast paths must consult it themselves, or a
        // Ctrl-C'd rebuild leaves keyword search silently returning zero hits
        // (`ok: true`, exit 0) until some unrelated engine-opening command
        // happens to run. See [`is_abandoned_rebuild`] for why the marker
        // ALONE is not the condition.
        Ok(lex) if is_abandoned_rebuild(&tantivy_dir, &lex) => {
            // Release the read handle before the healing engine open.
            drop(lex);
            heal_with_retry(&tantivy_dir, heal)
        }
        Ok(lex) => Ok(lex),
        Err(err) if onebrain_search::lex::is_schema_mismatch(&err) => {
            heal_with_retry(&tantivy_dir, heal)
        }
        Err(err) => Err(err),
    }
}

/// True for the ONE state a rebuild marker makes unservable: the index opens
/// cleanly under the current schema, a rebuild is still marked pending, and the
/// index is EMPTY. That is B-R1 — an interrupted wipe+repopulate, whose
/// leftovers are indistinguishable from a healthy index except for the marker,
/// and which really must be healed before anything is served.
///
/// A marker over a NON-EMPTY index is a different animal entirely, and healing
/// it is at best pointless and at worst a hard failure:
///
///  * **D2** (`chunk_meta` empty, lex populated) — `repopulate_lex_from_meta`
///    REFUSES rather than erase the last surviving copy, and deliberately
///    leaves the marker in place. The marker is therefore PERMANENT by design,
///    so healing can only ever repeat the same refusal.
///  * **A post-commit marker-clear failure** (read-only `index/`, exotic FS) —
///    the rebuild already committed; only the bookkeeping removal failed.
///  * **A crash before the repopulate's single commit** — the previous index
///    survives byte-for-byte, so keyword search still answers (possibly with
///    duplicates, which `doctor`'s `lex-index` check reports and `--fix`
///    repairs).
///
/// Gating on the marker alone made all three fail CLOSED. `Engine::open`
/// returns `EngineBusy` for any other live handle on the collection —
/// *including one in the same process* — and `server::search::run_lex` runs
/// INSIDE the daemon, which already holds `state.search_engine`. So a
/// permanently-pending marker turned every `mode=lex` request into a guaranteed
/// busy error after ~200 ms of pointless backoff, on exactly the states where
/// the lex index is the last readable copy of the data. Serving a degraded
/// index beats refusing a readable one.
///
/// A `num_docs()` that itself fails is treated as "not the B-R1 state": the
/// index is then broken in a way a repopulate cannot be trusted to fix, and the
/// error surfaces honestly on the search that follows instead of being turned
/// into a wipe.
fn is_abandoned_rebuild(tantivy_dir: &Path, lex: &LexIndex) -> bool {
    onebrain_search::lex::rebuild_pending(tantivy_dir) && matches!(lex.num_docs(), Ok(0))
}

/// Run the healing engine open (which wipes + repopulates the lex index from
/// redb `chunk_meta`), retrying a transiently busy collection lock, then hand
/// back the freshly opened lex index. Shared by both routes into the heal: a
/// stale schema, and a rebuild left pending by an interrupted migration.
///
/// The bound is the backoff table itself: every iteration either returns or
/// consumes one entry, and an exhausted table returns the busy error. So the
/// loop attempts `heal` at most `LEX_MIGRATE_RETRY_BACKOFF.len() + 1` times and
/// cannot fall out of the bottom — no unreachable tail to keep honest.
fn heal_with_retry(tantivy_dir: &Path, heal: &mut dyn FnMut() -> Result<()>) -> Result<LexIndex> {
    let mut backoffs = LEX_MIGRATE_RETRY_BACKOFF.iter();
    loop {
        match heal() {
            Ok(()) => return LexIndex::open(tantivy_dir),
            // Lost the race for the collection lock. Whoever won it is
            // very likely performing this exact migration, so back off
            // and retry rather than surfacing a transient
            // `E_ENGINE_BUSY` on the first post-upgrade query.
            Err(e) if is_engine_busy_error(&e) => {
                let Some(backoff) = backoffs.next() else {
                    return Err(e);
                };
                std::thread::sleep(*backoff);
                // Cheap win first: if the winner already committed the
                // migration, the plain open now succeeds and we never need the
                // engine (or its lock) at all. Refused only for the ONE state
                // that would reintroduce B-R1 through the back door — a race
                // winner that died mid-rebuild, leaving an index that opens
                // perfectly well and is perfectly empty. Same predicate as the
                // fast path above, for the same reason: a marker over a
                // POPULATED index (a D2 refusal the winner just hit) is
                // permanent, so rejecting the win here would only march us into
                // the busy error it exists to avoid.
                if let Ok(lex) = LexIndex::open(tantivy_dir) {
                    if !is_abandoned_rebuild(tantivy_dir, &lex) {
                        return Ok(lex);
                    }
                }
            }
            // A genuine failure (corrupt redb, IO error, …) is never
            // retried and never swallowed.
            Err(e) => return Err(e),
        }
    }
}

/// True when `err` is the collection-lock contention that surfaces as
/// `E_ENGINE_BUSY` / exit 77, whichever of the two classifications it carries:
/// the typed [`onebrain_search::error::EngineBusy`] straight off `Engine::open`,
/// or the [`onebrain_core::CoreError::EngineBusy`] that
/// [`map_engine_open_error`] re-wraps it into (dropping the original typed head
/// from the chain, which is why one check alone is not enough).
pub(crate) fn is_engine_busy_error(err: &anyhow::Error) -> bool {
    err.chain().any(|c| {
        c.downcast_ref::<onebrain_core::CoreError>()
            .is_some_and(|e| matches!(e, onebrain_core::CoreError::EngineBusy(_)))
    }) || onebrain_search::error::is_engine_busy(err)
}

/// Resolve the vault and open the engine rooted at its collection's cache
/// dir. Returns the resolved vault (for building the envelope's `vault`
/// field) alongside the opened engine.
///
/// The collection name is resolved via [`collection_for`]: configured value
/// if present, else an auto-generated `<dir>-<hash>` persisted to
/// `onebrain.yml`. This makes a fresh `onebrain search reindex` / `status`
/// "just work" with no manual config and no prompt.
pub fn open_engine(vault_flag: Option<PathBuf>) -> Result<(Engine, ResolvedVault)> {
    let resolved = vault_ctx::require(vault_flag)?;
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    let collection = collection_for(&resolved)?;

    let cache_dir = collection_cache_dir(&collection);
    let mut engine = Engine::open(&cache_dir, &config.search.embed_model)
        .map_err(|e| map_engine_open_error(e, &cache_dir))?;
    engine.set_exclude_patterns(config.search.exclude.clone());
    engine.set_rerank_settings(rerank_settings_from_config(&config.search.reranker));
    Ok((engine, resolved))
}

/// Open the engine for an ALREADY-RESOLVED vault, without re-resolving it via
/// [`vault_ctx::require`]. A variant of [`open_engine`] for callers who
/// already hold a `ResolvedVault` (e.g. because they needed it earlier in the
/// same call for other purposes) — re-resolving would call
/// [`vault_ctx::require`] a second time AND route back through
/// [`collection_for`], which persists the auto-generated collection name to
/// `onebrain.yml` a second time when the config had no `search.collection`
/// set. This calls [`collection_for`] exactly once instead.
///
/// Returns the resolved vault back alongside the engine (cloned from the
/// input) so callers can destructure it the same way [`open_engine`]'s
/// callers do.
pub fn open_engine_from_resolved(resolved: &ResolvedVault) -> Result<(Engine, ResolvedVault)> {
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    let collection = collection_for(resolved)?;

    let cache_dir = collection_cache_dir(&collection);
    let mut engine = Engine::open(&cache_dir, &config.search.embed_model)
        .map_err(|e| map_engine_open_error(e, &cache_dir))?;
    engine.set_exclude_patterns(config.search.exclude.clone());
    engine.set_rerank_settings(rerank_settings_from_config(&config.search.reranker));
    Ok((engine, resolved.clone()))
}

/// Open the engine for an ALREADY-RESOLVED collection name **without any
/// config persistence** — the doctor read path. Unlike [`open_engine`], this
/// never calls [`collection_for`], so a vault whose index dir exists while
/// its `search.collection` key is absent (e.g. after a hand edit) is NOT
/// silently rewritten — `collection_for`'s persist goes through a whole-file
/// serde re-serialization that would strip the commented template's
/// comments. Pair it with [`collection_name_readonly`].
pub fn open_engine_with_collection(resolved: &ResolvedVault, collection: &str) -> Result<Engine> {
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    let cache_dir = collection_cache_dir(collection);
    let mut engine = Engine::open(&cache_dir, &config.search.embed_model)
        .map_err(|e| map_engine_open_error(e, &cache_dir))?;
    engine.set_exclude_patterns(config.search.exclude.clone());
    engine.set_rerank_settings(rerank_settings_from_config(&config.search.reranker));
    Ok(engine)
}

/// Map the config's `search.reranker` block to the engine-facing
/// [`RerankSettings`] — the CLI is the only layer that reads config files, so
/// this is the single seam where `onebrain.yml` values become the engine's
/// rerank knobs. `min_score` falls back to the engine's calibrated
/// [`DEFAULT_RERANK_MIN_SCORE`] when the config leaves it unset.
///
/// Drift guard: an absent `search.reranker` block must map to EXACTLY
/// `RerankSettings::default()` — see the test below. If the two defaults
/// ever diverge (one crate's default changes without the other), this
/// mapping silently stops being a no-op for the common case.
pub(crate) fn rerank_settings_from_config(cfg: &RerankerConfig) -> RerankSettings {
    RerankSettings {
        enabled: cfg.enabled,
        model: cfg.model.clone(),
        min_candidates: cfg.min_candidates,
        min_score: cfg.min_score.unwrap_or(DEFAULT_RERANK_MIN_SCORE),
    }
}

/// Turn an `Engine::open` failure into the right typed CLI error. The redb
/// single-process lock case (already classified as `onebrain_search`'s
/// `EngineBusy`) becomes `CoreError::EngineBusy`, which the envelope + exit
/// mapping render as `E_ENGINE_BUSY` / exit 77. Every other failure keeps the
/// prior behaviour: a plain `.context(...)`-wrapped opaque error. This is the
/// single choke point so `query`, `vsearch`, `get`, and a full `reindex` all
/// report lock contention uniformly (v3.4.6).
pub(crate) fn map_engine_open_error(err: anyhow::Error, cache_dir: &Path) -> anyhow::Error {
    if onebrain_search::error::is_engine_busy(&err) {
        return anyhow::Error::new(onebrain_core::CoreError::EngineBusy(format!(
            "index at {} is locked by another process (e.g. the `onebrain mcp` server) — \
             retry once it releases the lock",
            cache_dir.display()
        )));
    }
    err.context(format!("opening search engine at {}", cache_dir.display()))
}

/// Turn a warm-daemon request failure into the right typed CLI error — the
/// daemon-path analogue of [`map_engine_open_error`]. When the daemon 503s
/// because it holds no engine (another process owns the redb lock), the client
/// classifies the error as `onebrain_search`'s `EngineBusy`; convert it to the
/// same `CoreError::EngineBusy` (E_ENGINE_BUSY / exit 77) the direct path uses,
/// so `query` / `get` / `reindex` report contention identically whether they
/// went direct or through the daemon. Any other daemon error keeps its verb
/// context (an opaque `E_INTERNAL`).
pub(crate) fn map_daemon_error(err: anyhow::Error, ctx: &'static str) -> anyhow::Error {
    if onebrain_search::error::is_engine_busy(&err) {
        return anyhow::Error::new(onebrain_core::CoreError::EngineBusy(
            "the search index is held by another process (e.g. an `onebrain mcp` \
             server, possibly from before an upgrade) — retry once it releases the engine"
                .to_string(),
        ));
    }
    err.context(ctx)
}

/// Persist `search.collection = collection` into the vault's config file
/// (`onebrain.yml`, or legacy `vault.yml` if that's what's present),
/// preserving every other key. Thin wrapper over the shared
/// `onebrain_fs::persist_search_key` (read → mutate `search.*` → backup →
/// atomic-write; `backup_config_file` is a hard precondition), which
/// `search_model::persist_embed_model` also uses.
fn persist_collection(vault_root: &Path, collection: &str) -> Result<()> {
    onebrain_fs::persist_search_key(vault_root, "collection", collection)
}

/// `true` when the collection's cache dir already exists on disk. Used as a
/// read-only pre-check by `session_init::native_pending` and `doctor` to skip
/// opening the engine (which would create the dir as a side effect) on a
/// never-indexed vault. Note: `search status` does NOT use this — it derives
/// its `indexed` field from `doc_count > 0`, since a cache dir can exist while
/// holding zero docs. Pure path check — never touches the engine, so it never
/// triggers a model download.
pub fn is_indexed(cache_dir: &Path) -> bool {
    cache_dir.is_dir()
}

/// Total on-disk bytes of the index itself under `cache_dir`: the `tantivy/`
/// and `vectors/` directories plus the `engine.redb` file, resolved through
/// the split layout so each artifact is counted wherever it actually lives
/// (new `index/` location or legacy root, mid-migration split included). The
/// downloaded `models--*` dirs are deliberately excluded — those are the
/// model's size, reported separately. Returns `None` when none of the three
/// exist yet (no index). Pure fs, never mutates (no `migrate()`).
pub fn index_size_bytes(cache_dir: &Path) -> Option<u64> {
    let layout = CollectionLayout::new(cache_dir);
    let any = onebrain_search::layout::INDEX_ARTIFACTS
        .iter()
        .any(|name| layout.index_artifact(name).exists());
    any.then(|| layout.index_size_bytes())
}

/// `true` when the vault's config file physically contains a
/// `search.embed_model` key. Distinct from `config.search.embed_model`, which
/// serde fills with the `multilingual-e5-small` default even when the key is
/// absent — so this raw-YAML check is the only way to tell "user has chosen a
/// model" from "user hasn't touched it yet".
///
/// Missing config / unreadable file / non-mapping YAML all count as "not
/// present" (a fresh vault hasn't chosen a model). Pure `std::fs` + parse.
pub fn embed_model_key_present(vault_root: &Path) -> bool {
    use onebrain_core::{find_config_file, CONFIG_FILENAME};

    let path = find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(yaml) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return false;
    };
    yaml.get("search")
        .and_then(|s| s.get("embed_model"))
        .is_some()
}

/// `true` when at least one registry model's `models--*` dir exists under the
/// collection cache dir. Pure `std::fs` scan — never downloads.
pub fn any_model_downloaded(cache_dir: &Path) -> bool {
    use onebrain_search::embed::{model_download_status, model_registry};
    model_registry()
        .iter()
        .any(|m| model_download_status(m, cache_dir).downloaded)
}

/// Whether the vault has *no* embedding model chosen yet: neither a
/// physically-present `search.embed_model` key NOR any downloaded model. This
/// is the gate for the first-run "pick a model before we download" prompt in
/// `search reindex` — a `false` here means the user has already committed to a
/// model (explicitly configured or already on disk), so no prompt is shown.
pub fn model_not_chosen(vault_root: &Path, cache_dir: &Path) -> bool {
    !embed_model_key_present(vault_root) && !any_model_downloaded(cache_dir)
}

/// Reconcile a stale model choice after a cache purge (or a manual model
/// deletion): when `search.embed_model` names a model that is no longer on
/// disk, drop the key so the vault reverts to "no model chosen". `search
/// reindex` then re-prompts for a selection and the UI stops marking a
/// not-downloaded model as active.
///
/// Race-safe + best-effort — does nothing when a reindex is in progress (it
/// downloads the model, so the dir is legitimately absent mid-run), when no
/// model key is committed, or when the configured model IS downloaded.
/// Reconciliation is advisory, never fatal: a write failure is logged to
/// stderr (not swallowed) and a successful drop is announced so the config
/// mutation is never silent. Call only from mutating surfaces (`search model`
/// TUI, `search reindex`) — NOT the read-only `search status` / `search model
/// list` paths.
pub(crate) fn reconcile_missing_model(vault_root: &Path, cache_dir: &Path, embed_model: &str) {
    use onebrain_search::embed::{model_download_status, model_registry};

    if read_reindex_progress(cache_dir).is_some() || !embed_model_key_present(vault_root) {
        return;
    }
    let downloaded = model_registry()
        .iter()
        .find(|m| m.name == embed_model)
        .is_some_and(|m| model_download_status(m, cache_dir).downloaded);
    if !downloaded {
        // Surface the outcome instead of discarding it (`let _ =` hid a real
        // config mutation): a removal is announced, a failure is logged.
        match onebrain_fs::remove_search_key(vault_root, "embed_model") {
            Ok(true) => eprintln!(
                "search: cleared stale search.embed_model = {embed_model} (model not downloaded)"
            ),
            Ok(false) => {}
            Err(e) => eprintln!(
                "⚠ Could not clear the stale search.embed_model setting — {e:#} (harmless; the \
                 reindex isn't affected)\n\
                 💡 if this repeats, check that onebrain.yml is writable"
            ),
        }
    }
}

/// Human-readable byte size (`471 MB`, `2.2 GB`, `12 B`) — whole-number MB/KB,
/// one decimal for GB. Shared by `search status` and `search model`
/// (`search reindex` keeps a local one-decimal-MB variant: its size DELTAS
/// like `+1.2 MB` need the precision).
pub(crate) fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Warm-daemon routing (v3.4.6 Track 2c)
// ─────────────────────────────────────────────────────────────────────────
//
// redb is single-process: exactly ONE process (the warm daemon) may open a
// collection's engine at a time (ADR 0023). So when a daemon is already
// running — typically because an `onebrain mcp` session holds the engine — the
// CLI search verbs route their request through it (over the existing localhost
// HTTP surface) instead of opening a second engine and hitting the redb lock
// (`E_ENGINE_BUSY`). When no daemon is running there's no contention, so the
// verbs open the engine directly (today's path), and T1's honest `EngineBusy`
// still applies if the lock is genuinely held by something else.

use crate::commands::daemon_client::{self, DaemonHandle};

/// `true` when daemon routing is turned OFF via `ONEBRAIN_NO_DAEMON` (any
/// non-empty value). The escape hatch: with it set, every search verb / hook
/// opens the engine directly and never discovers or starts a daemon — the
/// pre-daemon behaviour. Used by tests that must exercise the direct-open /
/// honest-`EngineBusy` path deterministically, and as an operator kill-switch.
pub(crate) fn daemon_routing_disabled() -> bool {
    std::env::var_os("ONEBRAIN_NO_DAEMON").is_some_and(|v| !v.is_empty())
}

/// Discover an already-running daemon and return a handle ONLY when it is
/// **live, at our version, AND bound to the vault the CLI is targeting**;
/// otherwise `None` (the caller opens the engine directly).
///
/// This is the CLI's **PASSIVE** routing check — it uses
/// [`daemon_client::discover_matching`], NOT #170's active
/// [`daemon_client::discover`]/[`ensure_running`]. The distinction is
/// load-bearing:
/// - `discover`/`ensure_running` are the MCP path's ACTIVE lifecycle owner:
///   on a version- or vault-mismatch they STOP + restart the daemon for the
///   caller's vault (one daemon per machine; switching vaults restarts it).
/// - `onebrain search …` must NEVER kill a daemon serving another vault — that
///   would disrupt a live MCP session. So the CLI reads `daemon.json`'s stored
///   canonical `vault` and, on ANY mismatch (wrong vault, wrong version, dead,
///   or a pre-vault-field record), simply routes to DIRECT open — it never
///   restarts and never spawns.
///
/// The result: a plain `onebrain search …` routes to the warm daemon only when
/// it already serves this exact vault (the contention case an MCP session
/// creates), and otherwise opens the engine directly so T1's honest
/// `E_ENGINE_BUSY` still applies if the lock is genuinely held.
pub(crate) fn route_to_daemon(resolved: &ResolvedVault) -> Option<DaemonHandle> {
    if daemon_routing_disabled() {
        return None;
    }
    match daemon_client::discover_matching(Some(resolved.root.as_path())) {
        Ok(handle) => handle,
        Err(e) => {
            tracing::debug!(error = %e, "daemon discover_matching failed; opening engine directly");
            None
        }
    }
}

/// Normalize a user-supplied doc path to the index's key form: absolute paths
/// under `vault_root` are made vault-relative, `./` prefixes are stripped, and
/// platform back-slashes become forward slashes. Anything else passes through
/// unchanged. Idempotent on an already-relative key.
///
/// The SINGLE source of truth for path→index-key normalization, shared by every
/// surface that keys the already-sent ledger (design §3b, #255): `search get`,
/// MCP `get`, the read-hook `token check` (Direct leg), and the daemon
/// `/api/token/ledger/check` route. They MUST agree byte-for-byte on the `path`
/// component of the ledger key `(session, doc_path, hash)` — the read-hook is
/// invoked with an ABSOLUTE path (Claude Code's Read tool always passes
/// absolute), while `search get` records the RELATIVE path. Without one shared
/// normalizer the two never collide and cross-surface dedup silently never
/// fires (and `engine.doc_hash`/`engine.get` would miss the relative index key
/// entirely on an absolute path).
///
/// ## Symlink boundaries (#268, R3 follow-up to #255)
///
/// The lexical `strip_prefix` above is a byte-for-byte comparison — it has no
/// notion of symlinks. When the incoming absolute path and `vault_root` were
/// resolved via different routes that disagree across a symlinked path
/// component (macOS `/tmp` → `/private/tmp`, `/var` → `/private/var`, or a
/// user-symlinked vault), the lexical strip fails even though the two paths
/// point at the same file, and the ledger key falls back to the raw absolute
/// path — silently failing the read-hook gate OPEN.
///
/// To stay fast on the overwhelmingly common case (spellings already agree),
/// canonicalization is only attempted as a fallback, and only when `input` is
/// absolute — canonicalizing a relative path would resolve it against the
/// process `cwd`, which has nothing to do with the vault and would produce a
/// wrong key. If canonicalization errors (path doesn't exist, permissions,
/// …) or the canonical strip also fails, this falls back to the original
/// lexical result — never panics, never guesses a wrong key.
pub(crate) fn normalize_doc_path(input: &str, vault_root: &Path) -> String {
    let p = Path::new(input);
    match strip_vault_prefix(p, vault_root) {
        Some(rel) => finish_normalize(&rel),
        None => finish_normalize(p),
    }
}

/// Strip `vault_root` from `input`, returning the vault-relative remainder, or
/// `None` when `input` is not under the vault.
///
/// The SINGLE symlink-aware prefix-strip shared by every surface that must
/// decide "is this absolute path under the vault, and if so what's its
/// vault-relative form?" — [`normalize_doc_path`] (the ledger-key normalizer)
/// and `search get`'s outside-vault guard. Keeping one implementation avoids a
/// third divergent lexical-vs-canonicalize copy drifting out of sync (#268).
///
/// Algorithm (same fast-path/fallback shape as the ledger normalizer):
/// 1. **Lexical `strip_prefix`** first — the hot path, no filesystem I/O when
///    the spellings already agree.
/// 2. On a lexical miss **and only when `input` is absolute**, `fs::canonicalize`
///    BOTH sides (resolving symlink boundaries like macOS `/tmp` →
///    `/private/tmp`) and retry the strip. Relative inputs are excluded:
///    canonicalizing a relative path resolves it against the process `cwd`,
///    which is unrelated to the vault.
/// 3. `None` when the strip fails on both routes, or when either canonicalize
///    errors (path doesn't exist, permissions, …) — callers stay fail-open.
pub(crate) fn strip_vault_prefix(input: &Path, vault_root: &Path) -> Option<PathBuf> {
    if let Ok(rel) = input.strip_prefix(vault_root) {
        return Some(rel.to_path_buf());
    }
    if !input.is_absolute() {
        return None;
    }
    let canon_input = fs::canonicalize(input).ok()?;
    let canon_root = fs::canonicalize(vault_root).ok()?;
    canon_input
        .strip_prefix(&canon_root)
        .map(Path::to_path_buf)
        .ok()
}

/// Shared tail of [`normalize_doc_path`]: forward-slash the path and trim a
/// leading `./`.
fn finish_normalize(rel: &Path) -> String {
    let s = rel.to_string_lossy().replace('\\', "/");
    s.trim_start_matches("./").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Total engine-open attempts the retry policy allows: one per backoff
    /// entry, plus the final attempt that has no wait left after it.
    ///
    /// Test-only, and derived from the same table the production loop consumes
    /// so the two can never drift. `heal_with_retry` does not read a count at
    /// all — exhausting [`LEX_MIGRATE_RETRY_BACKOFF`] IS its bound — so
    /// restating the policy here is what pins it.
    const LEX_MIGRATE_OPEN_ATTEMPTS: usize = LEX_MIGRATE_RETRY_BACKOFF.len() + 1;

    #[test]
    fn normalize_doc_path_strips_vault_root_and_dot_prefix() {
        let root = Path::new("/vault/ob-1");
        assert_eq!(
            normalize_doc_path("/vault/ob-1/00-inbox/note.md", root),
            "00-inbox/note.md"
        );
        assert_eq!(
            normalize_doc_path("./00-inbox/note.md", root),
            "00-inbox/note.md"
        );
        // Already-relative key → idempotent (unchanged).
        assert_eq!(
            normalize_doc_path("00-inbox/note.md", root),
            "00-inbox/note.md"
        );
        // Absolute path OUTSIDE the vault passes through unchanged.
        assert_eq!(
            normalize_doc_path("/elsewhere/x.md", root),
            "/elsewhere/x.md"
        );
    }

    /// #268 (R3 follow-up to #255): the lexical `strip_prefix` in
    /// `normalize_doc_path` fails silently when the incoming absolute path and
    /// `vault_root` disagree across a SYMLINKED path component — e.g. macOS
    /// `/tmp` → `/private/tmp`, `/var` → `/private/var`, or a user-symlinked
    /// vault. On failure it falls back to `unwrap_or(p)`, returning the raw
    /// absolute path instead of the relative index key, which makes the
    /// already-sent ledger (#255) miss and the read-hook gate fail OPEN.
    ///
    /// Build a real symlink in a tempdir (not relying on the exact `/tmp` ↔
    /// `/private/tmp` mapping, which is macOS-specific and not guaranteed
    /// stable), then normalize the "through-the-symlink" spelling of a doc
    /// path against the canonical vault_root. Before the fix this returns the
    /// raw absolute (unrelativized) path; after the fix it must return the
    /// relative key.
    #[cfg(unix)]
    #[test]
    fn normalize_doc_path_resolves_symlinked_vault_root() {
        let base = tempfile::tempdir().unwrap();
        let real_vault = base.path().join("real-vault");
        std::fs::create_dir_all(real_vault.join("00-inbox")).unwrap();
        let doc = real_vault.join("00-inbox/note.md");
        std::fs::write(&doc, "body").unwrap();

        let link_vault = base.path().join("linked-vault");
        std::os::unix::fs::symlink(&real_vault, &link_vault).unwrap();

        // The read-hook receives the path exactly as Claude Code's Read tool
        // saw it — through the symlinked spelling — while `vault_root` was
        // resolved (elsewhere) to the canonical, symlink-free spelling. This
        // mirrors the real-world `/tmp` vs `/private/tmp` split confirmed in
        // #255 R3.
        let input_through_symlink = link_vault.join("00-inbox/note.md");
        let canonical_root = real_vault.canonicalize().unwrap();

        assert_eq!(
            normalize_doc_path(input_through_symlink.to_str().unwrap(), &canonical_root),
            "00-inbox/note.md",
            "a symlinked-path input must still normalize to the relative index key, \
             not fall open to the raw absolute path"
        );
    }

    /// A relative `input` must never be canonicalized against `cwd` — only
    /// ABSOLUTE inputs are eligible for the symlink-resolution fallback.
    /// Canonicalizing a relative path implicitly resolves it against the
    /// process's current working directory, which has nothing to do with the
    /// vault and would silently produce a wrong key. A relative input that
    /// already fails the lexical strip (it's not absolute, so it trivially
    /// isn't a child of the absolute `vault_root`) must short-circuit straight
    /// to the existing passthrough behavior.
    #[test]
    fn normalize_doc_path_relative_input_is_idempotent_never_canonicalized() {
        let root = Path::new("/vault/ob-1");
        assert_eq!(
            normalize_doc_path("00-inbox/note.md", root),
            "00-inbox/note.md"
        );
        assert_eq!(
            normalize_doc_path("./00-inbox/note.md", root),
            "00-inbox/note.md"
        );
    }

    /// An absolute path that is genuinely outside the vault (no symlink
    /// involved, canonicalize succeeds on both sides, but the canonical strip
    /// still fails) must stay fail-open: return the input unchanged, never
    /// panic.
    #[cfg(unix)]
    #[test]
    fn normalize_doc_path_outside_vault_stays_fail_open() {
        let base = tempfile::tempdir().unwrap();
        let vault_root = base.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        let outside_dir = base.path().join("elsewhere");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let outside_file = outside_dir.join("x.md");
        std::fs::write(&outside_file, "body").unwrap();

        let result = normalize_doc_path(outside_file.to_str().unwrap(), &vault_root);
        assert_eq!(
            result,
            outside_file.to_str().unwrap(),
            "a genuinely-outside-vault absolute path must pass through unchanged, fail-open"
        );
    }

    /// A `vault_root` (or input) that does not exist on disk must not panic —
    /// `fs::canonicalize` errors out, and the function must fall back to the
    /// lexical (non-canonicalized) result.
    #[test]
    fn normalize_doc_path_nonexistent_paths_stay_fail_open_no_panic() {
        let root = Path::new("/vault/ob-1-does-not-exist-268");
        // Neither the input file nor vault_root exist on disk: canonicalize
        // fails on both, so this must fall back to the lexical passthrough
        // (unchanged) rather than panicking.
        assert_eq!(
            normalize_doc_path("/vault/ob-1-does-not-exist-268/00-inbox/note.md", root),
            "00-inbox/note.md",
            "lexical strip already succeeds here (no symlink involved), so the \
             canonicalize fallback path is never reached"
        );
        // Now force the lexical strip to fail too, by using a vault_root that
        // shares no prefix at all, and confirm no panic / safe passthrough.
        let other_root = Path::new("/some/other/nonexistent/root-268");
        let input = "/vault/ob-1-does-not-exist-268/00-inbox/note.md";
        assert_eq!(normalize_doc_path(input, other_root), input);
    }

    #[test]
    fn map_daemon_error_engine_busy_becomes_core_engine_busy() {
        // A daemon 503 arrives as `onebrain_search::EngineBusy` in the chain
        // (via `daemon_client::daemon_engine_busy`); `map_daemon_error` must turn
        // it into `CoreError::EngineBusy` so the verb exits 77 / E_ENGINE_BUSY —
        // exactly like the direct path's `map_engine_open_error`.
        let busy = anyhow::Error::new(onebrain_search::error::EngineBusy).context("via daemon");
        let mapped = map_daemon_error(busy, "route search through warm daemon");
        assert!(
            matches!(
                mapped.downcast_ref::<onebrain_core::CoreError>(),
                Some(onebrain_core::CoreError::EngineBusy(_))
            ),
            "an engine-busy daemon error must become CoreError::EngineBusy, got: {mapped:#}"
        );
    }

    #[test]
    fn map_daemon_error_other_keeps_verb_context() {
        // A non-busy daemon error keeps its opaque verb context (an E_INTERNAL),
        // and must NOT be classified as engine-busy.
        let other = anyhow::anyhow!("some genuine daemon failure");
        let mapped = map_daemon_error(other, "route `search get` through warm daemon");
        assert!(
            !onebrain_search::error::is_engine_busy(&mapped),
            "a non-busy error must not be classified as EngineBusy"
        );
        assert!(
            format!("{mapped:#}").contains("route `search get` through warm daemon"),
            "the verb context must be preserved, got: {mapped:#}"
        );
    }

    #[test]
    fn rerank_settings_from_config_default_matches_engine_default() {
        // Drift guard: an absent `search.reranker` block (config default)
        // must map to EXACTLY `RerankSettings::default()` — if the two
        // defaults ever diverge, this test catches it.
        let mapped = rerank_settings_from_config(&onebrain_core::RerankerConfig::default());
        assert_eq!(mapped, onebrain_search::engine::RerankSettings::default());
    }

    #[test]
    fn rerank_settings_from_config_maps_every_field() {
        let cfg = onebrain_core::RerankerConfig {
            enabled: false,
            model: "onebrain-rerank-v1".to_string(),
            min_candidates: 42,
            min_score: Some(0.55),
        };
        let mapped = rerank_settings_from_config(&cfg);
        assert!(!mapped.enabled);
        assert_eq!(mapped.model, "onebrain-rerank-v1");
        assert_eq!(mapped.min_candidates, 42);
        assert_eq!(mapped.min_score, 0.55);
    }

    #[test]
    fn rerank_settings_from_config_falls_back_to_engine_default_min_score() {
        let cfg = onebrain_core::RerankerConfig {
            min_score: None,
            ..Default::default()
        };
        let mapped = rerank_settings_from_config(&cfg);
        assert_eq!(
            mapped.min_score,
            onebrain_search::engine::DEFAULT_RERANK_MIN_SCORE
        );
    }

    #[test]
    fn format_size_renders_units() {
        assert_eq!(format_size(12), "12 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(471 * 1024 * 1024), "471 MB");
        assert_eq!(
            format_size(2 * 1024 * 1024 * 1024 + 200 * 1024 * 1024),
            "2.2 GB"
        );
    }
    use onebrain_core::{ResolvedVault, VaultRoot, VaultSource};
    use tempfile::tempdir;

    #[test]
    fn collection_name_readonly_does_not_persist_generated_name() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "folders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        let name = collection_name_readonly(dir.path()).unwrap();
        // Deterministic <dir>-<hash>, same as the persisting resolver would generate.
        assert_eq!(
            name,
            generate_collection_name(VaultRoot::from_path(dir.path()).unwrap().name(), dir.path())
        );
        // Config file must be UNCHANGED (no `collection:` written).
        let yaml = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(
            !yaml.contains("collection:"),
            "readonly resolver must not persist: {yaml}"
        );
    }

    #[test]
    fn collection_name_readonly_prefers_configured_value() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: explicit-name\n",
        )
        .unwrap();
        assert_eq!(
            collection_name_readonly(dir.path()).unwrap(),
            "explicit-name"
        );
    }

    /// Build a `ResolvedVault` rooted at `dir` (which must already contain an
    /// `onebrain.yml`), as the flag-resolved source.
    fn resolved_at(dir: &Path) -> ResolvedVault {
        ResolvedVault {
            root: VaultRoot::from_path(dir).unwrap(),
            source: VaultSource::Flag,
        }
    }

    #[test]
    fn collection_cache_dir_nests_under_search_root() {
        let dir = collection_cache_dir("my-vault");
        assert!(dir.ends_with("search/my-vault"));
    }

    #[test]
    fn generate_collection_name_is_dir_plus_short_hash() {
        let name = generate_collection_name("ob-1".to_string(), Path::new("/vaults/ob-1"));
        // `<dir>-<6 hex>`.
        assert!(name.starts_with("ob-1-"), "got {name}");
        let hash = name.strip_prefix("ob-1-").unwrap();
        assert_eq!(hash.len(), 6);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // Deterministic per path.
        assert_eq!(
            name,
            generate_collection_name("ob-1".to_string(), Path::new("/vaults/ob-1"))
        );
    }

    #[test]
    fn collection_for_autogenerates_and_persists_when_unset() {
        // Vault whose config has neither `search.collection` nor the legacy
        // `qmd_collection`.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "folders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        let resolved = resolved_at(dir.path());

        // First call generates `<dir>-<hash>` and persists it.
        let name = collection_for(&resolved).unwrap();
        let expected = generate_collection_name(resolved.root.name(), resolved.root.as_path());
        assert_eq!(name, expected);
        assert!(name.starts_with(&format!("{}-", resolved.root.name())));

        let yaml = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(
            yaml.contains(&format!("collection: {name}")),
            "persisted config should contain the generated collection:\n{yaml}"
        );
        // Other keys preserved.
        assert!(yaml.contains("inbox: 00-inbox"));

        // Second call reads the persisted value (stable, no re-derivation).
        let again = collection_for(&resolved).unwrap();
        assert_eq!(again, name);
    }

    #[test]
    fn collection_for_uses_configured_value_unchanged() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: my-explicit-name\n",
        )
        .unwrap();
        let resolved = resolved_at(dir.path());
        assert_eq!(collection_for(&resolved).unwrap(), "my-explicit-name");
    }

    #[test]
    fn collection_for_uses_legacy_qmd_collection_without_persisting() {
        // Legacy `qmd_collection` is honoured via `load_vault_config`'s
        // fallback, so no auto-generation happens.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "qmd_collection: legacy-name\n",
        )
        .unwrap();
        let resolved = resolved_at(dir.path());
        assert_eq!(collection_for(&resolved).unwrap(), "legacy-name");
    }

    #[test]
    fn open_engine_from_resolved_does_not_double_persist_collection() {
        // Copilot round-2 finding #3: `search_get.rs`'s direct (non-daemon)
        // path called `open_engine(vault_flag)` even though it already held
        // a `ResolvedVault` — re-resolving the vault AND re-deriving +
        // persisting an auto-generated `search.collection` a second time.
        // `open_engine_from_resolved` takes the already-resolved vault and
        // must call `collection_for` exactly once: a caller that already
        // resolved the collection (as `search_get::run` does) must not see
        // `onebrain.yml` written to again on open.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "folders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let resolved = resolved_at(dir.path());

        // Mirrors a caller that already resolved the collection before
        // opening the engine (as `search_get::run` does for the ledger
        // check).
        let expected_collection = collection_for(&resolved).unwrap();
        let before = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();

        let (_engine, out_resolved) = open_engine_from_resolved(&resolved).unwrap();
        assert_eq!(out_resolved.root.as_path(), resolved.root.as_path());

        // No further write happened: the file is byte-identical to right
        // after the first (and only) `collection_for` persist.
        let after = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert_eq!(
            after, before,
            "open_engine_from_resolved must not persist the collection again"
        );
        assert_eq!(
            after.matches("collection:").count(),
            1,
            "search.collection must be persisted exactly once, not duplicated: {after}"
        );
        assert!(after.contains(&format!("collection: {expected_collection}")));
    }

    #[test]
    fn is_indexed_false_when_dir_absent() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(!is_indexed(&missing));
    }

    #[test]
    fn index_size_bytes_sums_index_dirs_and_redb_excluding_models() {
        let dir = tempdir().unwrap();
        // No index yet → None.
        assert!(index_size_bytes(dir.path()).is_none());

        // Build the three artifacts under the split `index/` layout (paths
        // resolved via the layout so this test carries no literal artifact
        // path). tantivy 100 + vectors 200 + engine.redb 44 = 344.
        let tantivy = index_artifact_path(dir.path(), "tantivy");
        std::fs::create_dir_all(&tantivy).unwrap();
        std::fs::write(tantivy.join("meta.json"), vec![0u8; 100]).unwrap();
        let vectors = index_artifact_path(dir.path(), "vectors");
        std::fs::create_dir_all(&vectors).unwrap();
        std::fs::write(vectors.join("data.bin"), vec![0u8; 200]).unwrap();
        std::fs::write(
            index_artifact_path(dir.path(), "engine.redb"),
            vec![0u8; 44],
        )
        .unwrap();
        // A model dir must NOT be counted toward the index size.
        let model = models_cache_dir(dir.path()).join("models--intfloat--multilingual-e5-small");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("model.onnx"), vec![0u8; 9999]).unwrap();

        assert_eq!(index_size_bytes(dir.path()), Some(344));
    }

    #[test]
    fn is_indexed_true_when_dir_present() {
        let dir = tempdir().unwrap();
        assert!(is_indexed(dir.path()));
    }

    #[test]
    fn embed_model_key_present_true_when_configured() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  embed_model: bge-m3\n",
        )
        .unwrap();
        assert!(embed_model_key_present(dir.path()));
    }

    #[test]
    fn embed_model_key_present_false_when_absent() {
        let dir = tempdir().unwrap();
        // `search:` block with a collection but no embed_model.
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: t\n",
        )
        .unwrap();
        assert!(!embed_model_key_present(dir.path()));
    }

    #[test]
    fn embed_model_key_present_false_when_no_config_file() {
        let dir = tempdir().unwrap();
        assert!(!embed_model_key_present(dir.path()));
    }

    #[test]
    fn embed_model_key_present_false_on_unparseable_yaml() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "search: : : not yaml\n").unwrap();
        assert!(!embed_model_key_present(dir.path()));
    }

    #[test]
    fn any_model_downloaded_false_on_empty_cache() {
        let dir = tempdir().unwrap();
        assert!(!any_model_downloaded(dir.path()));
    }

    #[test]
    fn any_model_downloaded_true_when_one_present() {
        use onebrain_search::embed::model_registry;
        let dir = tempdir().unwrap();
        let m = &model_registry()[0];
        let mdir = dir.path().join(m.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 16]).unwrap();
        assert!(any_model_downloaded(dir.path()));
    }

    #[test]
    fn model_not_chosen_true_on_fresh_vault() {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "folders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        assert!(model_not_chosen(vault.path(), cache.path()));
    }

    #[test]
    fn model_not_chosen_false_when_key_configured() {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  embed_model: bge-m3\n",
        )
        .unwrap();
        assert!(!model_not_chosen(vault.path(), cache.path()));
    }

    #[test]
    fn model_not_chosen_false_when_model_downloaded() {
        use onebrain_search::embed::model_registry;
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        // No embed_model key, but a model already on disk → already committed.
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "folders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        let m = &model_registry()[0];
        let mdir = cache.path().join(m.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 16]).unwrap();
        assert!(!model_not_chosen(vault.path(), cache.path()));
    }

    #[test]
    fn reconcile_clears_a_stale_model_choice() {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap(); // empty → the model isn't downloaded
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: c\n  embed_model: bge-m3\n",
        )
        .unwrap();
        reconcile_missing_model(vault.path(), cache.path(), "bge-m3");
        assert!(
            !embed_model_key_present(vault.path()),
            "a configured-but-undownloaded model must be cleared"
        );
        let yaml = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("collection: c"), "{yaml}");
    }

    #[test]
    fn reconcile_is_noop_when_no_model_committed() {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: c\n",
        )
        .unwrap();
        reconcile_missing_model(vault.path(), cache.path(), "multilingual-e5-small");
        let yaml = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("collection: c"), "{yaml}");
    }

    // ── Warm-daemon routing (Track 2c) ─────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn daemon_routing_disabled_honours_env() {
        // Empty value → routing enabled; any non-empty value → disabled. Each
        // guard is scoped so only one holds the (non-reentrant) env lock at a
        // time.
        {
            let _enabled = crate::test_env::set_var("ONEBRAIN_NO_DAEMON", "");
            assert!(!daemon_routing_disabled(), "empty value → enabled");
        }
        {
            let _disabled = crate::test_env::set_var("ONEBRAIN_NO_DAEMON", "1");
            assert!(daemon_routing_disabled(), "non-empty value → disabled");
        }
    }

    #[cfg(unix)]
    #[test]
    fn route_to_daemon_none_when_disabled() {
        // With routing disabled, `route_to_daemon` short-circuits to None even
        // before any discovery — the direct-open / honest-EngineBusy path.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: rd-disabled\n",
        )
        .unwrap();
        let _set = crate::test_env::set_var("ONEBRAIN_NO_DAEMON", "1");
        let resolved = resolved_at(dir.path());
        assert!(route_to_daemon(&resolved).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn route_to_daemon_none_when_no_daemon_running() {
        // Routing enabled but no daemon.json under HOME → discover() is None →
        // route_to_daemon is None (fall back to direct open). HOME is redirected
        // to an empty tempdir so we never touch the real ~/.onebrain.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: rd-nodaemon\n",
        )
        .unwrap();
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("HOME", home.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("")),
        ]);
        let resolved = resolved_at(dir.path());
        assert!(route_to_daemon(&resolved).is_none());
    }

    /// PASSIVE, no-restart guard (Track 2c): a `daemon.json` bound to vault A,
    /// while the CLI resolves vault B, routes to DIRECT open (None) AND leaves
    /// the vault-A record untouched — the CLI must never stop/restart a daemon
    /// serving another vault (that would disrupt a live MCP session). Decision-
    /// level: no live server needed, since `discover_matching` rejects on the
    /// vault mismatch BEFORE any liveness probe or restart.
    #[cfg(unix)]
    #[test]
    fn route_to_daemon_wrong_vault_goes_direct_and_leaves_daemon_untouched() {
        use crate::commands::daemon_client::{canonical_vault_id, DaemonInfo};

        // Two distinct real vault dirs: A (the daemon's) and B (the CLI's).
        let vault_a = tempdir().unwrap();
        let vault_b = tempdir().unwrap();
        std::fs::write(
            vault_b.path().join("onebrain.yml"),
            "search:\n  collection: rd-vault-b\n",
        )
        .unwrap();
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("HOME", home.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("")),
        ]);

        // A daemon record in vault A's SLOT (port 1 = connection refused; never
        // probed here — the CLI resolves vault B's slot, which doesn't exist).
        let path = crate::commands::daemon_client::slot_json_path(Some(vault_a.path()))
            .unwrap()
            .unwrap();
        DaemonInfo {
            port: 1,
            token: "x".repeat(20),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            vault: canonical_vault_id(vault_a.path()),
        }
        .write(&path)
        .unwrap();

        // CLI resolves vault B → routes DIRECT (None), no restart.
        let resolved = resolved_at(vault_b.path());
        assert!(
            route_to_daemon(&resolved).is_none(),
            "wrong-vault daemon → direct open"
        );
        // The vault-A record is UNTOUCHED (passive path never removes/stops it).
        let still = DaemonInfo::read(&path).unwrap().expect("record preserved");
        assert_eq!(
            still.vault,
            canonical_vault_id(vault_a.path()),
            "CLI must not disturb a daemon serving another vault"
        );
    }

    // ── `open_lex_migrating` self-heal (issue #296) ─────────────────────────
    //
    // `open_lex_migrating` is the CLI-layer seam that lets the two read-only
    // lex fast paths (`search search` / the MCP lex sub-query) migrate a
    // stale-schema tantivy index without ever going through `Engine::index_doc`
    // themselves. It was previously proven only by a manual real-vault smoke
    // test; these pin its three branches with unit tests.

    use onebrain_search::chunk::Chunk;

    #[test]
    fn open_lex_migrating_repopulates_from_meta_on_schema_mismatch() {
        // Real-world scenario: a vault indexed by an older OneBrain build has a
        // tantivy dir under the pre-v3.4.16 schema. The lex fast path cannot
        // self-heal alone (no chunk text to refill from), so it must open the
        // engine once — which self-heals from redb's `chunk_meta` — and hand
        // back a WHOLE, searchable lex index, proven here by an actual search
        // hit rather than merely a successful `Ok(..)`.
        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: heal-296\n",
        )
        .unwrap();
        std::fs::write(
            vault.path().join("note.md"),
            "# Errors Handling\nzebra quokka narwhal error text\n",
        )
        .unwrap();

        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let resolved = resolved_at(vault.path());
        let collection = collection_for(&resolved).unwrap();
        let cache_dir = collection_cache_dir(&collection);

        // Populate chunk_meta + a CURRENT-schema lex index via a lex-only
        // reindex. This never touches the embedder (no network / model
        // download) — the same mechanism the CLI's `search reindex --lex-only`
        // uses, and exactly why it was chosen for this test's setup.
        {
            let (mut engine, _) = open_engine_from_resolved(&resolved).unwrap();
            engine
                .reindex_all_lex_only_with_progress(vault.path(), &mut |_| {})
                .unwrap();
        } // dropped here: releases the collection lock before the healing reopen.

        let tantivy_dir = index_artifact_path(&cache_dir, "tantivy");

        // Replace the current-schema tantivy dir with a FOREIGN-schema index —
        // simulating a vault upgraded from an older OneBrain build, exactly as
        // `onebrain_search::engine`'s
        // `reopen_after_lex_schema_change_repopulates_from_meta` test does.
        plant_foreign_schema(&tantivy_dir);

        let lex = open_lex_migrating(&cache_dir, &resolved)
            .expect("must self-heal, not hard-fail, on a schema mismatch");
        let hits = lex.search("error", 5).unwrap();
        assert!(
            !hits.is_empty(),
            "lex index must be repopulated from chunk_meta after the self-heal"
        );
        assert!(
            hits[0].0.starts_with("note.md#"),
            "expected the indexed chunk back, got {:?}",
            hits[0]
        );
    }

    #[test]
    fn open_lex_migrating_current_schema_leaves_engine_unopened() {
        // A tantivy dir already on the CURRENT schema must be handed back
        // as-is: existing contents preserved, and — critically — the engine
        // must never be opened (no wipe, no repopulate, no redb/lock file
        // created). The setup builds the tantivy index directly through
        // `LexIndex`'s own public API, with no `Engine`/chunk_meta involved at
        // all, so any engine-artifact appearing afterward can only have come
        // from `open_lex_migrating` itself.
        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: untouched-296\n",
        )
        .unwrap();
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let resolved = resolved_at(vault.path());
        let cache_dir = collection_cache_dir("untouched-296");
        let tantivy_dir = index_artifact_path(&cache_dir, "tantivy");

        {
            let mut ix = LexIndex::open(&tantivy_dir).unwrap();
            ix.add(&Chunk {
                chunk_id: "note.md#0".into(),
                doc_path: "note.md".into(),
                heading_path: String::new(),
                chunk_index: 0,
                text: "error handling in rust".into(),
            })
            .unwrap();
            ix.commit().unwrap();
        }

        let lex = open_lex_migrating(&cache_dir, &resolved).unwrap();
        let hits = lex.search("error", 5).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "existing content must be preserved untouched"
        );
        assert_eq!(hits[0].0, "note.md#0");

        assert!(
            !cache_dir.join(".collection.lock").exists(),
            "a current-schema index must never trigger an engine open"
        );
        assert!(
            !index_artifact_path(&cache_dir, "engine.redb").exists(),
            "no redb metadata db should be created for a healthy index"
        );
        assert!(
            !index_artifact_path(&cache_dir, "vectors").exists(),
            "no vector store should be created for a healthy index"
        );
    }

    #[test]
    fn open_lex_migrating_heals_an_index_left_pending_by_an_interrupted_rebuild() {
        // B-R1 (BLOCKER, v3.4.16 re-audit): SIGKILL a migrating `search
        // search` and what is left on disk is an EMPTY tantivy dir with a
        // CURRENT, matching schema — `open_or_reset` recreates it immediately
        // after the wipe. So `LexIndex::open` SUCCEEDS, the schema-mismatch
        // branch is never taken, and the fast path handed back an empty index:
        // `ok: true`, 0 hits, exit 0, no stderr, marker still on disk — on
        // every subsequent call, until some unrelated engine-opening command
        // happened to run. Only the rebuild marker distinguishes this state
        // from a healthy index, so the fast path must consult it itself.
        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: pending-br1\n",
        )
        .unwrap();
        std::fs::write(
            vault.path().join("note.md"),
            "# Errors Handling\nzebra quokka narwhal error text\n",
        )
        .unwrap();

        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let resolved = resolved_at(vault.path());
        let collection = collection_for(&resolved).unwrap();
        let cache_dir = collection_cache_dir(&collection);

        // Populate chunk_meta + a current-schema lex index (lex-only: no
        // embedder, no model download).
        {
            let (mut engine, _) = open_engine_from_resolved(&resolved).unwrap();
            engine
                .reindex_all_lex_only_with_progress(vault.path(), &mut |_| {})
                .unwrap();
        }

        // Reproduce the crash: marker written, dir wiped, dir recreated empty
        // under the CURRENT schema — then the process dies before the commit.
        let tantivy_dir = index_artifact_path(&cache_dir, "tantivy");
        let marker = onebrain_search::lex::rebuild_marker_path(&tantivy_dir);
        std::fs::write(&marker, b"rebuild pending\n").unwrap();
        std::fs::remove_dir_all(&tantivy_dir).unwrap();
        drop(LexIndex::open(&tantivy_dir).unwrap());

        // Confirm the trap is real: a plain open reports no error and no docs.
        assert_eq!(
            LexIndex::open(&tantivy_dir).unwrap().num_docs().unwrap(),
            0,
            "the post-crash index must open cleanly AND be empty"
        );
        assert!(onebrain_search::lex::rebuild_pending(&tantivy_dir));

        let lex = open_lex_migrating(&cache_dir, &resolved)
            .expect("a pending rebuild must be healed, not handed back empty");
        let hits = lex.search("error", 5).unwrap();
        assert!(
            !hits.is_empty(),
            "the fast path must rebuild from chunk_meta when a rebuild is pending"
        );
        assert!(hits[0].0.starts_with("note.md#"), "{:?}", hits[0]);
        assert!(
            !onebrain_search::lex::rebuild_pending(&tantivy_dir),
            "a completed heal must clear the marker"
        );
    }

    #[test]
    fn open_lex_migrating_markerless_empty_index_leaves_engine_unopened() {
        // The guard must key on the MARKER, not on emptiness: a fresh vault
        // (or one where everything is excluded) has a legitimately empty
        // current-schema index and must still open with no engine, no lock,
        // no redb — otherwise every such search pays a full engine open.
        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: emptyok-br1\n",
        )
        .unwrap();
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let resolved = resolved_at(vault.path());
        let cache_dir = collection_cache_dir("emptyok-br1");
        let tantivy_dir = index_artifact_path(&cache_dir, "tantivy");
        drop(LexIndex::open(&tantivy_dir).unwrap());
        assert!(!onebrain_search::lex::rebuild_pending(&tantivy_dir));

        let lex = open_lex_migrating(&cache_dir, &resolved).unwrap();
        assert_eq!(lex.num_docs().unwrap(), 0);
        assert!(
            !cache_dir.join(".collection.lock").exists(),
            "a markerless empty index must never trigger an engine open"
        );
        assert!(!index_artifact_path(&cache_dir, "engine.redb").exists());
        assert!(!index_artifact_path(&cache_dir, "vectors").exists());
    }

    #[test]
    fn open_lex_migrating_propagates_non_schema_errors_without_wiping() {
        // A non-schema failure (I/O, permissions, corruption) must propagate
        // untouched — a wipe/self-heal is only the right response to a
        // genuine schema mismatch. Force a plain I/O error portably (no
        // chmod/root dependency, unlike this repo's permission-based tests
        // elsewhere): put a REGULAR FILE where the tantivy directory belongs,
        // so `LexIndex::open`'s `fs::create_dir_all` fails outright before it
        // ever gets to opening an index — this can never be misclassified as
        // `TantivyError::SchemaError`.
        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: badpath-296\n",
        )
        .unwrap();
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let resolved = resolved_at(vault.path());
        let cache_dir = collection_cache_dir("badpath-296");
        let tantivy_dir = index_artifact_path(&cache_dir, "tantivy");
        std::fs::create_dir_all(tantivy_dir.parent().unwrap()).unwrap();
        std::fs::write(&tantivy_dir, b"not a directory").unwrap();

        // `LexIndex` isn't `Debug` (mirrors `Engine`), so unwrap the error via
        // `match` rather than `expect_err`.
        let err = match open_lex_migrating(&cache_dir, &resolved) {
            Ok(_) => panic!("a file where a directory belongs must error, not silently wipe"),
            Err(e) => e,
        };
        // Not misclassified as the self-heal trigger.
        assert!(
            !onebrain_search::lex::is_schema_mismatch(&err),
            "a plain I/O error must not be classified as a schema mismatch: {err:#}"
        );

        // Nothing was wiped: the file is still exactly what was written.
        assert!(
            tantivy_dir.is_file(),
            "must not have been replaced by a directory"
        );
        assert_eq!(std::fs::read(&tantivy_dir).unwrap(), b"not a directory");

        // The engine must never have been opened either (no wipe path taken
        // means no self-heal attempt).
        assert!(!cache_dir.join(".collection.lock").exists());
    }

    // ── busy-retry inside the self-heal (C3, v3.4.16) ──────────────────────
    //
    // The healing `Engine::open` takes redb's single-process collection lock.
    // Six concurrent `search search` calls on a freshly-upgraded index gave 1
    // success and 5 × `E_ENGINE_BUSY` (exit 77) — a transient, self-resolving
    // condition surfaced as a hard failure, and reachable in normal use (the
    // agent fires MCP lex sub-queries alongside the PostToolUse reindex hook).
    //
    // Driving `open_lex_migrating_inner`'s injected heal closure directly makes
    // the retry policy deterministic; the two public wrappers differ only in
    // WHICH engine open they pass in (proven by their own tests above and by
    // `server::search`'s byte-identical-config test).

    /// (Re)create `tantivy_dir` holding an index whose schema shares no field
    /// with ours, so the next open sees the typed `SchemaError` a pre-v3.4.16
    /// index produces. The stand-in for "a vault upgraded from an older
    /// OneBrain build" — same shape as `onebrain_search::engine`'s own test
    /// helper (the two crates are separate test targets, so the duplication
    /// across them is unavoidable).
    fn plant_foreign_schema(tantivy_dir: &Path) {
        let _ = std::fs::remove_dir_all(tantivy_dir);
        std::fs::create_dir_all(tantivy_dir).unwrap();
        let mut sb = tantivy::schema::Schema::builder();
        sb.add_text_field("something_else", tantivy::schema::TEXT);
        tantivy::Index::builder()
            .schema(sb.build())
            .open_or_create(tantivy::directory::MmapDirectory::open(tantivy_dir).unwrap())
            .unwrap();
    }

    /// A stale-schema tantivy dir at `<cache>/tantivy`, plus the cache dir —
    /// enough to drive `open_lex_migrating_inner` down its self-heal branch.
    fn stale_schema_cache_dir() -> (tempfile::TempDir, PathBuf) {
        let cache = tempdir().unwrap();
        let tantivy_dir = index_artifact_path(cache.path(), "tantivy");
        plant_foreign_schema(&tantivy_dir);
        let dir = cache.path().to_path_buf();
        (cache, dir)
    }

    /// Stand in for what the race winner does: replace the foreign-schema
    /// index with a current-schema one holding a single searchable chunk.
    fn write_current_schema_index(cache_dir: &Path) {
        let tantivy_dir = index_artifact_path(cache_dir, "tantivy");
        std::fs::remove_dir_all(&tantivy_dir).unwrap();
        let mut ix = LexIndex::open(&tantivy_dir).unwrap();
        ix.add(&Chunk {
            chunk_id: "note.md#0".into(),
            doc_path: "note.md".into(),
            heading_path: String::new(),
            chunk_index: 0,
            text: "error handling in rust".into(),
        })
        .unwrap();
        ix.commit().unwrap();
    }

    #[test]
    fn open_lex_migrating_retries_a_busy_engine_instead_of_failing() {
        // Attempt 1 loses the collection lock (typed `EngineBusy` — what
        // `Engine::open` itself returns); attempt 2 loses it again, this time
        // as the `CoreError::EngineBusy` that `map_engine_open_error` re-wraps
        // it into (both classifications must be retried); attempt 3 wins and
        // migrates. Without the retry the very first busy would have surfaced
        // as exit 77.
        let (_cache, cache_dir) = stale_schema_cache_dir();
        let mut calls = 0usize;
        let lex = open_lex_migrating_inner(&cache_dir, &mut || {
            calls += 1;
            match calls {
                1 => Err(anyhow::Error::new(onebrain_search::error::EngineBusy)),
                2 => Err(anyhow::Error::new(onebrain_core::CoreError::EngineBusy(
                    "index locked by another process".into(),
                ))),
                _ => {
                    write_current_schema_index(&cache_dir);
                    Ok(())
                }
            }
        })
        .expect("a transiently busy engine must be retried, not surfaced as E_ENGINE_BUSY");
        assert_eq!(calls, 3, "both busy classifications must be retried");
        assert_eq!(lex.search("error", 5).unwrap().len(), 1);
    }

    #[test]
    fn open_lex_migrating_takes_the_cheap_win_when_the_race_winner_migrated() {
        // The common real shape: another process won the lock and is doing the
        // very same migration. Once it commits, the plain `LexIndex::open`
        // succeeds — so the retry must re-check that FIRST and never need the
        // engine (or its lock) at all.
        let (_cache, cache_dir) = stale_schema_cache_dir();
        let mut calls = 0usize;
        let lex = open_lex_migrating_inner(&cache_dir, &mut || {
            calls += 1;
            // The winner commits its migration while we back off.
            write_current_schema_index(&cache_dir);
            Err(anyhow::Error::new(onebrain_search::error::EngineBusy))
        })
        .expect("a migration completed by the race winner must be picked up");
        assert_eq!(calls, 1, "the cheap re-open must win before a second try");
        assert_eq!(lex.search("error", 5).unwrap().len(), 1);
    }

    #[test]
    fn open_lex_migrating_rejects_a_cheap_win_the_race_winner_never_finished() {
        // The cheap-win probe's dark twin (B-R1): the process that won the
        // lock wiped the index and then DIED. Its leftovers open perfectly —
        // current schema, zero docs — so an unguarded `if let Ok(lex) =
        // LexIndex::open(..)` would accept an empty index and report success.
        // The marker is the only thing that says otherwise, so the probe must
        // require it to be gone.
        let (_cache, cache_dir) = stale_schema_cache_dir();
        let tantivy_dir = index_artifact_path(&cache_dir, "tantivy");
        let marker = onebrain_search::lex::rebuild_marker_path(&tantivy_dir);
        let mut calls = 0usize;
        let lex = open_lex_migrating_inner(&cache_dir, &mut || {
            calls += 1;
            match calls {
                1 => {
                    // The winner wiped to a current-schema EMPTY index, wrote
                    // its marker, and then crashed before repopulating.
                    std::fs::write(&marker, b"rebuild pending\n").unwrap();
                    std::fs::remove_dir_all(&tantivy_dir).unwrap();
                    drop(LexIndex::open(&tantivy_dir).unwrap());
                    Err(anyhow::Error::new(onebrain_search::error::EngineBusy))
                }
                _ => {
                    // We get the lock next and complete the rebuild properly.
                    write_current_schema_index(&cache_dir);
                    std::fs::remove_file(&marker).unwrap();
                    Ok(())
                }
            }
        })
        .expect("an abandoned rebuild must be finished, not accepted as a win");
        assert_eq!(
            calls, 2,
            "the cheap-win probe must refuse an index whose rebuild is still pending"
        );
        assert_eq!(
            lex.search("error", 5).unwrap().len(),
            1,
            "the returned index must be the REBUILT one, not the abandoned empty one"
        );
    }

    #[test]
    fn open_lex_migrating_serves_a_populated_index_whose_rebuild_marker_is_permanent() {
        // B-R1's fix, over-applied: gating the heal on the MARKER ALONE made
        // every read-only lex surface fail closed on states where the marker is
        // permanent BY DESIGN and the lex index is the last readable copy — D2
        // (`repopulate` refuses and deliberately keeps the marker) and a
        // post-commit marker-clear failure. `Engine::open` reports `EngineBusy`
        // for a live handle IN THE SAME PROCESS, and `server::search::run_lex`
        // runs inside the daemon that already holds one, so "heal" there was a
        // guaranteed 3-attempt, ~200 ms march to HTTP 503 with nothing
        // contending. A readable index must be served.
        let cache = tempdir().unwrap();
        let cache_dir = cache.path().to_path_buf();
        let tantivy_dir = index_artifact_path(&cache_dir, "tantivy");
        std::fs::create_dir_all(&tantivy_dir).unwrap();
        write_current_schema_index(&cache_dir);
        let marker = onebrain_search::lex::rebuild_marker_path(&tantivy_dir);
        std::fs::write(&marker, b"rebuild pending\n").unwrap();
        assert!(onebrain_search::lex::rebuild_pending(&tantivy_dir));

        let mut calls = 0usize;
        let lex = open_lex_migrating_inner(&cache_dir, &mut || {
            calls += 1;
            Ok(())
        })
        .expect("a populated index must be served, not refused");
        assert_eq!(
            calls, 0,
            "a marker over a NON-EMPTY index must never trigger the engine open"
        );
        assert_eq!(
            lex.search("error", 5).unwrap().len(),
            1,
            "the readable index must actually answer"
        );
        assert!(
            marker.exists(),
            "this path must not resolve the marker — doctor still has to report it"
        );
    }

    #[test]
    fn open_lex_migrating_takes_a_cheap_win_whose_marker_is_permanent() {
        // Same reasoning one layer down: if the race winner lands in the
        // permanently-pending state while we back off, rejecting its populated
        // index only marches us into the busy error the retry exists to avoid.
        // Only an EMPTY index + marker (the abandoned rebuild) is refused —
        // that case is `open_lex_migrating_rejects_a_cheap_win_the_race_winner_never_finished`.
        let (_cache, cache_dir) = stale_schema_cache_dir();
        let tantivy_dir = index_artifact_path(&cache_dir, "tantivy");
        let marker = onebrain_search::lex::rebuild_marker_path(&tantivy_dir);
        let mut calls = 0usize;
        let lex = open_lex_migrating_inner(&cache_dir, &mut || {
            calls += 1;
            // The winner migrated and committed, but its marker survives.
            write_current_schema_index(&cache_dir);
            std::fs::write(&marker, b"rebuild pending\n").unwrap();
            Err(anyhow::Error::new(onebrain_search::error::EngineBusy))
        })
        .expect("a populated index committed by the race winner must be picked up");
        assert_eq!(calls, 1, "the cheap re-open must win before a second try");
        assert_eq!(lex.search("error", 5).unwrap().len(), 1);
    }

    #[test]
    fn open_lex_migrating_gives_up_on_a_persistently_busy_engine() {
        // Bounded: a genuinely stuck lock is surfaced honestly as busy after a
        // small, fixed number of tries — never an unbounded loop.
        let (_cache, cache_dir) = stale_schema_cache_dir();
        let mut calls = 0usize;
        let started = std::time::Instant::now();
        let err = match open_lex_migrating_inner(&cache_dir, &mut || {
            calls += 1;
            Err(anyhow::Error::new(onebrain_search::error::EngineBusy))
        }) {
            Ok(_) => panic!("a permanently busy engine must not report success"),
            Err(e) => e,
        };
        let elapsed = started.elapsed();
        assert_eq!(
            calls, LEX_MIGRATE_OPEN_ATTEMPTS,
            "the retry must be bounded"
        );
        // `LEX_MIGRATE_OPEN_ATTEMPTS` is DERIVED from the same table the loop
        // consumes, so the assertion above holds for any table — including one
        // that sleeps for ten seconds. Pin the actual budget too: the cost of a
        // busy give-up is what the user waits through, and it is the reason the
        // table is "bounded and small on purpose".
        assert_eq!(calls, 3, "two backoffs plus the first attempt");
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "the whole give-up must stay well under a second of sleeping, got {elapsed:?}"
        );
        assert!(
            is_engine_busy_error(&err),
            "the honest busy classification must survive the retries: {err:#}"
        );
    }

    #[test]
    fn open_lex_migrating_never_retries_a_genuine_failure() {
        // Corrupt redb, IO error, permission denied — not transient, so
        // retrying only wastes the user's time and hides the cause.
        let (_cache, cache_dir) = stale_schema_cache_dir();
        let mut calls = 0usize;
        let err = match open_lex_migrating_inner(&cache_dir, &mut || {
            calls += 1;
            Err(anyhow::anyhow!("engine.redb is corrupt"))
        }) {
            Ok(_) => panic!("a genuine failure must not be swallowed"),
            Err(e) => e,
        };
        assert_eq!(calls, 1, "a non-busy failure must be surfaced immediately");
        assert!(
            format!("{err:#}").contains("engine.redb is corrupt"),
            "{err:#}"
        );
    }
}

/// Path of the live reindex-progress marker inside a collection cache dir.
/// Written by an in-flight `search reindex`, read by `search status` from
/// any other process, removed when the reindex finishes.
pub(crate) fn reindex_progress_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("reindex-progress.json")
}

/// A live reindex's `(done, total)` doc counts.
#[derive(
    Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub(crate) struct ReindexLiveProgress {
    pub done: usize,
    pub total: usize,
}

/// Read the live progress marker, if a reindex appears to be running.
/// Fresh = touched within the last 30 minutes — a crashed reindex leaves the
/// file behind, so stale markers are ignored (and best-effort removed).
pub(crate) fn read_reindex_progress(cache_dir: &Path) -> Option<ReindexLiveProgress> {
    let path = reindex_progress_path(cache_dir);
    let meta = std::fs::metadata(&path).ok()?;
    let fresh = meta
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|e| e.as_secs() < 30 * 60);
    if !fresh {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()
}

#[cfg(test)]
mod live_progress_tests {
    use super::*;

    #[test]
    fn read_reindex_progress_roundtrip_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_reindex_progress(dir.path()).is_none(), "no marker yet");

        let marker = reindex_progress_path(dir.path());
        std::fs::write(&marker, r#"{"done":457,"total":761}"#).unwrap();
        let p = read_reindex_progress(dir.path()).unwrap();
        assert_eq!(
            p,
            ReindexLiveProgress {
                done: 457,
                total: 761
            }
        );

        std::fs::write(&marker, "not json").unwrap();
        assert!(
            read_reindex_progress(dir.path()).is_none(),
            "garbage → None"
        );
    }
}
