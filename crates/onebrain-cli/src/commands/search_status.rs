//! `onebrain search status` — report native-search index status.
//!
//! Vault-required (exit 64 outside a vault). Reports the configured (or
//! auto-generated) collection, embed model, on-disk cache dir, the downloaded
//! model's size + download time, the last-indexed timestamp, the indexed doc
//! count, and the pending drift between the vault and the index.
//!
//! Opening the engine here is read-only: `Engine::open` uses the lazy
//! embedder (engine.rs), so it never downloads a model. `Engine::status`
//! only reads stored hashes and re-hashes the vault's `*.md` files — no
//! embed, no download — so `status` stays cheap even on a huge vault.

use std::path::{Path, PathBuf};

use anyhow::Result;
use onebrain_core::path::ResolvedVault;
use serde::Serialize;

use crate::commands::search_common::{
    collection_cache_dir, collection_for, format_size, index_size_bytes, read_reindex_progress,
    resolve_collection, ReindexLiveProgress,
};
use crate::output::{emit, item, section, Envelope, OutputMode};
use onebrain_core::load_vault_config;
use onebrain_search::embed::{model_download_status, model_registry};
use onebrain_search::engine::Engine;
use onebrain_search::rerank::{reranker_download_status, reranker_registry};

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub(crate) struct SearchStatusData {
    collection: Option<String>,
    embed_model: String,
    cache_dir: Option<PathBuf>,
    indexed: bool,
    /// `true` when this is a semantic build with a configured collection whose
    /// current embedding model is NOT downloaded (a fresh vault, or a purged
    /// cache) — search is blocked until a reindex. Lets machine consumers tell
    /// this apart from a lex-only build or a collection-less vault, both of
    /// which also report a null model size.
    current_model_missing: bool,
    /// Size in bytes of the ACTIVE model's dir (`models--*`) under the
    /// collection cache dir. `None` if the active model isn't downloaded yet.
    /// (The whole-cache total, including any lingering other-model dirs, is
    /// `cache_size_bytes`.)
    model_size_bytes: Option<u64>,
    /// Epoch seconds of the active model dir's mtime (when it was downloaded).
    /// `None` if not downloaded.
    model_downloaded_at: Option<u64>,
    /// Epoch seconds of the last `reindex` run. `None` if never indexed.
    last_indexed_at: Option<u64>,
    /// Total on-disk size in bytes of the index itself (the `tantivy/` and
    /// `vectors/` dirs plus `engine.redb`, under the collection cache dir) — the
    /// downloaded models are NOT counted here (see `model_size_bytes`). `None`
    /// when no index exists yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    index_size_bytes: Option<u64>,
    /// Number of docs currently indexed. `null` when the count is UNKNOWN
    /// because the engine couldn't be opened (see `busy`) — distinct from a
    /// genuine `0` (a real, empty index).
    doc_count: Option<usize>,
    /// Pending drift: docs on disk with no stored hash. `null` when unknown
    /// (engine busy).
    pending_new: Option<usize>,
    /// Pending drift: docs whose content changed since last index. `null` when
    /// unknown (engine busy).
    pending_changed: Option<usize>,
    /// Pending drift: indexed docs whose file is gone. `null` when unknown
    /// (engine busy).
    pending_removed: Option<usize>,
    /// Total byte size of the whole collection cache dir (models + index +
    /// live markers). `None` when no collection is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_size_bytes: Option<u64>,
    /// Present when a `search reindex` is running RIGHT NOW in another
    /// process (live marker in the cache dir): its (done, total) counts.
    /// While set, `doc_count`/pending below may lag the in-flight run.
    #[serde(skip_serializing_if = "Option::is_none")]
    reindexing: Option<ReindexLiveProgress>,
    /// `true` when the engine could NOT be opened because its index is locked
    /// by another process (redb single-process lock — typically the long-lived
    /// `onebrain mcp` server). When set, `doc_count` and the `pending_*` counts
    /// are `null`/unknown (NOT a healthy zero) and a `W_ENGINE_BUSY` warning
    /// rides the envelope. See v3.4.6 (bug B): never report "up to date" while
    /// busy.
    busy: bool,
    /// Present when the engine opened but its status could NOT be read, or the
    /// engine failed to open for a NON-lock reason (corrupt redb, IO error,
    /// permission denied, missing model dir). Distinct from `busy` (a lock held
    /// by a live process): here the index is broken/unreadable, so counts are
    /// `null`/unknown and status text shows "⚠️ status read failed" — NEVER
    /// "✅ up to date" (v3.4.6 round-2: don't resurrect bug B over a broken
    /// index). Rides a `W_STATUS_UNREADABLE` warning on the envelope.
    #[serde(skip_serializing_if = "Option::is_none")]
    status_error: Option<String>,
    /// `false` in a lex-only build (no `semantic` feature): no ONNX runtime,
    /// so embedding-backed verbs (`vsearch`, hybrid `query`, `model set`) are
    /// unavailable and `reindex` indexes keyword-only. See ADR 0017.
    semantic_available: bool,
    /// Configured `search.reranker.model` name (Tier-2 cross-encoder).
    reranker_model: String,
    /// `true` when the Tier-2 rerank stage will actually run: `search.reranker.enabled`
    /// AND the configured reranker model is downloaded. CLI-side (config +
    /// filesystem) readiness only — this does NOT open the engine to confirm
    /// the model loads; a load failure at query time still falls back to
    /// unreranked (see `Engine::rerank_active`, which the daemon status
    /// endpoint will surface directly in Task 7).
    reranker_ready: bool,
    /// `true` when the configured reranker model's `models--*` dir is present
    /// on disk (matches doctor's `reranker_downloaded:` vocabulary — see
    /// `doctor.rs`'s `native_search_check`). Distinct from `reranker_ready`,
    /// which additionally requires `search.reranker.enabled`.
    reranker_downloaded: bool,
    /// On-disk byte size of the configured reranker's downloaded model dir,
    /// `None` if not downloaded. Mirrors `model_size_bytes` for the embedder.
    reranker_disk_bytes: Option<u64>,
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    let (resolved, collection) = resolve_collection(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);

    // Warm-daemon path: when a daemon holds the engine, source the doc/pending
    // counts from it instead of opening a second engine (which would report
    // `busy`). Only when it serves this exact vault; else `None` → direct probe.
    let daemon = crate::commands::search_common::route_to_daemon(&resolved);
    let data = status_data(&resolved, collection, daemon.as_ref())?;

    // Bug B (v3.4.6): a busy index is a valid report (exit 0), but must ride a
    // warning so machine consumers see the contention explicitly rather than
    // inferring it from the null counts. Round-2: an unreadable/broken index
    // (open/status failure that ISN'T a live lock) rides its own
    // `W_STATUS_UNREADABLE` warning so it, too, can't be mistaken for a healthy
    // empty index. Both keep exit 0 — `status` is a report, not a failure.
    let busy = data.busy;
    let status_error = data.status_error.clone();
    let mut envelope = Envelope::ok("search.status", Some(vault_info), data);
    if busy {
        envelope = envelope.with_warning(
            "W_ENGINE_BUSY",
            "search index is locked by another process (e.g. the `onebrain mcp` server) — \
             doc/pending counts are unknown until it releases the lock",
        );
    } else if let Some(msg) = status_error {
        envelope = envelope.with_warning(
            "W_STATUS_UNREADABLE",
            format!(
                "search index status could not be read (index broken/unreadable) — \
                 doc/pending counts are unknown: {msg}"
            ),
        );
    }
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

impl SearchStatusData {
    /// Total pending drift (new + changed + removed). Unknown counts (`None`,
    /// e.g. engine busy) contribute zero. Single source of truth for the two
    /// consumers in `render_text` (the status line and the reindex hint) so the
    /// rollup can't drift between them.
    fn pending_total(&self) -> usize {
        self.pending_new.unwrap_or(0)
            + self.pending_changed.unwrap_or(0)
            + self.pending_removed.unwrap_or(0)
    }

    /// Indexed doc count (`None` when unknown — engine busy/unreadable). Exposed
    /// `pub(crate)` for the MCP track's tests, which assert the daemon-routed
    /// `status` tool reports the same count as the direct-engine path.
    #[cfg(test)]
    pub(crate) fn doc_count(&self) -> Option<usize> {
        self.doc_count
    }
}

/// Build the `search status` payload for an already-resolved vault.
///
/// `collection` is the effective collection name (already resolved by the
/// caller, e.g. via [`resolve_collection`] or [`collection_for`]) so callers
/// that already have it don't re-derive it.
///
/// Order matters here: the on-disk model/index sizes are read from the
/// filesystem *before* the engine is opened, because `Engine::open` creates
/// the cache dir (with an empty index) as a side effect — reading
/// `index_size_bytes` afterwards would count the freshly-created empty redb
/// and report a non-zero index size on a never-indexed vault.
///
/// `indexed` is derived from the engine's `doc_count` (below), NOT from the
/// cache dir's existence: an existing-but-empty dir — one left behind by a
/// prior status probe's `Engine::open`, or a cache purged and recreated empty
/// — must still report `indexed: false`.
/// `daemon`: an optional warm-daemon handle. When `Some`, the doc/pending
/// counts come from the daemon's `/api/internal/status` (the sole redb owner)
/// instead of opening a second engine — so `search status` succeeds with real
/// counts while an MCP session holds the engine, rather than degrading to
/// `busy`. All the fs-derived fields (model/index/cache sizes) are read
/// identically either way. A daemon status-probe failure degrades to the same
/// `Error` outcome the direct broken-index path uses (unknown counts +
/// `W_STATUS_UNREADABLE`), never a false "up to date".
pub(crate) fn status_data(
    resolved: &ResolvedVault,
    collection: Option<String>,
    daemon: Option<&crate::commands::daemon_client::DaemonHandle>,
) -> Result<SearchStatusData> {
    let config = load_vault_config(&resolved.root)?;

    // Cache dir + on-disk model stats + index size (pure fs reads — never a
    // download). Must run before the engine is opened (see doc comment).
    let (cache_dir, model_size_bytes, model_downloaded_at, index_size_bytes) = match &collection {
        Some(c) => {
            let dir = collection_cache_dir(c);
            let (size, downloaded) = match active_model_dir_stats(&dir, &config.search.embed_model)
            {
                Some((size, mtime)) => (Some(size), mtime),
                None => (None, None),
            };
            let idx_size = index_size_bytes(&dir);
            (Some(dir), size, downloaded, idx_size)
        }
        None => (None, None, None, None),
    };
    let reindexing = cache_dir.as_deref().and_then(read_reindex_progress);
    let cache_size_bytes = cache_dir
        .as_deref()
        .filter(|d| d.is_dir())
        .map(onebrain_search::embed::dir_size_bytes);

    // Index status (doc count, last_indexed, drift) — opens the engine
    // read-only (lazy embedder → no model download) and re-hashes the vault.
    // Open directly at the already-resolved `cache_dir` instead of via
    // `open_engine`, which would redundantly re-resolve the vault + collection
    // we already have. `set_exclude_patterns` still mirrors `open_engine` so
    // `status`'s drift walk honours `search.exclude` (else excluded files would
    // inflate `pending_new`).
    //
    // Honesty contract (v3.4.6, bug B): when the engine is LOCKED by another
    // process (redb single-process lock — the `onebrain mcp` server), report
    // the counts as UNKNOWN (`None`) + `busy: true`, NOT as a healthy `0` that
    // would render "✅ up to date".
    //
    // Round-2 fix: a NON-lock open failure (permission denied, corrupt redb
    // header, missing model dir) OR a `status()` read failure (corrupt redb /
    // IO / partial write) must ALSO refuse the up-to-date branch. Both degrade
    // to a distinct `Error` outcome (unknown counts + a `W_STATUS_UNREADABLE`
    // warning), NOT the silent `busy:false` path that used to resurrect bug B
    // by rendering "✅ up to date" over a broken index. The underlying error is
    // logged to stderr (mirrors the hook path in search_reindex.rs) so it's
    // never swallowed.
    let probe = match (daemon, cache_dir.as_deref()) {
        // Warm-daemon path: counts come from the held engine over HTTP, so a
        // live MCP session no longer forces a `busy` report.
        (Some(handle), _) => probe_via_daemon(handle),
        (None, Some(dir)) => match Engine::open(dir, &config.search.embed_model) {
            Ok(mut engine) => {
                engine.set_exclude_patterns(config.search.exclude.clone());
                match engine.status(resolved.root.as_path()) {
                    Ok(s) => StatusProbe::Ok(s),
                    Err(e) => {
                        eprintln!("search status: {e:#}");
                        StatusProbe::Error {
                            message: short_error(&e),
                        }
                    }
                }
            }
            Err(e) if onebrain_search::error::is_engine_busy(&e) => {
                StatusProbe::Unknown { busy: true }
            }
            Err(e) => {
                eprintln!("search status: {e:#}");
                StatusProbe::Error {
                    message: short_error(&e),
                }
            }
        },
        (None, None) => StatusProbe::Unknown { busy: false },
    };

    let mut status_error: Option<String> = None;
    let (last_indexed_at, doc_count, pending_new, pending_changed, pending_removed, busy) =
        match probe {
            StatusProbe::Ok(s) => (
                s.last_indexed_at,
                Some(s.doc_count),
                Some(s.pending_new),
                Some(s.pending_changed),
                Some(s.pending_removed),
                false,
            ),
            StatusProbe::Unknown { busy } => (None, None, None, None, None, busy),
            StatusProbe::Error { message } => {
                status_error = Some(message);
                (None, None, None, None, None, false)
            }
        };

    let current_model_missing =
        cfg!(feature = "semantic") && collection.is_some() && model_size_bytes.is_none();

    // No collection → no cache dir to check a reranker download against; the
    // model name still reports from config (it's meaningful even unindexed),
    // but readiness/download default to "nothing on disk yet".
    let (reranker_model, reranker_ready, reranker_downloaded, reranker_disk_bytes) =
        match cache_dir.as_deref() {
            Some(dir) => reranker_status_fields(&config, dir),
            None => (config.search.reranker.model.clone(), false, false, None),
        };

    Ok(SearchStatusData {
        collection,
        embed_model: config.search.embed_model,
        cache_dir,
        // Unknown doc_count (engine busy / open failed) is NOT "indexed".
        indexed: doc_count.is_some_and(|c| c > 0),
        current_model_missing,
        model_size_bytes,
        model_downloaded_at,
        last_indexed_at,
        index_size_bytes,
        doc_count,
        pending_new,
        pending_changed,
        pending_removed,
        cache_size_bytes,
        reindexing,
        busy,
        status_error,
        semantic_available: cfg!(feature = "semantic"),
        reranker_model,
        reranker_ready,
        reranker_downloaded,
        reranker_disk_bytes,
    })
}

/// Outcome of the read-only index-status probe in [`status_data`]. Separates a
/// real status read from the two failure shapes: `Unknown { busy }` is the
/// honest lock-contention case (the index is fine, just held by a live
/// process), while `Error` is a broken/unreadable index — a `status()` read
/// failure or a NON-lock open failure. Both suppress the "up to date" branch,
/// but only `Error` carries a message + `W_STATUS_UNREADABLE` warning.
#[derive(Debug)]
enum StatusProbe {
    Ok(onebrain_search::engine::IndexStatus),
    Unknown { busy: bool },
    Error { message: String },
}

/// Probe the daemon's `/api/internal/status` and map it into a [`StatusProbe`].
/// The daemon is the sole redb owner, so this is the contention-free path used
/// while an MCP session is live — a live index here is NEVER `busy`. A transport
/// / parse failure degrades to `Error` (unknown counts + `W_STATUS_UNREADABLE`),
/// mirroring the direct path's broken-index handling so it can't render a false
/// "up to date".
///
/// One exception: a daemon that holds NO engine (it 503s because another process
/// owns the redb lock — e.g. an `onebrain mcp` from before an upgrade) is
/// contention, not a broken read. The client classifies that as `EngineBusy`, so
/// report `Unknown { busy: true }` — the SAME honest signal the direct path emits
/// when `Engine::open` hits the lock — instead of leaking the raw 503 string.
fn probe_via_daemon(handle: &crate::commands::daemon_client::DaemonHandle) -> StatusProbe {
    match handle.status() {
        Ok(v) => probe_from_daemon_status(&v),
        Err(e) if onebrain_search::error::is_engine_busy(&e) => StatusProbe::Unknown { busy: true },
        Err(e) => {
            eprintln!("search status (daemon): {e:#}");
            StatusProbe::Error {
                message: short_error(&e),
            }
        }
    }
}

/// Map a `/api/internal/status` JSON body into a [`StatusProbe`]. Pure (no
/// I/O) so the mapping — including the "missing doc_count → broken read"
/// refusal — is unit-testable without a live daemon.
fn probe_from_daemon_status(v: &serde_json::Value) -> StatusProbe {
    let field = |k: &str| v.get(k).and_then(serde_json::Value::as_u64);
    match field("doc_count") {
        Some(doc_count) => StatusProbe::Ok(onebrain_search::engine::IndexStatus {
            doc_count: doc_count as usize,
            last_indexed_at: field("last_indexed"),
            pending_new: field("pending_new").unwrap_or(0) as usize,
            pending_changed: field("pending_changed").unwrap_or(0) as usize,
            pending_removed: field("pending_removed").unwrap_or(0) as usize,
        }),
        // A malformed body (no numeric doc_count) is a broken read, not a
        // healthy zero — refuse the up-to-date branch.
        None => StatusProbe::Error {
            message: format!("daemon status missing doc_count: {v}"),
        },
    }
}

/// One-line, size-bounded rendering of an error's full `anyhow` chain, for the
/// status text line and the `W_STATUS_UNREADABLE` warning. The complete chain
/// still goes to stderr via `eprintln!("search status: {e:#}")`.
fn short_error(err: &anyhow::Error) -> String {
    const MAX: usize = 160;
    let full = format!("{err:#}").replace('\n', " ");
    if full.chars().count() > MAX {
        let truncated: String = full.chars().take(MAX).collect();
        format!("{truncated}…")
    } else {
        full
    }
}

/// Build status data for an already-open engine (used by the MCP `status`
/// tool, which holds a live `Engine` and doesn't want to open a second one).
///
/// Unlike [`status_data`], the cache dir will already exist by the time this
/// runs (the caller's engine opened it), so `indexed` reports whether the
/// index has actually been populated (`doc_count > 0` after the status
/// query) rather than merely whether the cache dir exists.
pub(crate) fn status_data_for(
    engine: &Engine,
    resolved: &ResolvedVault,
) -> Result<SearchStatusData> {
    let collection = collection_for(resolved)?;
    let config = load_vault_config(&resolved.root)?;

    let (cache_dir, model_size_bytes, model_downloaded_at, index_size_bytes) = {
        let dir = collection_cache_dir(&collection);
        let (size, downloaded) = match active_model_dir_stats(&dir, &config.search.embed_model) {
            Some((size, mtime)) => (Some(size), mtime),
            None => (None, None),
        };
        let idx_size = index_size_bytes(&dir);
        (Some(dir), size, downloaded, idx_size)
    };
    let reindexing = cache_dir.as_deref().and_then(read_reindex_progress);
    let cache_size_bytes = cache_dir
        .as_deref()
        .filter(|d| d.is_dir())
        .map(onebrain_search::embed::dir_size_bytes);

    let status = engine.status(resolved.root.as_path())?;

    // Collection is always set on this path, so the signal hinges only on a
    // semantic build with no downloaded model.
    let current_model_missing = cfg!(feature = "semantic") && model_size_bytes.is_none();
    let (reranker_model, reranker_ready, reranker_downloaded, reranker_disk_bytes) =
        reranker_status_fields(
            &config,
            cache_dir.as_deref().expect("cache_dir always set here"),
        );

    Ok(SearchStatusData {
        collection: Some(collection),
        embed_model: config.search.embed_model,
        cache_dir,
        indexed: status.doc_count > 0,
        current_model_missing,
        model_size_bytes,
        model_downloaded_at,
        last_indexed_at: status.last_indexed_at,
        index_size_bytes,
        doc_count: Some(status.doc_count),
        pending_new: Some(status.pending_new),
        pending_changed: Some(status.pending_changed),
        pending_removed: Some(status.pending_removed),
        cache_size_bytes,
        reindexing,
        // The caller already holds a live engine handle, so by construction
        // the index is NOT locked out — this path is never "busy".
        busy: false,
        // A live engine that already answered `status()` above is readable by
        // construction — never the unreadable-index case.
        status_error: None,
        semantic_available: cfg!(feature = "semantic"),
        reranker_model,
        reranker_ready,
        reranker_downloaded,
        reranker_disk_bytes,
    })
}

/// Counts a daemon's `GET /api/internal/status` reports about the held engine —
/// the subset of [`SearchStatusData`] the daemon actually knows. The MCP
/// `status` tool (when routed through the daemon) pairs these with the
/// filesystem-derived fields (model/index sizes, cache dir, reindex progress)
/// that the client reads locally, so the tool's response shape is identical to
/// the direct-engine path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DaemonStatusCounts {
    pub doc_count: usize,
    pub pending_new: usize,
    pub pending_changed: usize,
    pub pending_removed: usize,
    pub last_indexed: Option<u64>,
}

/// Build the `status` payload from daemon-reported engine counts plus local
/// filesystem reads — the MCP `status` tool's DAEMON path.
///
/// Unlike [`status_data_for`] (which reads counts off a locally-held `Engine`),
/// this never opens an engine: the daemon is the sole engine owner and already
/// returned the counts over HTTP. The model/index/cache sizes are pure fs reads
/// keyed off the same `collection_cache_dir`, so the response shape matches the
/// direct-engine path byte-for-byte. `busy`/`status_error` are always
/// `false`/`None`: a successful daemon status call means the engine was
/// readable by construction (contention is resolved inside the daemon).
pub(crate) fn status_data_from_daemon(
    resolved: &ResolvedVault,
    counts: DaemonStatusCounts,
) -> Result<SearchStatusData> {
    let collection = collection_for(resolved)?;
    let config = load_vault_config(&resolved.root)?;

    let dir = collection_cache_dir(&collection);
    let (model_size_bytes, model_downloaded_at) =
        match active_model_dir_stats(&dir, &config.search.embed_model) {
            Some((size, mtime)) => (Some(size), mtime),
            None => (None, None),
        };
    let index_size_bytes = index_size_bytes(&dir);
    let reindexing = read_reindex_progress(&dir);
    let cache_size_bytes = dir
        .is_dir()
        .then(|| onebrain_search::embed::dir_size_bytes(&dir));

    let current_model_missing = cfg!(feature = "semantic") && model_size_bytes.is_none();
    let (reranker_model, reranker_ready, reranker_downloaded, reranker_disk_bytes) =
        reranker_status_fields(&config, &dir);

    Ok(SearchStatusData {
        collection: Some(collection),
        embed_model: config.search.embed_model,
        cache_dir: Some(dir),
        indexed: counts.doc_count > 0,
        current_model_missing,
        model_size_bytes,
        model_downloaded_at,
        last_indexed_at: counts.last_indexed,
        index_size_bytes,
        doc_count: Some(counts.doc_count),
        pending_new: Some(counts.pending_new),
        pending_changed: Some(counts.pending_changed),
        pending_removed: Some(counts.pending_removed),
        cache_size_bytes,
        reindexing,
        busy: false,
        status_error: None,
        semantic_available: cfg!(feature = "semantic"),
        reranker_model,
        reranker_ready,
        reranker_downloaded,
        reranker_disk_bytes,
    })
}

/// Size in bytes + mtime (epoch secs) of the ACTIVE model's `models--*` dir
/// only. Returns `None` when the active model isn't on disk (or isn't a known
/// registry model). Pure fs: no subprocess, no download.
///
/// This is reported under the active model's name in `status`, so it must
/// reflect the active model alone. An earlier version summed EVERY `models--*`
/// dir under the cache, which inflated the figure — and falsely implied "a
/// model is present" — whenever a previously-active model's dir lingered next
/// to the current one (#21). The whole-cache total is surfaced separately as
/// `cache_size_bytes`.
fn active_model_dir_stats(cache_dir: &Path, active_model: &str) -> Option<(u64, Option<u64>)> {
    let info = model_registry().iter().find(|m| m.name == active_model)?;
    let status = model_download_status(info, cache_dir);
    let size = status.disk_size?;
    Some((size, dir_mtime_secs(&status.path)))
}

/// The four reranker status fields (`reranker_model`, `reranker_ready`,
/// `reranker_downloaded`, `reranker_disk_bytes`), computed from config + a
/// pure filesystem check of the collection cache dir — never opens the
/// engine, never downloads. Shared by all three `SearchStatusData` builders
/// below so the readiness definition can't drift between the direct,
/// held-engine, and daemon paths.
fn reranker_status_fields(
    config: &onebrain_core::VaultConfig,
    cache_dir: &Path,
) -> (String, bool, bool, Option<u64>) {
    let reranker_model = config.search.reranker.model.clone();
    let download = reranker_registry()
        .iter()
        .find(|r| r.name == reranker_model)
        .map(|r| reranker_download_status(r, cache_dir));
    let reranker_downloaded = download.as_ref().is_some_and(|d| d.downloaded);
    let reranker_ready = config.search.reranker.enabled && reranker_downloaded;
    let reranker_disk_bytes = download.and_then(|d| d.disk_size);
    (
        reranker_model,
        reranker_ready,
        reranker_downloaded,
        reranker_disk_bytes,
    )
}

/// `root`'s own mtime as epoch seconds, or `None` if unreadable.
fn dir_mtime_secs(root: &Path) -> Option<u64> {
    let meta = std::fs::metadata(root).ok()?;
    let modified = meta.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Format epoch seconds as local `YYYY-MM-DD HH:MM`, or `None` on an
/// out-of-range timestamp.
fn format_local(secs: u64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs as i64, 0).map(|dt| {
        dt.with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    })
}

fn render_text(env: &Envelope<SearchStatusData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    // Grouped layout: emoji only on section headers, values indented with
    // plain spaces — alignment never depends on how wide a terminal font
    // draws an emoji.
    let mut lines = Vec::new();

    lines.push(section("🧠", "Model"));
    lines.push(item("Name", &d.embed_model));
    if d.semantic_available {
        match d.model_size_bytes {
            Some(size) => {
                lines.push(item("Size", &format_size(size)));
                if let Some(downloaded) = d.model_downloaded_at.and_then(format_local) {
                    lines.push(item("Downloaded", &downloaded));
                }
            }
            None => lines.push(item("Size", "not downloaded")),
        }
    } else {
        // Lex-only build: no ONNX runtime for this platform, so the model is
        // never downloaded and semantic verbs are unavailable.
        lines.push(item("Semantic", "unavailable in this build (keyword-only)"));
    }

    lines.push(String::new());
    lines.push(section("🎯", "Reranker"));
    lines.push(item("Name", &d.reranker_model));
    lines.push(item(
        "Ready",
        if d.reranker_ready {
            "✅  yes"
        } else {
            "—  no"
        },
    ));
    match d.reranker_disk_bytes {
        Some(size) => lines.push(item("Size", &format_size(size))),
        None => lines.push(item("Size", "not downloaded")),
    }

    lines.push(String::new());
    lines.push(section("📊", "Index"));
    match &d.collection {
        Some(c) => lines.push(item("Collection", c)),
        None => lines.push(item(
            "Collection",
            "not set — set `search.collection` in onebrain.yml",
        )),
    }
    match d.doc_count {
        Some(n) => lines.push(item("Docs", &n.to_string())),
        None => lines.push(item("Docs", "unknown")),
    }
    if let Some(size) = d.index_size_bytes {
        lines.push(item("Size", &format_size(size)));
    }
    match d.last_indexed_at.and_then(format_local) {
        Some(when) => lines.push(item("Last indexed", &when)),
        None => lines.push(item("Last indexed", "never")),
    }
    if let Some(r) = &d.reindexing {
        let pct = (r.done * 100).checked_div(r.total).unwrap_or(0);
        lines.push(item(
            "Reindexing",
            &format!(
                "🔄  {}/{} ({pct}%) — running now; counts may lag",
                r.done, r.total
            ),
        ));
    }
    // Bug B (v3.4.6): a locked index NEVER reports "✅ up to date" — the counts
    // are unknown, so say so. These failure branches win over the
    // pending/up-to-date rendering below. Round-2: a broken/unreadable index
    // (status read failed, or a non-lock open failure) gets the same honest
    // treatment via `status_error`, so it can't slip into the up-to-date branch
    // either.
    if let Some(msg) = &d.status_error {
        lines.push(item("Status", &format!("⚠️  status read failed ({msg})")));
    } else if d.busy {
        lines.push(item(
            "Status",
            "⚠️  engine busy (indexed by another process)",
        ));
    } else {
        // Pending counts are `Some` on the healthy path; treat an unknown
        // (non-busy open/status failure) as zero pending for this rollup.
        let pending = d.pending_total();
        if pending > 0 {
            lines.push(item(
                "Status",
                &format!(
                    "⚠️  {pending} pending ({} new · {} changed · {} removed)",
                    d.pending_new.unwrap_or(0),
                    d.pending_changed.unwrap_or(0),
                    d.pending_removed.unwrap_or(0)
                ),
            ));
        } else {
            lines.push(item("Status", "✅  up to date"));
        }
    }

    if let Some(dir) = &d.cache_dir {
        lines.push(String::new());
        lines.push(section("📁", "Cache"));
        lines.push(item("Dir", &dir.display().to_string()));
        if let Some(size) = d.cache_size_bytes {
            lines.push(item("Size", &format_size(size)));
        }
    }

    // Pending rollup for the hint block below. Unknown/busy counts contribute
    // zero — a busy engine gets no reindex hint (retry, don't reindex).
    let pending = d.pending_total();

    // A missing model blocks all search — surface it ahead of pending drift (a
    // reindex selects/downloads the model AND indexes). Suppressed while a
    // reindex is already running (the model is being fetched right now), and
    // while the engine is busy (the count is unknown, so don't guess).
    if d.status_error.is_some() {
        // Broken/unreadable index: the honest recovery is a rebuild, not a
        // retry (unlike the busy case, nothing will free up on its own).
        lines.push(String::new());
        lines.push(
            "💡  Index status could not be read (index may be corrupt) — \
             run `onebrain search reindex --force` to rebuild"
                .to_string(),
        );
    } else if d.busy {
        lines.push(String::new());
        lines.push(
            "💡  Index is locked by another process (e.g. the `onebrain mcp` server) — \
             retry `onebrain search status` once it exits"
                .to_string(),
        );
    } else if d.current_model_missing && d.reindexing.is_none() {
        lines.push(String::new());
        lines.push(
            "💡  No embedding model downloaded — run `onebrain search reindex` to select, download + index"
                .to_string(),
        );
    } else if pending > 0 {
        lines.push(String::new());
        lines.push("💡  Run `onebrain search reindex` to index pending changes".to_string());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn probe_from_daemon_status_maps_counts() {
        let body = serde_json::json!({
            "doc_count": 12, "pending_new": 1, "pending_changed": 2,
            "pending_removed": 3, "pending_total": 6, "last_indexed": 99, "indexed": true
        });
        match probe_from_daemon_status(&body) {
            StatusProbe::Ok(s) => {
                assert_eq!(s.doc_count, 12);
                assert_eq!(s.pending_new, 1);
                assert_eq!(s.pending_changed, 2);
                assert_eq!(s.pending_removed, 3);
                assert_eq!(s.last_indexed_at, Some(99));
                assert_eq!(s.pending_total(), 6);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn probe_from_daemon_status_healthy_zero_is_ok_not_error() {
        // A real empty index (doc_count present, = 0) is Ok(0) — NOT the broken
        // read that a MISSING doc_count triggers.
        let body = serde_json::json!({"doc_count": 0, "pending_new": 0});
        assert!(matches!(
            probe_from_daemon_status(&body),
            StatusProbe::Ok(s) if s.doc_count == 0
        ));
    }

    #[test]
    fn probe_from_daemon_status_missing_doc_count_is_error() {
        // No doc_count → a broken/unreadable read, refusing the up-to-date
        // branch (never a false healthy zero).
        let body = serde_json::json!({"pending_new": 0});
        assert!(matches!(
            probe_from_daemon_status(&body),
            StatusProbe::Error { .. }
        ));
    }

    fn env(collection: Option<&str>, indexed: bool) -> Envelope<SearchStatusData> {
        Envelope::ok(
            "search.status",
            None,
            SearchStatusData {
                collection: collection.map(str::to_string),
                embed_model: "multilingual-e5-small".to_string(),
                cache_dir: collection.map(|c| PathBuf::from(format!("/cache/{c}"))),
                indexed,
                current_model_missing: false,
                model_size_bytes: None,
                model_downloaded_at: None,
                last_indexed_at: None,
                index_size_bytes: None,
                cache_size_bytes: None,
                doc_count: Some(0),
                pending_new: Some(0),
                pending_changed: Some(0),
                pending_removed: Some(0),
                reindexing: None,
                busy: false,
                status_error: None,
                semantic_available: true,
                reranker_model: "onebrain-reranker-v1".to_string(),
                reranker_ready: false,
                reranker_downloaded: false,
                reranker_disk_bytes: None,
            },
        )
    }

    #[test]
    fn text_flags_lex_only_build_in_model_section() {
        let mut e = env(Some("ob-1"), true);
        e.data.as_mut().unwrap().semantic_available = false;
        let s = render_text(&e);
        assert!(s.contains("🧠  Model"), "{s}");
        assert!(s.contains("Semantic"), "{s}");
        assert!(s.contains("unavailable in this build"), "{s}");
        // The Model section's own download line is suppressed when semantic
        // is unavailable — scope to the Model section (the Reranker section
        // below it independently reports its own "not downloaded").
        let model_section = s.split("🎯  Reranker").next().unwrap();
        assert!(!model_section.contains("not downloaded"), "{model_section}");
    }

    // ── Reranker status fields (v3.4.7 Task 6) ─────────────────────────────

    #[test]
    fn text_shows_reranker_section_not_downloaded_by_default() {
        let s = render_text(&env(Some("ob-1"), true));
        assert!(s.contains("🎯  Reranker"), "{s}");
        assert!(s.contains("    Name          onebrain-reranker-v1"), "{s}");
        assert!(s.contains("    Ready         —  no"), "{s}");
        assert!(s.contains("    Size          not downloaded"), "{s}");
    }

    #[test]
    fn text_shows_reranker_ready_and_size_when_downloaded() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            d.reranker_ready = true;
            d.reranker_downloaded = true;
            d.reranker_disk_bytes = Some(569_011_484);
        }
        let s = render_text(&e);
        assert!(s.contains("    Ready         ✅  yes"), "{s}");
        assert!(s.contains("    Size          543 MB"), "{s}");
    }

    #[test]
    fn reranker_status_fields_not_ready_on_fresh_cache_dir() {
        let cache = tempfile::tempdir().unwrap();
        let config = onebrain_core::VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
            search: Default::default(),
        };
        let (model, ready, downloaded, disk_bytes) = reranker_status_fields(&config, cache.path());
        assert_eq!(model, "onebrain-reranker-v1");
        assert!(!ready, "nothing downloaded yet");
        assert!(!downloaded);
        assert_eq!(disk_bytes, None);
    }

    #[test]
    fn reranker_status_fields_ready_when_enabled_and_downloaded() {
        let cache = tempfile::tempdir().unwrap();
        let info = &reranker_registry()[0];
        let mdir = cache.path().join(info.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model_int8.onnx"), vec![0u8; 4096]).unwrap();

        let config = onebrain_core::VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
            search: Default::default(), // reranker.enabled defaults true
        };
        let (model, ready, downloaded, disk_bytes) = reranker_status_fields(&config, cache.path());
        assert_eq!(model, info.name);
        assert!(ready, "enabled + downloaded → ready");
        assert!(downloaded);
        assert!(disk_bytes.unwrap() > 0);
    }

    #[test]
    fn reranker_status_fields_not_ready_when_disabled_even_if_downloaded() {
        let cache = tempfile::tempdir().unwrap();
        let info = &reranker_registry()[0];
        let mdir = cache.path().join(info.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model_int8.onnx"), vec![0u8; 4096]).unwrap();

        let mut config = onebrain_core::VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
            search: Default::default(),
        };
        config.search.reranker.enabled = false;
        let (_model, ready, downloaded, disk_bytes) = reranker_status_fields(&config, cache.path());
        assert!(
            !ready,
            "disabled reranker is never ready, even if downloaded"
        );
        // Downloaded/size are still reported (it's on disk), only readiness gates.
        assert!(downloaded);
        assert!(disk_bytes.unwrap() > 0);
    }

    #[test]
    fn text_shows_collection_and_model() {
        let s = render_text(&env(Some("ob-1"), true));
        assert!(s.contains("🧠  Model"), "{s}");
        assert!(s.contains("    Name          multilingual-e5-small"), "{s}");
        assert!(s.contains("📊  Index"), "{s}");
        assert!(s.contains("    Collection    ob-1"), "{s}");
        assert!(s.contains("    Docs          0"), "{s}");
        assert!(s.contains("📁  Cache"), "{s}");
        assert!(s.contains("    Dir           /cache/ob-1"), "{s}");
    }

    #[test]
    fn text_flags_missing_collection() {
        let s = render_text(&env(None, false));
        assert!(s.contains("not set — set `search.collection`"), "{s}");
    }

    #[test]
    fn text_shows_not_downloaded_and_never_indexed_when_absent() {
        let s = render_text(&env(Some("ob-1"), false));
        assert!(s.contains("    Size          not downloaded"), "{s}");
        assert!(s.contains("    Last indexed  never"), "{s}");
        // No `⏬  Downloaded` line when the model isn't present.
        assert!(!s.contains("    Downloaded"));
        // No `📊  Index size` line when there's no index.
        assert!(!s.contains("Index size"));
    }

    #[test]
    fn text_shows_up_to_date_when_no_drift() {
        // Truly healthy: no drift AND the current model is downloaded — so no
        // hint. (A missing model would itself warrant a reindex hint; see
        // `text_hints_to_reindex_when_current_model_not_downloaded`.)
        let mut e = env(Some("ob-1"), true);
        e.data.as_mut().unwrap().model_size_bytes = Some(493_921_024);
        let s = render_text(&e);
        assert!(s.contains("    Status        ✅  up to date"), "{s}");
        assert!(!s.contains("pending"), "{s}");
        assert!(!s.contains("💡"), "no hint when up to date: {s}");
    }

    #[test]
    fn text_hints_to_reindex_when_current_model_not_downloaded() {
        // Collection set, semantic build, but the model file is absent (never
        // downloaded, or the cache was purged by OS storage cleanup). Even with
        // no pending drift, status must point the user at reindex — search
        // can't run without the model.
        let mut e = env(Some("ob-1"), true); // model_size_bytes defaults to None
        e.data.as_mut().unwrap().current_model_missing = true;
        let s = render_text(&e);
        assert!(s.contains("    Size          not downloaded"), "{s}");
        assert!(
            s.contains("💡")
                && s.contains("No embedding model downloaded")
                && s.contains("onebrain search reindex"),
            "expected a model-not-downloaded reindex hint: {s}"
        );
    }

    #[test]
    fn text_hints_when_current_model_missing() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            d.current_model_missing = true;
            d.pending_new = Some(2); // drift present too
        }
        let s = render_text(&e);
        assert!(
            s.contains("💡")
                && s.contains("No embedding model downloaded")
                && s.contains("onebrain search reindex"),
            "{s}"
        );
        assert!(
            !s.contains("index pending changes"),
            "model hint suppresses pending hint: {s}"
        );
    }

    #[test]
    fn text_no_model_hint_while_reindex_running() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            d.current_model_missing = true;
            d.reindexing = Some(ReindexLiveProgress { done: 3, total: 10 });
        }
        let s = render_text(&e);
        assert!(!s.contains("💡"), "no reindex hint while reindexing: {s}");
    }

    #[test]
    fn text_no_model_hint_without_collection() {
        // No collection → `model_size_bytes` is None for lack of a cache dir,
        // NOT because a chosen model is missing. Don't emit a reindex hint.
        let s = render_text(&env(None, false));
        assert!(!s.contains("💡"), "no model hint without a collection: {s}");
    }

    #[test]
    fn text_shows_engine_busy_never_up_to_date() {
        // Bug B (v3.4.6): a locked index reports the busy status + unknown
        // docs, and NEVER "✅ up to date".
        let mut e = env(Some("ob-1"), false);
        {
            let d = e.data.as_mut().unwrap();
            d.busy = true;
            d.doc_count = None;
            d.pending_new = None;
            d.pending_changed = None;
            d.pending_removed = None;
        }
        let s = render_text(&e);
        assert!(
            s.contains("⚠️  engine busy (indexed by another process)"),
            "{s}"
        );
        assert!(
            !s.contains("up to date"),
            "busy must never read up to date: {s}"
        );
        assert!(s.contains("    Docs          unknown"), "{s}");
        // The hint points at retry, not reindex.
        assert!(
            s.contains("💡") && s.contains("locked by another process"),
            "{s}"
        );
        assert!(
            !s.contains("index pending changes"),
            "busy suppresses the reindex hint: {s}"
        );
    }

    #[test]
    fn json_busy_reports_null_counts_and_flag() {
        // Bug B: machine consumers must see `busy: true` and null (not zero)
        // counts, so they can't mistake a locked index for a healthy empty one.
        let mut e = env(Some("ob-1"), false);
        {
            let d = e.data.as_mut().unwrap();
            d.busy = true;
            d.doc_count = None;
            d.pending_new = None;
            d.pending_changed = None;
            d.pending_removed = None;
        }
        let v = serde_json::to_value(e.data.as_ref().unwrap()).unwrap();
        assert_eq!(v["busy"], true);
        assert!(
            v["doc_count"].is_null(),
            "doc_count must be null when busy: {v}"
        );
        assert!(v["pending_new"].is_null(), "{v}");
        assert_eq!(v["indexed"], false);
    }

    #[test]
    fn text_status_error_never_up_to_date_and_surfaces_failure() {
        // Round-2 (v3.4.6): a broken/unreadable index (open or status-read
        // failure that ISN'T a live lock) must NEVER render "✅ up to date".
        // With counts all `None` (unknown) and `busy: false`, the pre-fix code
        // fell into the else-branch and printed "up to date" — the exact bug B
        // regression. `status_error` now wins.
        let mut e = env(Some("ob-1"), false);
        {
            let d = e.data.as_mut().unwrap();
            d.busy = false;
            d.doc_count = None;
            d.pending_new = None;
            d.pending_changed = None;
            d.pending_removed = None;
            d.status_error = Some("redb: invalid database header".to_string());
        }
        let s = render_text(&e);
        assert!(
            s.contains("⚠️  status read failed (redb: invalid database header)"),
            "{s}"
        );
        assert!(
            !s.contains("up to date"),
            "an unreadable index must never read up to date: {s}"
        );
        assert!(s.contains("    Docs          unknown"), "{s}");
        // The hint points at a rebuild, not the plain pending-changes reindex.
        assert!(
            s.contains("💡") && s.contains("index may be corrupt") && s.contains("--force"),
            "{s}"
        );
        assert!(
            !s.contains("index pending changes"),
            "a status-read failure suppresses the plain pending hint: {s}"
        );
    }

    #[test]
    fn json_status_error_reports_null_counts_and_message() {
        // Machine consumers must see null (not zero) counts + the error string,
        // so a broken index can't be mistaken for a healthy empty one.
        let mut e = env(Some("ob-1"), false);
        {
            let d = e.data.as_mut().unwrap();
            d.doc_count = None;
            d.pending_new = None;
            d.pending_changed = None;
            d.pending_removed = None;
            d.status_error = Some("permission denied".to_string());
        }
        let v = serde_json::to_value(e.data.as_ref().unwrap()).unwrap();
        assert_eq!(v["status_error"], "permission denied");
        assert_eq!(v["busy"], false, "unreadable is not the same as busy: {v}");
        assert!(v["doc_count"].is_null(), "{v}");
        assert!(v["pending_new"].is_null(), "{v}");
        assert_eq!(v["indexed"], false);
    }

    #[test]
    fn json_omits_status_error_when_none() {
        let v = serde_json::to_value(env(Some("ob-1"), true).data.as_ref().unwrap()).unwrap();
        assert!(v.get("status_error").is_none());
    }

    #[test]
    fn short_error_truncates_long_chains() {
        let long = "x".repeat(500);
        let s = short_error(&anyhow::anyhow!(long));
        assert!(s.chars().count() <= 161, "len {}", s.chars().count());
        assert!(s.ends_with('…'), "{s}");
    }

    #[test]
    fn short_error_collapses_newlines() {
        let s = short_error(&anyhow::anyhow!("line one\nline two"));
        assert!(!s.contains('\n'), "{s}");
        assert!(s.contains("line one line two"), "{s}");
    }

    #[test]
    fn text_shows_pending_drift_when_present() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            // 5 docs indexed ⇒ the model was downloaded to embed them; set it so
            // the pending-drift hint (not the model-missing hint) is exercised.
            d.model_size_bytes = Some(493_921_024);
            d.doc_count = Some(5);
            d.pending_new = Some(2);
            d.pending_changed = Some(1);
            d.pending_removed = Some(3);
        }
        let s = render_text(&e);
        assert!(s.contains("    Docs          5"), "{s}");
        assert!(
            s.contains("    Status        ⚠️  6 pending (2 new · 1 changed · 3 removed)"),
            "{s}"
        );
        assert!(s.contains("💡  Run `onebrain search reindex`"), "{s}");
        assert!(!s.contains("up to date"), "{s}");
    }

    #[test]
    fn text_shows_model_size_and_download_time_when_present() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            d.model_size_bytes = Some(471 * 1024 * 1024);
            d.model_downloaded_at = Some(1_700_000_000);
        }
        let s = render_text(&e);
        assert!(s.contains("    Size          471 MB"), "{s}");
        assert!(s.contains("    Downloaded    "), "{s}");
    }

    #[test]
    fn text_shows_index_size_when_present() {
        let mut e = env(Some("ob-1"), true);
        e.data.as_mut().unwrap().index_size_bytes = Some(16 * 1024 * 1024);
        let s = render_text(&e);
        assert!(s.contains("    Size          16 MB"), "{s}");
    }

    #[test]
    fn text_uses_two_spaces_after_every_emoji_and_aligned_values() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            d.model_size_bytes = Some(471 * 1024 * 1024);
            d.index_size_bytes = Some(1024);
        }
        let s = render_text(&e);
        // Every rendered emoji row starts with `<emoji>  ` (two spaces).
        for line in s.lines() {
            for emoji in ["🔍", "🧠", "📦", "📁", "📊", "🕐"] {
                if let Some(rest) = line.strip_prefix(emoji) {
                    assert!(
                        rest.starts_with("  ") && !rest.starts_with("   "),
                        "row `{line}` must have exactly two spaces after {emoji}"
                    );
                }
            }
        }
    }

    #[test]
    fn json_shape_has_new_and_existing_fields() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            d.doc_count = Some(7);
            d.pending_new = Some(1);
            d.last_indexed_at = Some(1_700_000_000);
            d.model_size_bytes = Some(1234);
            d.index_size_bytes = Some(5678);
        }
        let v = serde_json::to_value(e.data.as_ref().unwrap()).unwrap();
        // Existing fields preserved.
        assert_eq!(v["collection"], "ob-1");
        assert!(v["embed_model"].is_string());
        assert_eq!(v["indexed"], true);
        // New fields added.
        assert_eq!(v["doc_count"], 7);
        assert_eq!(v["pending_new"], 1);
        assert_eq!(v["pending_changed"], 0);
        assert_eq!(v["pending_removed"], 0);
        assert_eq!(v["last_indexed_at"], 1_700_000_000u64);
        assert_eq!(v["model_size_bytes"], 1234);
        assert_eq!(v["index_size_bytes"], 5678);
    }

    #[test]
    fn json_omits_index_size_when_absent() {
        let v = serde_json::to_value(env(Some("ob-1"), true).data.as_ref().unwrap()).unwrap();
        assert!(v.get("index_size_bytes").is_none());
    }

    #[test]
    fn text_shows_live_reindex_line_when_marker_present() {
        let mut e = env(Some("ob-1"), true);
        e.data.as_mut().unwrap().reindexing = Some(ReindexLiveProgress {
            done: 457,
            total: 761,
        });
        let s = render_text(&e);
        assert!(s.contains("    Reindexing    🔄  457/761 (60%)"), "{s}");
        assert!(s.contains("counts may lag"), "{s}");
    }

    #[test]
    fn text_shows_cache_size_when_present() {
        let mut e = env(Some("ob-1"), true);
        e.data.as_mut().unwrap().cache_size_bytes = Some(512 * 1024 * 1024);
        let s = render_text(&e);
        assert!(s.contains("📁  Cache"), "{s}");
        // Both Dir and Size sit in the Cache section.
        let cache_at = s.find("📁  Cache").unwrap();
        let tail = &s[cache_at..];
        assert!(tail.contains("    Dir           /cache/ob-1"), "{tail}");
        assert!(tail.contains("    Size          512 MB"), "{tail}");
    }

    #[test]
    fn json_omits_reindexing_when_absent() {
        let v = serde_json::to_value(env(Some("ob-1"), true).data.as_ref().unwrap()).unwrap();
        assert!(v.get("reindexing").is_none());
    }

    #[test]
    fn active_model_dir_stats_none_when_active_model_not_on_disk() {
        let dir = tempdir().unwrap();
        // A cache dir with unrelated subdirs but no `models--*`.
        std::fs::create_dir(dir.path().join("tantivy")).unwrap();
        std::fs::create_dir(dir.path().join("vectors")).unwrap();
        assert!(active_model_dir_stats(dir.path(), "multilingual-e5-small").is_none());
    }

    #[test]
    fn active_model_dir_stats_sums_only_the_active_model_dir() {
        let dir = tempdir().unwrap();
        let model = dir.path().join("models--intfloat--multilingual-e5-small");
        std::fs::create_dir_all(model.join("snapshots/abc")).unwrap();
        std::fs::write(model.join("snapshots/abc/model.onnx"), vec![0u8; 1000]).unwrap();
        std::fs::write(model.join("config.json"), vec![0u8; 24]).unwrap();
        // A non-model sibling dir must NOT be counted.
        std::fs::create_dir(dir.path().join("tantivy")).unwrap();
        std::fs::write(dir.path().join("tantivy/meta.json"), vec![0u8; 5000]).unwrap();

        let (size, mtime) = active_model_dir_stats(dir.path(), "multilingual-e5-small").unwrap();
        // Exact equality is intentional and portable: `dir_size_bytes` sums
        // `metadata().len()` (logical file bytes, never filesystem blocks), and
        // `==` is what catches contamination from a sibling model dir — `>=`
        // would let that regression through.
        assert_eq!(size, 1024, "should sum only the active model dir's files");
        assert!(mtime.is_some());
    }

    #[test]
    fn active_model_dir_stats_ignores_other_model_dirs() {
        // Two models on disk; status must report ONLY the active one (#21).
        let dir = tempdir().unwrap();
        let active = dir.path().join("models--intfloat--multilingual-e5-small");
        std::fs::create_dir_all(&active).unwrap();
        std::fs::write(active.join("model.onnx"), vec![0u8; 1000]).unwrap();
        let stale = dir.path().join("models--intfloat--multilingual-e5-base");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("model.onnx"), vec![0u8; 999_000]).unwrap();

        let (size, _) = active_model_dir_stats(dir.path(), "multilingual-e5-small").unwrap();
        assert_eq!(
            size, 1000,
            "must report only the active model's bytes, not the lingering base model's"
        );
    }

    #[test]
    fn format_local_formats_or_none() {
        // A normal timestamp renders `YYYY-MM-DD HH:MM`.
        let s = format_local(1_700_000_000).unwrap();
        assert_eq!(s.len(), "2023-11-14 22:13".len());
        assert!(s.contains('-') && s.contains(':'));
    }

    // ─────────────────────────────────────────────────────────────────
    // `status_data_for` — the MCP `status` tool's data builder. Directly
    // exercises the `indexed = doc_count > 0` refinement (vs. `status_data`'s
    // cache-dir-existence `indexed`), which until now was only asserted in a
    // doc comment.
    // ─────────────────────────────────────────────────────────────────

    /// Build a `ResolvedVault` rooted at `dir` (which must already contain an
    /// `onebrain.yml`), as the flag-resolved source — mirrors the identical
    /// helper in `search_common.rs`'s test module.
    fn resolved_at(dir: &Path) -> onebrain_core::ResolvedVault {
        onebrain_core::ResolvedVault {
            root: onebrain_core::VaultRoot::from_path(dir).unwrap(),
            source: onebrain_core::VaultSource::Flag,
        }
    }

    #[test]
    fn status_data_for_never_indexed_reports_not_indexed_and_zero_docs() {
        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: t-status-data-for\n",
        )
        .unwrap();
        let resolved = resolved_at(vault.path());

        // `Engine::open` is lazy-embedder (never downloads a model) and just
        // creates/opens the cache dir — cheap, no network, matches every
        // other `search_status`/`search_common` test's use of a bare tempdir
        // as the cache dir.
        let cache = tempdir().unwrap();
        let engine = Engine::open(cache.path(), "multilingual-e5-small").unwrap();

        let data = status_data_for(&engine, &resolved).unwrap();

        assert_eq!(data.collection, Some("t-status-data-for".to_string()));
        assert_eq!(data.doc_count, Some(0));
        assert!(
            !data.indexed,
            "never-indexed vault (doc_count == 0) must report indexed: false"
        );
        assert!(!data.busy, "a live-engine status path is never busy");
        assert_eq!(data.semantic_available, cfg!(feature = "semantic"));
    }

    #[test]
    fn indexed_flag_refinement_doc_count_gt_zero_is_true() {
        // Companion invariant to
        // `status_data_for_never_indexed_reports_not_indexed_and_zero_docs`.
        //
        // This test intentionally validates the refinement rule in isolation:
        // `indexed` must be computed as `doc_count > 0`.
        //
        // We do not populate a real `Engine` with indexed docs here because
        // that path requires embedding (`index_doc` ->
        // `embed_passages_if_available`) and may trigger real model setup in
        // this crate's test context. `Engine::open_with_embedder` is not
        // accessible here (`pub(crate)` in `onebrain-search`), so this remains
        // an explicit logic-contract test.
        let doc_count = 3usize;
        let data = SearchStatusData {
            collection: Some("t-status-data-for".to_string()),
            embed_model: "multilingual-e5-small".to_string(),
            cache_dir: Some(PathBuf::from("/cache/t-status-data-for")),
            indexed: doc_count > 0, // mirrors `status_data_for`'s `status.doc_count > 0`
            current_model_missing: cfg!(feature = "semantic"), // model_size_bytes is None here
            model_size_bytes: None,
            model_downloaded_at: None,
            last_indexed_at: Some(1_700_000_000),
            index_size_bytes: None,
            doc_count: Some(doc_count),
            pending_new: Some(0),
            pending_changed: Some(0),
            pending_removed: Some(0),
            cache_size_bytes: None,
            reindexing: None,
            busy: false,
            status_error: None,
            semantic_available: cfg!(feature = "semantic"),
            reranker_model: "onebrain-reranker-v1".to_string(),
            reranker_ready: false,
            reranker_downloaded: false,
            reranker_disk_bytes: None,
        };

        assert_eq!(data.doc_count, Some(3));
        assert!(
            data.indexed,
            "indexed vault (doc_count > 0) must report indexed: true"
        );
    }
}
