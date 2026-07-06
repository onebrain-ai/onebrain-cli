//! `/api/internal/*` — the warm-daemon's engine-owner endpoints.
//!
//! redb (the search engine's KV store) is single-process: exactly ONE process
//! may open a given collection's engine at a time. The daemon opens it ONCE at
//! boot (held in [`AppState::search_engine`]) and exposes reindex + status over
//! HTTP so the CLI `onebrain search …` verbs and the native MCP server can be
//! *clients* of that one engine instead of each opening their own (which redb
//! would reject with "Database already open"). Those client wirings live in
//! separate tracks; this module is the daemon side + the shared route table.
//!
//! ```text
//!   POST /api/internal/reindex   { mode: "pending" | "paths", paths?: [..] }
//!                                → run the engine reindex, return a status JSON
//!   GET  /api/internal/status    → live { doc_count, pending_*, last_indexed,
//!                                          indexed } from the held engine
//!   POST /api/internal/rerank    { query, paths: [..], top_k }
//!                                → doc-level Tier-2 rerank of the given
//!                                  candidate paths, returns `{ hits: [..] }`
//! ```
//!
//! Both routes are gated by the same token-auth middleware as the rest of the
//! surface (applied by [`super::build_router`]) and both require the daemon to
//! actually hold an engine — a `serve` process / unit-test router with no held
//! engine gets 503 (`ApiError::ServiceUnavailable`), never a per-request open,
//! because internal callers depend on the single-owner invariant.
//!
//! Reindex writes serialise on the engine's blocking [`std::sync::Mutex`]; the
//! mode maps to the SAME engine calls the CLI `search reindex` verbs use, so
//! the behaviour never drifts:
//! - `pending` → embed exactly `Engine::pending_vector_paths` (mirrors
//!   `search reindex --pending-only`).
//! - `paths`   → `Engine::reindex_paths` over the caller-supplied doc paths
//!   (mirrors `search reindex <paths…>`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use super::api::{require_vault_root, ApiError};
use super::{AppState, SharedEngine};
use crate::commands::search_common::{
    collection_cache_dir, collection_name_readonly, rerank_settings_from_config,
};
use onebrain_search::engine::Engine;
use onebrain_search::rerank::{reranker_download_status, reranker_registry};

/// Build the `/api/internal` sub-router. State + auth are attached by
/// [`super::build_router`] for the whole tree, so this stays a pure route table.
pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Engine-INDEPENDENT liveness: 200 whenever the process is up, whether
        // or not it holds an engine. The client's `is_live()` MUST probe this
        // (not `/internal/status`, which 503s with no engine) — otherwise a
        // live daemon that hasn't opened an engine reads as dead and gets its
        // `daemon.json` deleted + respawned in a loop.
        .route("/health", get(get_health))
        .route("/internal/status", get(get_internal_status))
        .route("/internal/get", get(get_internal_get))
        .route("/internal/reindex", post(post_internal_reindex))
        .route("/internal/rerank", post(post_internal_rerank))
}

/// `GET /api/health` — the engine-independent liveness probe. Returns
/// `{ ok: true, engine_held }` with 200 whenever the daemon is up. Still behind
/// the token-auth middleware (the whole surface is), so it leaks nothing to an
/// unauthenticated caller; it just doesn't depend on the search engine being
/// held, which is exactly what a liveness check must not do.
///
/// The vault identity a CLI client vault-matches against is NOT surfaced here —
/// it reads the daemon's canonical bound vault from `daemon.json` (`DaemonInfo.
/// vault`, written at bind time and rung true by the liveness probe), so there
/// is no second served-vault source to keep in sync. See #170's `vault_decision`
/// + the passive `daemon_client::discover_matching`.
async fn get_health(State(state): State<Arc<AppState>>) -> Response {
    Json(serde_json::json!({
        "ok": true,
        "engine_held": state.search_engine.is_some(),
    }))
    .into_response()
}

/// Open the one engine the daemon holds for its lifetime, rooted at the vault's
/// collection cache dir. Read-only collection resolution (never persists
/// `search.collection` — the daemon must not mutate config), matching
/// `search_common::collection_name_readonly`. Returns `None` (so search falls
/// back to per-request open) on any failure — a vault with no config, an
/// unopenable engine, etc. — logged at `warn` so a misconfigured daemon is
/// diagnosable from `daemon.log`.
pub(super) fn open_held_engine(vault_root: &Path) -> Option<SharedEngine> {
    match try_open_held_engine(vault_root) {
        Ok((engine, reranker_model, cache_dir)) => {
            tracing::info!(
                vault = %vault_root.display(),
                "daemon holding search engine for process lifetime"
            );
            let shared = Arc::new(std::sync::Mutex::new(engine));
            spawn_reranker_warm_thread(shared.clone(), reranker_model, cache_dir);
            Some(shared)
        }
        Err(e) => {
            tracing::warn!(
                vault = %vault_root.display(),
                error = %e,
                "could not open persistent search engine; search falls back to per-request open"
            );
            None
        }
    }
}

/// Fire-and-forget background warm-load: nudge the lazy Tier-2 reranker to
/// resolve now, off the request path, so the FIRST reranked query after boot
/// doesn't pay the (multi-hundred-MB) cross-encoder's cold-load latency.
///
/// Cheap presence check FIRST (a plain fs stat via [`reranker_download_status`],
/// done here BEFORE ever touching the engine mutex) — only when the configured
/// model is actually downloaded does the thread touch the engine at all, so a
/// fresh vault with no model spawns a thread that does no lock work and exits
/// immediately.
///
/// When the model IS downloaded, resolving the `OnceCell` LOADS the
/// cross-encoder from disk into memory — hundreds of MB, up to seconds —
/// and the engine mutex is held for that whole construction (it is a
/// blocking `std::sync::Mutex`; there is no way to construct outside it).
/// To guarantee the warm-up can only ever run in an idle window and never
/// serializes a real request behind the load, the thread uses `try_lock`:
/// if ANYTHING else holds the engine (a query racing daemon boot — which
/// would resolve the cell itself anyway), the thread just gives up. The
/// download itself never happens here — that belongs to `reindex`.
///
/// Must never panic the daemon: any failure (poisoned lock, disabled
/// reranker, load error) is swallowed — `rerank_active()` itself is
/// skip-not-fail by design (see `Engine::reranker`), and a poisoned mutex is
/// recovered via `into_inner()` like every other engine lock site in this
/// module.
///
/// Returns the `JoinHandle` so tests can join it deterministically instead of
/// racing a detached thread; [`open_held_engine`] (the production caller)
/// drops it, which is exactly a fire-and-forget spawn.
fn spawn_reranker_warm_thread(
    engine: SharedEngine,
    reranker_model: String,
    cache_dir: PathBuf,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let downloaded = reranker_registry()
            .iter()
            .find(|m| m.name == reranker_model)
            .is_some_and(|info| reranker_download_status(info, &cache_dir).downloaded);
        if !downloaded {
            // No model on disk yet — nothing to warm; a reindex will fetch it.
            return;
        }
        // Resolve the lazy reranker now so the first real query hits an
        // already-warm cell. try_lock: warm-up must only run in an idle
        // window — if anything else holds the engine, back off and let it
        // resolve the cell itself (same total cost, no added contention).
        match engine.try_lock() {
            Ok(guard) => {
                let _ = guard.rerank_active();
            }
            Err(std::sync::TryLockError::Poisoned(p)) => {
                let _ = p.into_inner().rerank_active();
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
    })
}

/// Fallible core of [`open_held_engine`]: resolve the collection (read-only),
/// open the engine at its cache dir, and apply the vault's exclude patterns —
/// exactly what `search_common::open_engine` does, minus the config-persisting
/// collection resolver (the daemon must never write config). Also returns the
/// configured reranker model name + the collection's cache dir, so the caller
/// can spawn the warm-load thread without re-deriving them (and without any
/// new `Engine` accessor).
fn try_open_held_engine(vault_root: &Path) -> anyhow::Result<(Engine, String, PathBuf)> {
    let config = onebrain_core::load_vault_config_at(vault_root)?;
    let collection = collection_name_readonly(vault_root)?;
    let cache_dir = collection_cache_dir(&collection);
    let mut engine = Engine::open(&cache_dir, &config.search.embed_model)?;
    engine.set_exclude_patterns(config.search.exclude.clone());
    engine.set_rerank_settings(rerank_settings_from_config(&config.search.reranker));
    Ok((engine, config.search.reranker.model, cache_dir))
}

/// Pull the held engine out of state, or 503 when the daemon holds none. The
/// internal routes REQUIRE the single-owner engine — unlike `/api/vault/search`
/// they never fall back to a per-request open, because their callers (the CLI /
/// MCP client tracks) route here precisely to avoid a second redb opener.
fn require_engine(state: &AppState) -> Result<&SharedEngine, ApiError> {
    state
        .search_engine
        .as_ref()
        .ok_or_else(|| ApiError::ServiceUnavailable("daemon holds no search engine".to_string()))
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/internal/status
// ─────────────────────────────────────────────────────────────────────────

/// Live index status from the held engine. Field names mirror the CLI
/// `search status` / MCP `status` shape so a client can forward this verbatim.
#[derive(Debug, Serialize, PartialEq)]
struct InternalStatusResponse {
    /// Distinct docs currently indexed.
    doc_count: usize,
    /// Docs on disk with no stored hash (would be added by a reindex).
    pending_new: usize,
    /// Docs whose content hash drifted from the index.
    pending_changed: usize,
    /// Indexed docs whose file is gone from disk.
    pending_removed: usize,
    /// Total pending drift (`new + changed + removed`).
    pending_total: usize,
    /// Epoch seconds of the last full reindex/embed, or `None` if never.
    last_indexed: Option<u64>,
    /// `true` once at least one doc is indexed (`doc_count > 0`) — the same
    /// "real index" definition `search status` uses, not mere cache-dir
    /// existence.
    indexed: bool,
    /// Configured `search.reranker.model` name (Tier-2 cross-encoder). Mirrors
    /// the CLI's `SearchStatusData::reranker_model`.
    reranker_model: String,
    /// `true` when the Tier-2 rerank stage will actually run for the NEXT
    /// query: unlike the CLI's config+filesystem-only `reranker_ready` (see
    /// `search_status.rs::reranker_status_fields`), this reads the LIVE held
    /// engine's `Engine::rerank_active()` — settings enabled AND a reranker
    /// that is loaded (or successfully loadable) right now. The daemon is the
    /// one place that can ask the engine directly, so this is the most
    /// honest of the three surfaces.
    reranker_ready: bool,
    /// `true` when the configured reranker model's `models--*` dir is present
    /// on disk. Mirrors the CLI's `SearchStatusData::reranker_downloaded`.
    reranker_downloaded: bool,
    /// On-disk byte size of the configured reranker's downloaded model dir,
    /// `None` if not downloaded. Mirrors the CLI's `reranker_disk_bytes`.
    reranker_disk_bytes: Option<u64>,
}

async fn get_internal_status(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();
    let engine = require_engine(&state)?.clone();

    // Engine status is a synchronous hash-walk (no async, no embedder) — run it
    // off the async runtime under the blocking mutex. The reranker fields piggy-
    // back on the SAME blocking closure + lock (rather than a second
    // spawn_blocking) since `rerank_active()` also needs the engine.
    let (status, reranker_model, reranker_ready, reranker_downloaded, reranker_disk_bytes) =
        tokio::task::spawn_blocking(move || {
            let config = onebrain_core::load_vault_config_at(&root)?;
            let collection = collection_name_readonly(&root)?;
            let cache_dir = collection_cache_dir(&collection);
            let reranker_model = config.search.reranker.model.clone();
            let download = reranker_registry()
                .iter()
                .find(|r| r.name == reranker_model)
                .map(|r| reranker_download_status(r, &cache_dir));
            let reranker_downloaded = download.as_ref().is_some_and(|d| d.downloaded);
            let reranker_disk_bytes = download.and_then(|d| d.disk_size);

            let engine = engine.lock().unwrap_or_else(|p| p.into_inner());
            let reranker_ready = engine.rerank_active();
            let status = engine.status(&root)?;
            Ok::<_, anyhow::Error>((
                status,
                reranker_model,
                reranker_ready,
                reranker_downloaded,
                reranker_disk_bytes,
            ))
        })
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "internal status task panicked");
            ApiError::Internal("status failed".to_string())
        })?
        .map_err(|e| {
            tracing::warn!(error = %e, "engine status failed");
            ApiError::Internal("status failed".to_string())
        })?;

    let resp = InternalStatusResponse {
        doc_count: status.doc_count,
        pending_new: status.pending_new,
        pending_changed: status.pending_changed,
        pending_removed: status.pending_removed,
        pending_total: status.pending_total(),
        last_indexed: status.last_indexed_at,
        indexed: status.doc_count > 0,
        reranker_model,
        reranker_ready,
        reranker_downloaded,
        reranker_disk_bytes,
    };
    Ok(Json(resp).into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/internal/get
// ─────────────────────────────────────────────────────────────────────────

/// Query string for `GET /api/internal/get`.
#[derive(Debug, Deserialize)]
struct InternalGetQuery {
    /// Vault-relative, forward-slash doc path (an index key), e.g.
    /// `00-inbox/note.md`.
    path: String,
}

/// A doc's full indexed text (the `search get` shape). Mirrors the CLI's
/// `SearchGetData` so a client can forward it verbatim.
#[derive(Debug, Serialize, PartialEq)]
struct InternalGetResponse {
    doc_path: String,
    content: String,
}

/// `GET /api/internal/get?path=<doc>` — the held engine's `Engine::get` (a redb
/// read). The daemon is the sole redb owner, so the CLI `search get` verb
/// routes here while a session is live instead of opening a second engine and
/// hitting the single-writer lock. A doc that isn't indexed is a 404 so the
/// client can render the same "not indexed yet" hint as the direct path.
async fn get_internal_get(
    State(state): State<Arc<AppState>>,
    Query(q): Query<InternalGetQuery>,
) -> Result<Response, ApiError> {
    // A vault must be bound even though `Engine::get` keys off vault-relative
    // paths only — keeps the internal surface uniformly 503 with no vault.
    let _ = require_vault_root(&state)?;
    let engine = require_engine(&state)?.clone();
    let doc_path = q.path.clone();

    // `Engine::get` is a synchronous redb read — run it off the async runtime
    // under the blocking mutex, same as status/reindex.
    let content = tokio::task::spawn_blocking(move || {
        let engine = engine.lock().unwrap_or_else(|p| p.into_inner());
        engine.get(&doc_path)
    })
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "internal get task panicked");
        ApiError::Internal("get failed".to_string())
    })?;

    match content {
        Ok(content) => Ok(Json(InternalGetResponse {
            doc_path: q.path,
            content,
        })
        .into_response()),
        // A miss is a client-facing 404 (not a 500) — `Engine::get` returns a
        // "doc not found" error for an unindexed path; the CLI maps this back to
        // its "paths are vault-relative / reindex may not have run" hint.
        Err(_) => Err(ApiError::NotFound(format!("doc not indexed: {}", q.path))),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/internal/reindex
// ─────────────────────────────────────────────────────────────────────────

/// Reindex request. `mode` selects which engine reindex runs; `paths` is
/// required (and non-empty) for `mode: "paths"`, ignored otherwise.
#[derive(Debug, Deserialize)]
struct ReindexRequest {
    mode: ReindexMode,
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ReindexMode {
    /// Embed exactly the pending (drifted) docs — mirrors `--pending-only`.
    Pending,
    /// Reindex the caller-supplied vault-relative doc paths.
    Paths,
    /// Keyword-only (tantivy) reindex over the whole vault, NO embedding —
    /// mirrors `--lex-only`. Cheap: it never loads the model, so the PostToolUse
    /// auto-reindex hook can route here in-session without a per-write embed.
    Lex,
    /// Full reindex + embed of the whole vault — mirrors a bare `search
    /// reindex`. Lets a manual full reindex run WHILE the daemon holds the
    /// engine (it would otherwise hit the redb lock). Embeds, so it can load the
    /// model; only reached from an explicit CLI reindex, never a hook.
    All,
}

/// Reindex result — the batch stats plus the post-reindex doc count, so a
/// client sees the effect in one round trip without a follow-up status call.
#[derive(Debug, Serialize, PartialEq)]
struct ReindexResponse {
    added: usize,
    updated: usize,
    removed: usize,
    unchanged: usize,
    failed: usize,
    /// Docs indexed after this reindex (so a caller can confirm `doc_count`
    /// rose without a second request).
    doc_count: usize,
    /// The active embedding model, so the CLI can name it in the reindex summary
    /// exactly as the direct path does (mirrors `search reindex`'s
    /// `embed_model`). Read from vault config, defaulting when the key is absent.
    embed_model: String,
}

async fn post_internal_reindex(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ReindexRequest>,
) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();
    let engine = require_engine(&state)?.clone();

    // For `mode=paths`, confine EVERY caller-supplied entry to the vault BEFORE
    // it can reach `engine.reindex_paths` → `vault_root.join(doc_path)` →
    // `std::fs::read`. Without this an entry like `../../../../etc/passwd` or an
    // absolute path would be read + indexed (path-traversal). `pending` mode
    // derives its worklist from a vault walk, so it needs no confinement.
    let confined_paths: Vec<String> = if req.mode == ReindexMode::Paths {
        if req.paths.is_empty() {
            return Err(ApiError::BadRequest(
                "mode=paths requires a non-empty paths list".to_string(),
            ));
        }
        req.paths
            .iter()
            .map(|p| super::api::confine_reindex_path(&root, p))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };

    // Reindex is synchronous (tantivy + redb + embedding). Run it off the async
    // runtime, holding the engine mutex for the batch's duration so a concurrent
    // search serialises behind it rather than racing the redb writer.
    let root_for_task = root.clone();
    let result = tokio::task::spawn_blocking(move || {
        let root = root_for_task;
        let mut engine = engine.lock().unwrap_or_else(|p| p.into_inner());
        let stats = match req.mode {
            ReindexMode::Pending => {
                let pending = engine.pending_vector_paths(&root)?;
                if pending.is_empty() {
                    onebrain_search::engine::ReindexStats::default()
                } else {
                    engine.reindex_paths(&root, &pending)?
                }
            }
            ReindexMode::Paths => engine.reindex_paths(&root, &confined_paths)?,
            // Keyword-only whole-vault pass — no embed, no model load.
            ReindexMode::Lex => engine.reindex_all_lex_only_with_progress(&root, &mut |_| {})?,
            // Full whole-vault reindex + embed.
            ReindexMode::All => engine.reindex_all(&root)?,
        };
        let doc_count = engine.status(&root)?.doc_count;
        Ok::<_, anyhow::Error>((stats, doc_count))
    })
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "internal reindex task panicked");
        ApiError::Internal("reindex failed".to_string())
    })?
    .map_err(|e| {
        tracing::warn!(error = %e, "engine reindex failed");
        ApiError::Internal("reindex failed".to_string())
    })?;

    let (stats, doc_count) = result;
    // Name the active model so the CLI hook path renders the same summary the
    // direct path does. Read-only; defaults when the vault config lacks the key.
    let embed_model = onebrain_core::load_vault_config_at(&root)
        .map(|c| c.search.embed_model)
        .unwrap_or_default();
    let resp = ReindexResponse {
        added: stats.added,
        updated: stats.updated,
        removed: stats.removed,
        unchanged: stats.unchanged,
        failed: stats.failed,
        doc_count,
        embed_model,
    };
    Ok(Json(resp).into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/internal/rerank
// ─────────────────────────────────────────────────────────────────────────

/// Doc-level rerank request: a query plus a caller-supplied candidate path
/// set (e.g. from a prior path-scoped retrieval step), and how many of the
/// best-scoring paths to keep.
#[derive(Debug, Deserialize)]
struct RerankReq {
    query: String,
    paths: Vec<String>,
    top_k: usize,
}

/// One reranked hit, in the SAME wire shape as `server::search::SearchHit`
/// (the webui/MCP search response) so a client can reuse the exact same
/// parser for both endpoints.
#[derive(Debug, Serialize, PartialEq)]
struct RerankHit {
    /// Vault-relative, slash-separated doc path.
    path: String,
    /// Relevance score (RRF-fused; unused input here since `rerank_paths`
    /// seeds every candidate chunk at 0.0 and lets the cross-encoder rank —
    /// kept for wire-shape parity with `SearchHit`).
    score: f64,
    /// Note title — the heading path if present, else the file stem.
    title: String,
    /// Short one-line excerpt of the best-scoring chunk for this doc.
    snippet: String,
    /// Calibrated 0-1 Tier-2 cross-encoder relevance. `None` when the rerank
    /// stage was skipped (disabled, model not downloaded, or a rerank
    /// failure for the whole query) — mirrors `Hit::rerank_score`.
    #[serde(skip_serializing_if = "Option::is_none")]
    rerank_score: Option<f32>,
}

/// Response body: the reranked, doc-deduped hit list.
#[derive(Debug, Serialize, PartialEq)]
struct RerankResponse {
    hits: Vec<RerankHit>,
}

/// `POST /api/internal/rerank` — the held engine's `Engine::rerank_paths`: a
/// doc-level Tier-2 rerank over a caller-supplied candidate path set (e.g. the
/// MCP `query` tool's fused lex+vec result paths). Like `reindex`/`get`, this
/// REQUIRES the single-owner held engine — no per-request fallback — because
/// its callers route here precisely to avoid a second redb opener.
///
/// No path confinement is needed here (unlike `reindex {mode:"paths"}`):
/// `rerank_paths` never touches the filesystem — it looks up already-indexed
/// chunk ids for each `doc_path` in redb (a plain key miss for an unknown/
/// hostile path just yields zero chunks, never a read), so there is no
/// traversal surface to guard.
async fn post_internal_rerank(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RerankReq>,
) -> Result<Response, ApiError> {
    let engine = require_engine(&state)?.clone();

    // Rerank is synchronous (redb reads + the Tier-2 cross-encoder). Run it
    // off the async runtime, holding the engine mutex for its duration, same
    // as reindex/status.
    let hits = tokio::task::spawn_blocking(move || {
        let engine = engine.lock().unwrap_or_else(|p| p.into_inner());
        engine.rerank_paths(&req.query, req.paths, req.top_k)
    })
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "internal rerank task panicked");
        ApiError::Internal("rerank failed".to_string())
    })?
    .map_err(|e| {
        tracing::warn!(error = %e, "engine rerank failed");
        ApiError::Internal("rerank failed".to_string())
    })?;

    let resp = RerankResponse {
        hits: hits
            .into_iter()
            .map(|h| RerankHit {
                title: if h.heading_path.is_empty() {
                    title_from_doc_path(&h.doc_path)
                } else {
                    h.heading_path.clone()
                },
                path: h.doc_path,
                score: h.score,
                snippet: h.snippet,
                rerank_score: h.rerank_score,
            })
            .collect(),
    };
    Ok(Json(resp).into_response())
}

/// File stem of a slash-separated vault path (`a/b/note.md` → `note`), used as
/// a fallback title when there is no heading path. Mirrors
/// `server::search::title_from_path` (kept as a separate copy since that one
/// is private to its module).
fn title_from_doc_path(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".md").unwrap_or(file).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{build_router, ServeConfig};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TOKEN: &str = "internal-test-token-1234567890";

    /// A vault dir with a config but a genuinely empty cache dir. The held
    /// engine still opens (an empty index is valid), so `/api/internal/status`
    /// returns a real zero-count status rather than 503.
    fn vault_with_empty_index() -> (tempfile::TempDir, tempfile::TempDir) {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: internal-status-test\n",
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        (vault, cache)
    }

    fn router_holding_engine(vault: &Path) -> axum::Router {
        let mut cfg = ServeConfig::localhost(Some(vault.to_path_buf()), 0, TOKEN.to_string(), None);
        cfg.hold_engine = true;
        build_router(cfg)
    }

    #[test]
    fn open_held_engine_none_on_non_vault_dir() {
        // A dir with no onebrain.yml isn't a vault → open_held_engine returns
        // None (the daemon then falls back to per-request open), exercising the
        // error branch + its warn log.
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        assert!(open_held_engine(dir.path()).is_none());
    }

    #[test]
    fn open_held_engine_some_on_valid_vault() {
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        assert!(open_held_engine(vault.path()).is_some());
    }

    /// Brief 7.2: the warm-load thread must exit silently (no panic, no lock
    /// contention) when the configured reranker model is NOT downloaded — the
    /// common case for a fresh vault. Joins the spawned thread directly so a
    /// regression that blocks on the engine mutex (e.g. a bug that tries to
    /// download inline) fails the test instead of hanging the suite.
    #[test]
    fn warm_thread_exits_silently_when_model_not_downloaded() {
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let engine = open_held_engine(vault.path()).expect("engine opens");

        // Directly exercise the spawn helper (not just relying on
        // `open_held_engine`'s internal call) so this test pins the "model
        // absent -> exits without touching the lock" contract even if the
        // call site inside `open_held_engine` changes.
        let cache_dir = collection_cache_dir(&collection_name_readonly(vault.path()).unwrap());
        let handle = spawn_reranker_warm_thread(
            engine.clone(),
            "onebrain-reranker-v1".to_string(),
            cache_dir,
        );
        handle.join().expect("warm thread must not panic");

        // The engine must still be lockable afterwards (no poisoning, no
        // held lock) — a sanity check that the warm thread released
        // everything cleanly.
        let guard = engine.lock().unwrap();
        drop(guard);
    }

    #[test]
    fn status_response_serializes_expected_shape() {
        let v = serde_json::to_value(InternalStatusResponse {
            doc_count: 3,
            pending_new: 1,
            pending_changed: 0,
            pending_removed: 2,
            pending_total: 3,
            last_indexed: Some(42),
            indexed: true,
            reranker_model: "onebrain-reranker-v1".to_string(),
            reranker_ready: true,
            reranker_downloaded: true,
            reranker_disk_bytes: Some(1024),
        })
        .unwrap();
        assert_eq!(v["doc_count"], 3);
        assert_eq!(v["pending_total"], 3);
        assert_eq!(v["last_indexed"], 42);
        assert_eq!(v["indexed"], true);
        assert_eq!(v["reranker_model"], "onebrain-reranker-v1");
        assert_eq!(v["reranker_ready"], true);
        assert_eq!(v["reranker_downloaded"], true);
        assert_eq!(v["reranker_disk_bytes"], 1024);
    }

    #[test]
    fn status_response_serializes_reranker_disk_bytes_none_as_null() {
        // `reranker_disk_bytes: None` (model not on disk) must serialize as
        // JSON `null`, not be omitted — the daemon status shape has no
        // `skip_serializing_if` on this field (unlike the CLI's
        // `SearchStatusData`), so a client can distinguish "checked, absent"
        // from a missing key.
        let v = serde_json::to_value(InternalStatusResponse {
            doc_count: 0,
            pending_new: 0,
            pending_changed: 0,
            pending_removed: 0,
            pending_total: 0,
            last_indexed: None,
            indexed: false,
            reranker_model: "onebrain-reranker-v1".to_string(),
            reranker_ready: false,
            reranker_downloaded: false,
            reranker_disk_bytes: None,
        })
        .unwrap();
        assert!(v.get("reranker_disk_bytes").is_some(), "{v}");
        assert!(v["reranker_disk_bytes"].is_null(), "{v}");
    }

    #[tokio::test]
    async fn reindex_503_when_no_engine_held() {
        // Like the status variant, the reindex route also 503s (never opens an
        // engine per-request) when the process holds none.
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: reindex-no-engine\n",
        )
        .unwrap();
        let cfg =
            ServeConfig::localhost(Some(vault.path().to_path_buf()), 0, TOKEN.to_string(), None);
        let router = build_router(cfg);

        let resp = router
            .oneshot(
                Request::post("/api/internal/reindex")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"pending"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn reindex_rejects_malformed_json() {
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/internal/reindex")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"bogus"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // An unknown enum variant fails deserialization → 4xx (axum's Json
        // rejection), never a 5xx.
        assert!(resp.status().is_client_error(), "got {}", resp.status());
    }

    // ── SECURITY: path-traversal in `reindex {mode:"paths"}` ───────────────
    // Every caller-supplied path must be confined to the vault BEFORE it reaches
    // the engine's `vault_root.join(doc_path)` + `std::fs::read`. Each hostile
    // form must be rejected with 400, and the target file must NOT be indexed.
    #[tokio::test]
    async fn reindex_paths_rejects_traversal_absolute_and_symlink_escape() {
        use onebrain_search::chunk::Chunk;
        use onebrain_search::lex::LexIndex;
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());

        // Seed a WORKING index with a LEGIT in-vault doc, so the "secret absent"
        // assertion below is proven against a real, searchable index — not a
        // vacuously-empty one (where every query returns 0 regardless).
        {
            let tantivy = collection_cache_dir(&collection_name_readonly(vault.path()).unwrap())
                .join("tantivy");
            let mut lex = LexIndex::open(&tantivy).unwrap();
            lex.add(&Chunk {
                chunk_id: "legit.md#0".to_string(),
                doc_path: "legit.md".to_string(),
                heading_path: String::new(),
                text: "legitimate PUBLIC vault marker".to_string(),
                chunk_index: 0,
            })
            .unwrap();
            lex.commit().unwrap();
        }

        // A secret OUTSIDE the vault that a traversal would try to read + index.
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.md");
        std::fs::write(&secret, "TOPSECRET vault-escape marker\n").unwrap();

        // A symlink INSIDE the vault pointing at the outside secret (symlink
        // escape — the lexical `..` check can't see through it).
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, vault.path().join("link.md")).unwrap();

        let router = router_holding_engine(vault.path());

        // Sanity: the index WORKS — searching the legit marker returns the doc.
        let resp = router
            .clone()
            .oneshot(
                Request::get("/api/vault/search?q=legitimate&mode=lex")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["hits"][0]["path"], "legit.md",
            "the seeded index must be searchable (else the secret-absent check is vacuous): {v}"
        );

        // Each hostile entry, tried on its own → must be 400 (rejected).
        let mut hostile: Vec<String> = vec![
            "../../../../etc/passwd".to_string(),
            secret.display().to_string(), // absolute path
            "sub/../../escape.md".to_string(),
        ];
        #[cfg(unix)]
        hostile.push("link.md".to_string()); // symlink escape

        for path in &hostile {
            let body = serde_json::json!({ "mode": "paths", "paths": [path] }).to_string();
            let resp = router
                .clone()
                .oneshot(
                    Request::post("/api/internal/reindex")
                        .header("x-onebrain-token", TOKEN)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "hostile path must be rejected: {path}"
            );
        }

        // Now — against a KNOWN-WORKING index — the escaped secret is absent: a
        // search for its unique marker returns nothing, proving confinement
        // stopped it from being read + indexed (not that the index was empty).
        let resp = router
            .oneshot(
                Request::get("/api/vault/search?q=TOPSECRET&mode=lex")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["hits"].as_array().map(|a| a.len()),
            Some(0),
            "escaped secret must NOT be indexed even against a working index: {v}"
        );
    }

    /// End-to-end `reindex {mode:"paths"}` with a REAL on-disk doc: it must
    /// index the doc and return NON-ZERO `added` + `doc_count`, with each stat
    /// mapped to the right response field. Lex-only build — in a lex-only build
    /// `index_doc` records the doc's lex chunks + `DOC_HASHES` hash WITHOUT
    /// embedding (`embed_passages_if_available` returns `None`), so `added`/
    /// `doc_count` genuinely rise with NO model download. This is what catches a
    /// field-swap like `removed: stats.updated` in `ReindexResponse`; the other
    /// reindex tests only assert the all-zero no-op.
    #[cfg(not(feature = "semantic"))]
    #[tokio::test]
    async fn reindex_paths_adds_a_new_doc_and_maps_fields() {
        let (vault, cache) = vault_with_empty_index();
        std::fs::write(vault.path().join("note.md"), "hello indexed world\n").unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        // First reindex: the doc is NEW → added=1, updated=0, removed=0,
        // unchanged=0, doc_count=1.
        let resp = router
            .clone()
            .oneshot(
                Request::post("/api/internal/reindex")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"paths","paths":["note.md"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "reindex should succeed");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["added"], 1, "new doc → added=1: {v}");
        assert_eq!(v["updated"], 0, "field mapping: updated must be 0: {v}");
        assert_eq!(v["removed"], 0, "field mapping: removed must be 0: {v}");
        assert_eq!(v["unchanged"], 0, "field mapping: unchanged must be 0: {v}");
        assert_eq!(v["failed"], 0, "no failures: {v}");
        assert_eq!(v["doc_count"], 1, "doc_count rose to 1: {v}");

        // Second reindex of the SAME unchanged file: added=0, unchanged=1 — a
        // different field lights up, so a swap of added↔unchanged would be caught.
        let resp = router
            .oneshot(
                Request::post("/api/internal/reindex")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"paths","paths":["note.md"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["added"], 0, "unchanged doc → added=0: {v}");
        assert_eq!(v["unchanged"], 1, "unchanged doc → unchanged=1: {v}");
        assert_eq!(v["doc_count"], 1, "still exactly 1 doc: {v}");
    }

    /// `mode:"lex"` (Track 2c) — a keyword-only whole-vault reindex. In a
    /// lex-only build it indexes the on-disk doc WITHOUT embedding, so `added`
    /// and `doc_count` rise with no model download; the response also names the
    /// active `embed_model` so the CLI hook path can render the same summary.
    #[cfg(not(feature = "semantic"))]
    #[tokio::test]
    async fn reindex_lex_mode_indexes_whole_vault() {
        let (vault, cache) = vault_with_empty_index();
        std::fs::write(vault.path().join("note.md"), "warm daemon lex pass\n").unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/internal/reindex")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"lex"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "lex reindex should succeed");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["added"], 1, "new doc lex-indexed → added=1: {v}");
        // `doc_count` (DOC_HASHES keys) reflects EMBEDDED docs; a keyword-only
        // lex pass records lex chunks but no content hash, so it stays 0 here.
        // The point of `lex` mode is exactly this: index for keyword search
        // WITHOUT the embed that would bump doc_count.
        assert_eq!(
            v["doc_count"], 0,
            "lex-only records no hash → doc_count 0: {v}"
        );
        // The active model is named so the CLI hook renders the same summary.
        assert_eq!(v["embed_model"], "multilingual-e5-small", "{v}");
    }

    /// `mode:"all"` (Track 2c) — a full whole-vault reindex. Lex-only build →
    /// no embed, so `added`/`doc_count` rise download-free, same as `lex`/
    /// `paths`. (Under `semantic` this would embed → model download, which the
    /// no-download test rule forbids, so it's asserted only in the lex-only tier.)
    #[cfg(not(feature = "semantic"))]
    #[tokio::test]
    async fn reindex_all_mode_indexes_whole_vault() {
        let (vault, cache) = vault_with_empty_index();
        std::fs::write(vault.path().join("note.md"), "full daemon reindex\n").unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/internal/reindex")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"all"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "full reindex should succeed");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["added"], 1, "new doc → added=1: {v}");
        assert_eq!(v["doc_count"], 1, "doc_count rose to 1: {v}");
    }

    /// `GET /api/internal/get` (Track 2c) — returns a doc's indexed text after
    /// it's indexed, and 404s for an unindexed path (so the CLI can render its
    /// "not indexed yet" hint). Lex-only build: indexing is download-free.
    #[cfg(not(feature = "semantic"))]
    #[tokio::test]
    async fn internal_get_returns_indexed_text_and_404s_when_missing() {
        let (vault, cache) = vault_with_empty_index();
        std::fs::write(vault.path().join("note.md"), "indexed body text\n").unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        // Not indexed yet → 404.
        let resp = router
            .clone()
            .oneshot(
                Request::get("/api/internal/get?path=note.md")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "unindexed → 404");

        // Index it, then GET returns its text.
        router
            .clone()
            .oneshot(
                Request::post("/api/internal/reindex")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"paths","paths":["note.md"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = router
            .oneshot(
                Request::get("/api/internal/get?path=note.md")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "indexed doc → 200");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["doc_path"], "note.md", "{v}");
        assert!(
            v["content"].as_str().unwrap().contains("indexed body text"),
            "get returns the indexed text: {v}"
        );
    }

    #[tokio::test]
    async fn internal_get_503_when_no_engine_held() {
        // Like status/reindex, get 503s (never per-request opens) with no engine.
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: get-no-engine\n",
        )
        .unwrap();
        let cfg =
            ServeConfig::localhost(Some(vault.path().to_path_buf()), 0, TOKEN.to_string(), None);
        let router = build_router(cfg);
        let resp = router
            .oneshot(
                Request::get("/api/internal/get?path=x.md")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Direct unit test of the stats→response field mapping, independent of the
    /// engine — a belt-and-braces guard against a field swap in
    /// `ReindexResponse` construction that runs in BOTH build tiers (the
    /// end-to-end test above is lex-only). Uses distinct values per field so any
    /// transposition is caught.
    #[test]
    fn reindex_response_maps_each_stat_field() {
        let stats = onebrain_search::engine::ReindexStats {
            added: 1,
            updated: 2,
            removed: 3,
            unchanged: 4,
            failed: 5,
        };
        let resp = ReindexResponse {
            added: stats.added,
            updated: stats.updated,
            removed: stats.removed,
            unchanged: stats.unchanged,
            failed: stats.failed,
            doc_count: 6,
            embed_model: "multilingual-e5-small".to_string(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["added"], 1);
        assert_eq!(v["updated"], 2);
        assert_eq!(v["removed"], 3);
        assert_eq!(v["unchanged"], 4);
        assert_eq!(v["failed"], 5);
        assert_eq!(v["doc_count"], 6);
        assert_eq!(v["embed_model"], "multilingual-e5-small");
    }

    #[tokio::test]
    async fn health_is_200_even_with_no_engine_held() {
        // The engine-independent liveness route: 200 whether or not an engine is
        // held. This is what the client's is_live() probes so a live-but-
        // engine-less daemon isn't mistaken for dead.
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: health-no-engine\n",
        )
        .unwrap();
        // hold_engine=false → search_engine is None, yet /api/health must be 200.
        let cfg =
            ServeConfig::localhost(Some(vault.path().to_path_buf()), 0, TOKEN.to_string(), None);
        let router = build_router(cfg);

        let resp = router
            .oneshot(
                Request::get("/api/health")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["engine_held"], false);
    }

    #[tokio::test]
    async fn vault_search_via_held_engine_empty_query_and_hits() {
        // Exercises `get_vault_search` through the held engine: an empty query
        // short-circuits to empty hits, and a real lex query returns the seeded
        // doc. Covers the async handler + the held-engine `run_search` lex path.
        use onebrain_search::chunk::Chunk;
        use onebrain_search::lex::LexIndex;
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        {
            let tantivy = collection_cache_dir(&collection_name_readonly(vault.path()).unwrap())
                .join("tantivy");
            let mut lex = LexIndex::open(&tantivy).unwrap();
            lex.add(&Chunk {
                chunk_id: "beta.md#0".to_string(),
                doc_path: "beta.md".to_string(),
                heading_path: String::new(),
                text: "lazy dog sleeps".to_string(),
                chunk_index: 0,
            })
            .unwrap();
            lex.commit().unwrap();
        }
        let router = router_holding_engine(vault.path());

        // Empty query → empty hits (200), no engine work.
        let resp = router
            .clone()
            .oneshot(
                Request::get("/api/vault/search?q=&mode=lex")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["hits"].as_array().unwrap().len(), 0);

        // Real query → the seeded doc, through the held engine.
        let resp = router
            .oneshot(
                Request::get("/api/vault/search?q=lazy&mode=lex")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["hits"][0]["path"], "beta.md");
    }

    #[tokio::test]
    async fn vault_search_without_held_engine_uses_per_request_path() {
        // hold_engine=false → search goes through the per-request `run_native`
        // path (the `serve`/CLI path), covering it via the async handler.
        use onebrain_search::chunk::Chunk;
        use onebrain_search::lex::LexIndex;
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        {
            let tantivy = collection_cache_dir(&collection_name_readonly(vault.path()).unwrap())
                .join("tantivy");
            let mut lex = LexIndex::open(&tantivy).unwrap();
            lex.add(&Chunk {
                chunk_id: "gamma.md#0".to_string(),
                doc_path: "gamma.md".to_string(),
                heading_path: String::new(),
                text: "per request path".to_string(),
                chunk_index: 0,
            })
            .unwrap();
            lex.commit().unwrap();
        }
        // No hold_engine → search_engine is None → run_native.
        let cfg =
            ServeConfig::localhost(Some(vault.path().to_path_buf()), 0, TOKEN.to_string(), None);
        let router = build_router(cfg);
        let resp = router
            .oneshot(
                Request::get("/api/vault/search?q=request&mode=lex")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["hits"][0]["path"], "gamma.md");
    }

    #[test]
    fn app_state_debug_elides_token_and_shows_engine_flag() {
        // Covers AppState's hand-written Debug impl (mod.rs).
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let mut cfg =
            ServeConfig::localhost(Some(vault.path().to_path_buf()), 0, TOKEN.to_string(), None);
        cfg.hold_engine = true;
        let (_router, state) = crate::server::build_router_with_state(cfg);
        let s = format!("{state:?}");
        assert!(s.contains("AppState"), "{s}");
        assert!(s.contains("search_engine_held"), "{s}");
        assert!(!s.contains(TOKEN), "Debug must not print the token: {s}");
    }

    #[tokio::test]
    async fn health_requires_token() {
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());
        let resp = router
            .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn status_reports_zero_on_empty_index() {
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::get("/api/internal/status")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["doc_count"], 0);
        assert_eq!(v["indexed"], false);
        assert_eq!(v["pending_total"], 0);
        // Reranker fields: default config (no `search.reranker` block) names
        // the default model, but it isn't downloaded in a fresh test cache dir
        // → not ready, no download size.
        assert_eq!(v["reranker_model"], "onebrain-reranker-v1");
        assert_eq!(v["reranker_ready"], false);
        assert_eq!(v["reranker_downloaded"], false);
        assert!(v["reranker_disk_bytes"].is_null());
    }

    /// Brief 7.1: `open_held_engine` must apply the vault's `RerankSettings` —
    /// asserted here via the LIVE `/api/internal/status` `reranker_ready`
    /// field with `search.reranker.enabled: false` configured. Even though the
    /// registry's default model isn't downloaded in this fresh test cache
    /// (so `reranker_ready` would be `false` either way), the disabled-vs-
    /// default-model-name distinction proves the config's `reranker.model`
    /// actually reached the held engine's settings — a config the daemon
    /// never read would report the hard-coded default name instead.
    #[tokio::test]
    async fn internal_status_reflects_configured_reranker_model_and_disabled_flag() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: internal-status-reranker-cfg\n  reranker:\n    enabled: false\n    model: onebrain-reranker-v1\n",
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::get("/api/internal/status")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["reranker_model"], "onebrain-reranker-v1");
        // Disabled in config → never ready, regardless of download status.
        assert_eq!(v["reranker_ready"], false);
    }

    #[tokio::test]
    async fn internal_routes_401_without_token() {
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::get("/api/internal/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn internal_status_503_when_no_engine_held() {
        // A router built WITHOUT hold_engine (the `serve` / test default) holds
        // no engine, so the internal routes must 503 rather than open one.
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: no-engine-test\n",
        )
        .unwrap();
        let cfg =
            ServeConfig::localhost(Some(vault.path().to_path_buf()), 0, TOKEN.to_string(), None);
        let router = build_router(cfg);

        let resp = router
            .oneshot(
                Request::get("/api/internal/status")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn reindex_paths_requires_non_empty_paths() {
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/internal/reindex")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"paths","paths":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn reindex_pending_with_no_drift_is_a_clean_noop() {
        // An empty-index vault with no docs has nothing pending, so
        // `reindex {pending}` returns default (all-zero) stats WITHOUT ever
        // constructing the embedder — the download-free path that proves the
        // route + held engine are wired end to end in BOTH build tiers.
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/internal/reindex")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"pending"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "reindex should succeed");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["added"], 0);
        assert_eq!(v["failed"], 0);
        assert_eq!(v["doc_count"], 0);
    }

    /// Two clients hit the ONE held engine concurrently — a lex search and a
    /// status call — and both succeed with no redb "Database already open"
    /// error. This is the core warm-daemon invariant at the unit level (the
    /// integration test repeats it over a real socket). Download-free: lex
    /// search + status never construct an embedder.
    #[tokio::test]
    async fn two_clients_share_the_held_engine_concurrently() {
        let (vault, cache) = vault_with_empty_index();
        // Give the lex index one real doc so search has something to open.
        {
            use onebrain_search::chunk::Chunk;
            use onebrain_search::lex::LexIndex;
            let collection = collection_name_readonly(vault.path()).unwrap();
            let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
            let tantivy = collection_cache_dir(&collection).join("tantivy");
            let mut lex = LexIndex::open(&tantivy).unwrap();
            lex.add(&Chunk {
                chunk_id: "alpha.md#0".to_string(),
                doc_path: "alpha.md".to_string(),
                heading_path: String::new(),
                text: "the quick brown fox".to_string(),
                chunk_index: 0,
            })
            .unwrap();
            lex.commit().unwrap();
        }
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let search = router.clone().oneshot(
            Request::get("/api/vault/search?q=quick&mode=lex")
                .header("x-onebrain-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        );
        let status = router.oneshot(
            Request::get("/api/internal/status")
                .header("x-onebrain-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        );
        let (search_resp, status_resp) = tokio::join!(search, status);
        assert_eq!(search_resp.unwrap().status(), StatusCode::OK);
        assert_eq!(status_resp.unwrap().status(), StatusCode::OK);
    }

    // ── POST /api/internal/rerank ───────────────────────────────────────────

    #[tokio::test]
    async fn rerank_503_when_no_engine_held() {
        // Like reindex/get, rerank also 503s (never opens an engine
        // per-request) when the process holds none.
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: rerank-no-engine\n",
        )
        .unwrap();
        let cfg =
            ServeConfig::localhost(Some(vault.path().to_path_buf()), 0, TOKEN.to_string(), None);
        let router = build_router(cfg);

        let resp = router
            .oneshot(
                Request::post("/api/internal/rerank")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"query":"hello","paths":["note.md"],"top_k":5}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn rerank_rejects_malformed_json() {
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/internal/rerank")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query": "hello""#)) // truncated / malformed
                    .unwrap(),
            )
            .await
            .unwrap();
        // Malformed JSON fails axum's `Json` extractor → 4xx, never a 5xx.
        assert!(resp.status().is_client_error(), "got {}", resp.status());
    }

    #[tokio::test]
    async fn rerank_missing_required_field_is_bad_request() {
        // `top_k` is required (no `#[serde(default)]`) — an omitted field is
        // also a Json-extractor rejection, same 4xx family as malformed JSON.
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/internal/rerank")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"hello","paths":["note.md"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(resp.status().is_client_error(), "got {}", resp.status());
    }

    /// End-to-end doc-level rerank against a held engine: index a real doc via
    /// `/internal/reindex`, then rerank it via `/internal/rerank` and assert
    /// the mapped fields (`path`, `title`, `snippet`) — mirroring
    /// `reindex_paths_adds_a_new_doc_and_maps_fields`'s harness. Lex-only
    /// build: no reranker model is downloaded, so `rerank_score` stays `None`
    /// (the skip-not-fail path in `apply_rerank`/`Engine::reranker`) — this
    /// still proves the route + held engine + field mapping are wired
    /// end-to-end without requiring a model download.
    #[cfg(not(feature = "semantic"))]
    #[tokio::test]
    async fn rerank_paths_reranks_indexed_doc_and_maps_fields() {
        let (vault, cache) = vault_with_empty_index();
        std::fs::write(vault.path().join("note.md"), "hello indexed world\n").unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        // Index the doc first — rerank_paths looks up already-indexed chunk
        // meta in redb, so it needs a prior reindex to have anything to rank.
        let resp = router
            .clone()
            .oneshot(
                Request::post("/api/internal/reindex")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"mode":"paths","paths":["note.md"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "seed reindex should succeed");

        let resp = router
            .oneshot(
                Request::post("/api/internal/rerank")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"query":"hello","paths":["note.md"],"top_k":5}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "rerank should succeed");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["hits"].as_array().map(|a| a.len()), Some(1), "{v}");
        assert_eq!(v["hits"][0]["path"], "note.md", "{v}");
        assert_eq!(v["hits"][0]["title"], "note", "{v}");
        assert!(
            v["hits"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("hello indexed world"),
            "{v}"
        );
        // No reranker model downloaded in this fresh test cache → the field is
        // omitted (skip_serializing_if), never a stray null.
        assert!(v["hits"][0].get("rerank_score").is_none(), "{v}");
    }

    /// An unknown candidate path (never indexed) yields zero chunks in redb —
    /// `rerank_paths` returns an empty hit list rather than erroring, since a
    /// path miss is a plain, expected redb-table miss (see
    /// `Engine::doc_chunk_ids`), not a traversal or malformed-input case.
    #[tokio::test]
    async fn rerank_unknown_path_returns_empty_hits() {
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/internal/rerank")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"query":"hello","paths":["never-indexed.md"],"top_k":5}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["hits"].as_array().map(|a| a.len()), Some(0), "{v}");
    }

    #[tokio::test]
    async fn rerank_requires_token() {
        let (vault, cache) = vault_with_empty_index();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_engine(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/internal/rerank")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"hello","paths":[],"top_k":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
