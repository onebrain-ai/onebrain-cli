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

use std::path::Path;
use std::sync::Arc;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use super::api::{require_vault_root, ApiError};
use super::{AppState, SharedEngine};
use crate::commands::search_common::{collection_cache_dir, collection_name_readonly};
use onebrain_search::engine::Engine;

/// Build the `/api/internal` sub-router. State + auth are attached by
/// [`super::build_router`] for the whole tree, so this stays a pure route table.
pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/internal/status", get(get_internal_status))
        .route("/internal/reindex", post(post_internal_reindex))
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
        Ok(engine) => {
            tracing::info!(
                vault = %vault_root.display(),
                "daemon holding search engine for process lifetime"
            );
            Some(Arc::new(std::sync::Mutex::new(engine)))
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

/// Fallible core of [`open_held_engine`]: resolve the collection (read-only),
/// open the engine at its cache dir, and apply the vault's exclude patterns —
/// exactly what `search_common::open_engine` does, minus the config-persisting
/// collection resolver (the daemon must never write config).
fn try_open_held_engine(vault_root: &Path) -> anyhow::Result<Engine> {
    let config = onebrain_core::load_vault_config_at(vault_root)?;
    let collection = collection_name_readonly(vault_root)?;
    let cache_dir = collection_cache_dir(&collection);
    let mut engine = Engine::open(&cache_dir, &config.search.embed_model)?;
    engine.set_exclude_patterns(config.search.exclude.clone());
    Ok(engine)
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
}

async fn get_internal_status(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();
    let engine = require_engine(&state)?.clone();

    // Engine status is a synchronous hash-walk (no async, no embedder) — run it
    // off the async runtime under the blocking mutex.
    let status = tokio::task::spawn_blocking(move || {
        let engine = engine.lock().unwrap_or_else(|p| p.into_inner());
        engine.status(&root)
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
    };
    Ok(Json(resp).into_response())
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
}

async fn post_internal_reindex(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ReindexRequest>,
) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();
    let engine = require_engine(&state)?.clone();

    if req.mode == ReindexMode::Paths && req.paths.is_empty() {
        return Err(ApiError::BadRequest(
            "mode=paths requires a non-empty paths list".to_string(),
        ));
    }

    // Reindex is synchronous (tantivy + redb + embedding). Run it off the async
    // runtime, holding the engine mutex for the batch's duration so a concurrent
    // search serialises behind it rather than racing the redb writer.
    let result = tokio::task::spawn_blocking(move || {
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
            ReindexMode::Paths => engine.reindex_paths(&root, &req.paths)?,
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
    let resp = ReindexResponse {
        added: stats.added,
        updated: stats.updated,
        removed: stats.removed,
        unchanged: stats.unchanged,
        failed: stats.failed,
        doc_count,
    };
    Ok(Json(resp).into_response())
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
        })
        .unwrap();
        assert_eq!(v["doc_count"], 3);
        assert_eq!(v["pending_total"], 3);
        assert_eq!(v["last_indexed"], 42);
        assert_eq!(v["indexed"], true);
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
}
