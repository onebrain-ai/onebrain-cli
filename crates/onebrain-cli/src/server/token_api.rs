//! `/api/token/*` — the warm daemon's token-optimization endpoints (design
//! §5/§5b). The daemon is the sole owner of `token.redb` (memoization +
//! already-sent ledger, Track 3); these routes serve the ledger and gain
//! surfaces over HTTP so MCP + CLI clients (and the read-hook, Track 5/8) stay
//! lock-free HTTP clients of the one owner.
//!
//! ```text
//!   GET  /api/token/gain?by=&since=&history=  → gain pivot JSON (Track 2 rollups)
//!   GET  /api/token/status                    → { level, ledger_active, cache_bytes }
//!   POST /api/token/ledger/check              → { path, session_token, record? }
//!                                               → ledger verdict (+ reference envelope)
//! ```
//!
//! Every route is behind the same token-auth middleware as the rest of the
//! surface (applied by [`super::build_router`]). They require the daemon to
//! actually hold the token cache — a `serve` / unit-test router with none gets
//! 503, never a per-request open (the daemon is the single redb owner).
//!
//! ## Coordination note (Track 2)
//! `GET /api/token/gain` is a STUB in this track: it returns an empty pivot
//! shape until Track 2 lands the `PivotResult` type + `token.redb` rollups.
//! When Track 2 merges, this handler reads the rollups and returns the real
//! `PivotResult` (the SAME struct the CLI `--json` and webui consume). The
//! route existing (200, empty) — rather than 404 — is deliberate: clients
//! feature-detect on 404 (old daemon) vs 200 (route present), and an empty
//! pivot is a truthful "no data yet", not "route missing".
//!
//! `GET /api/token/status`'s `level` is likewise a best-effort default
//! (`conservative`, the product default) until Track 2's
//! `token_optimization` config block lands and Track 4 wires per-call/config
//! level resolution.

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
use super::AppState;
use crate::commands::search_common::{collection_cache_dir, collection_name_readonly};
use onebrain_token::{LedgerVerdict, TokenCache};

/// The `token/` sibling of `models/` + `index/` under a collection cache dir,
/// and the `token.redb` file inside it. `CollectionLayout` scans only
/// `models--*` + the index artifacts, so `token/` collides with nothing
/// (design §1).
fn token_db_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("token").join("token.redb")
}

/// Open (creating dir + file) the daemon-owned token cache for `vault_root`.
/// Returns `None` (logged at `warn`, like [`super::internal::open_held_engine`])
/// on any failure — no config, unresolvable collection, or an unopenable DB —
/// so the token routes degrade to "cache unavailable" rather than the process
/// failing to boot.
pub(super) fn open_held_token_cache(vault_root: &Path) -> Option<Arc<TokenCache>> {
    match try_open_held_token_cache(vault_root) {
        Ok(cache) => {
            tracing::info!(
                vault = %vault_root.display(),
                "daemon holding token.redb cache for process lifetime"
            );
            Some(Arc::new(cache))
        }
        Err(e) => {
            tracing::warn!(
                vault = %vault_root.display(),
                error = %e,
                "could not open token.redb cache; /api/token/* routes report unavailable"
            );
            None
        }
    }
}

fn try_open_held_token_cache(vault_root: &Path) -> anyhow::Result<TokenCache> {
    let collection = collection_name_readonly(vault_root)?;
    let cache_dir = collection_cache_dir(&collection);
    let db_path = token_db_path(&cache_dir);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(TokenCache::open(&db_path)?)
}

/// Build the `/token` sub-router (mounted under `/api` by
/// [`super::build_router`]).
pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/token/gain", get(get_token_gain))
        .route("/token/status", get(get_token_status))
        .route("/token/ledger/check", post(post_ledger_check))
}

/// Pull the held token cache out of state, or 503 when the daemon holds none.
fn require_token_cache(state: &AppState) -> Result<Arc<TokenCache>, ApiError> {
    state
        .token_cache
        .clone()
        .ok_or_else(|| ApiError::ServiceUnavailable("daemon holds no token cache".to_string()))
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/token/gain  (STUB — Track 2 rollups)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GainQuery {
    by: Option<String>,
    since: Option<String>,
    history: Option<bool>,
}

/// Returns the gain pivot. STUB until Track 2 (see module note): an empty
/// pivot shape with the echoed query, so a client can distinguish "route
/// present, no data" (200) from "route missing" (404 on an old daemon).
async fn get_token_gain(Query(q): Query<GainQuery>) -> Response {
    Json(serde_json::json!({
        "rows": [],
        "totals": serde_json::Value::Null,
        "by": q.by,
        "since": q.since,
        "history": q.history.unwrap_or(false),
        // Removed once Track 2's PivotResult + rollups land here.
        "pending": "token gain rollups land with Track 2 (PivotResult)",
    }))
    .into_response()
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/token/status
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, PartialEq)]
struct TokenStatusResponse {
    /// Effective optimization level. Best-effort `"conservative"` (product
    /// default) until Track 2's `token_optimization` config lands + Track 4
    /// wires real resolution.
    level: String,
    /// Whether the already-sent ledger is active — true at level 2↑
    /// (`balanced`/`aggressive`), matching design §3b's activation rule.
    ledger_active: bool,
    /// On-disk byte size of `token.redb`, or `0` when the file doesn't exist
    /// yet (cache held but never written).
    cache_bytes: u64,
}

/// Level 2↑ (`balanced`/`aggressive`) activates the ledger (design §3b).
fn ledger_active_for_level(level: &str) -> bool {
    matches!(level, "balanced" | "aggressive")
}

async fn get_token_status(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();
    // Holding the cache is required — this route reports on it.
    let _cache = require_token_cache(&state)?;

    let cache_bytes = tokio::task::spawn_blocking(move || {
        let collection = collection_name_readonly(&root).ok()?;
        let db_path = token_db_path(&collection_cache_dir(&collection));
        std::fs::metadata(db_path).ok().map(|m| m.len())
    })
    .await
    .map_err(|e| ApiError::Internal(format!("status task join: {e}")))?
    .unwrap_or(0);

    // Track 4 replaces this default with `config.token_optimization.level`.
    let level = "conservative".to_string();
    let ledger_active = ledger_active_for_level(&level);

    Ok(Json(TokenStatusResponse {
        level,
        ledger_active,
        cache_bytes,
    })
    .into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/token/ledger/check
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LedgerCheckRequest {
    /// Vault-relative doc path to check.
    path: String,
    /// The resolved session token. Empty/absent → ledger inactive for this
    /// call (design §3b: never guess a token); the verdict is `no_session`.
    #[serde(default)]
    session_token: String,
    /// When `true`, record the current hash after a `first_send`/`changed`
    /// verdict — lets the read-hook (Track 5/8) atomically "allow + mark sent"
    /// so the NEXT repeat read is caught. Default `false` (pure check).
    #[serde(default)]
    record: bool,
}

/// The reference envelope embedded in an `unchanged` verdict (design §3b) —
/// the frozen shape Track 4/5 consume. `sent_earlier` is always `true` here.
#[derive(Debug, Serialize, PartialEq)]
struct ReferenceEnvelope {
    doc_path: String,
    hash: String,
    sent_earlier: bool,
    bytes_saved: u64,
    rematerialize: String,
}

#[derive(Debug, Serialize, PartialEq)]
struct LedgerCheckResponse {
    /// `first_send` | `unchanged` | `changed` | `no_session` | `unknown_doc`.
    verdict: String,
    /// Present only on `unchanged` — the reference a caller may send instead
    /// of the full body.
    #[serde(skip_serializing_if = "Option::is_none")]
    reference: Option<ReferenceEnvelope>,
}

async fn post_ledger_check(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LedgerCheckRequest>,
) -> Result<Response, ApiError> {
    if req.path.trim().is_empty() {
        return Err(ApiError::BadRequest("empty path".to_string()));
    }
    let cache = require_token_cache(&state)?;

    // No session token → ledger silently inactive for this call (never guess).
    if req.session_token.trim().is_empty() {
        return Ok(Json(LedgerCheckResponse {
            verdict: "no_session".to_string(),
            reference: None,
        })
        .into_response());
    }

    let engine = state.search_engine.clone();
    let path = req.path.clone();
    let session = req.session_token.clone();
    let record = req.record;

    let response = tokio::task::spawn_blocking(move || -> Result<LedgerCheckResponse, ApiError> {
        // Current content hash from the held engine. Unknown/unindexed doc →
        // we cannot compare, so the ledger can't claim "unchanged".
        let current_hash = match &engine {
            Some(shared) => {
                let guard = shared.lock().unwrap_or_else(|p| p.into_inner());
                guard.doc_hash(&path)
            }
            None => None,
        };
        let Some(current_hash) = current_hash else {
            return Ok(LedgerCheckResponse {
                verdict: "unknown_doc".to_string(),
                reference: None,
            });
        };

        let ledger = cache.ledger();
        let verdict = ledger
            .check(&session, &path, &current_hash)
            .map_err(|e| ApiError::Internal(format!("ledger check: {e}")))?;

        let response = match &verdict {
            LedgerVerdict::Unchanged { sent_hash } => {
                // Credit the full avoided inline size when the body is cheaply
                // reconstructable; best-effort (0 on any error — never fail the
                // decision on a size estimate).
                let bytes_saved = engine
                    .as_ref()
                    .and_then(|s| {
                        let guard = s.lock().unwrap_or_else(|p| p.into_inner());
                        guard.get(&path).ok()
                    })
                    .map(|body| body.len() as u64)
                    .unwrap_or(0);
                LedgerCheckResponse {
                    verdict: "unchanged".to_string(),
                    reference: Some(ReferenceEnvelope {
                        doc_path: path.clone(),
                        hash: sent_hash.clone(),
                        sent_earlier: true,
                        bytes_saved,
                        rematerialize: format!("onebrain search get {path} --force"),
                    }),
                }
            }
            LedgerVerdict::FirstSend | LedgerVerdict::Changed => {
                if record {
                    ledger
                        .record(&session, &path, &current_hash)
                        .map_err(|e| ApiError::Internal(format!("ledger record: {e}")))?;
                }
                let verdict = if matches!(verdict, LedgerVerdict::FirstSend) {
                    "first_send"
                } else {
                    "changed"
                };
                LedgerCheckResponse {
                    verdict: verdict.to_string(),
                    reference: None,
                }
            }
        };
        Ok(response)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("ledger task join: {e}")))??;

    Ok(Json(response).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{build_router, ServeConfig};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TOKEN: &str = "token-api-test-token-1234567890";

    #[test]
    fn token_db_path_is_token_sibling() {
        let p = token_db_path(Path::new("/cache/col"));
        assert!(p.ends_with("token/token.redb"));
        assert!(p.starts_with("/cache/col"));
    }

    #[test]
    fn ledger_active_only_at_balanced_and_above() {
        assert!(!ledger_active_for_level("off"));
        assert!(!ledger_active_for_level("conservative"));
        assert!(ledger_active_for_level("balanced"));
        assert!(ledger_active_for_level("aggressive"));
    }

    #[test]
    fn open_held_token_cache_none_on_non_vault_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        assert!(open_held_token_cache(dir.path()).is_none());
    }

    /// A vault with a config + an isolated cache dir; the held daemon opens
    /// both the engine and the token cache.
    fn vault_and_cache() -> (tempfile::TempDir, tempfile::TempDir) {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: token-api-test\n",
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        (vault, cache)
    }

    fn router_holding_cache(vault: &Path) -> axum::Router {
        let mut cfg = ServeConfig::localhost(Some(vault.to_path_buf()), 0, TOKEN.to_string(), None);
        cfg.hold_engine = true;
        build_router(cfg)
    }

    #[test]
    fn open_held_token_cache_some_on_valid_vault() {
        let (vault, cache) = vault_and_cache();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        assert!(open_held_token_cache(vault.path()).is_some());
    }

    #[tokio::test]
    async fn status_returns_shape_with_held_cache() {
        let (vault, cache) = vault_and_cache();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_cache(vault.path());

        let resp = router
            .oneshot(
                Request::get("/api/token/status")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["level"], "conservative");
        assert_eq!(v["ledger_active"], false);
        assert!(v["cache_bytes"].is_u64());
    }

    #[tokio::test]
    async fn status_503_without_held_cache() {
        // No hold_engine → no token cache → the route reports unavailable
        // rather than opening a second redb owner per request.
        let (vault, _cache) = vault_and_cache();
        let cfg =
            ServeConfig::localhost(Some(vault.path().to_path_buf()), 0, TOKEN.to_string(), None);
        let router = build_router(cfg);

        let resp = router
            .oneshot(
                Request::get("/api/token/status")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn gain_route_returns_stub_200() {
        let (vault, cache) = vault_and_cache();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_cache(vault.path());

        let resp = router
            .oneshot(
                Request::get("/api/token/gain?by=month,surface&history=true")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Route present (200, not 404) so clients feature-detect correctly.
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["rows"], serde_json::json!([]));
        assert_eq!(v["by"], "month,surface");
        assert!(v.get("pending").is_some(), "stub marks Track 2 pending");
    }

    #[tokio::test]
    async fn ledger_check_empty_path_400() {
        let (vault, cache) = vault_and_cache();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_cache(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/token/ledger/check")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"path":"","session_token":"s"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ledger_check_no_session_returns_no_session_verdict() {
        // Empty session token → ledger silently inactive (design §3b): the
        // daemon never guesses a token, so the verdict is `no_session`.
        let (vault, cache) = vault_and_cache();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_cache(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/token/ledger/check")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"path":"a.md","session_token":""}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["verdict"], "no_session");
        assert!(v.get("reference").is_none());
    }

    #[tokio::test]
    async fn ledger_check_unknown_doc_when_not_indexed() {
        // A real session token but a path the engine has never indexed:
        // doc_hash is None, so the ledger cannot claim "unchanged" — the
        // verdict is `unknown_doc`, never a false reference.
        let (vault, cache) = vault_and_cache();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_cache(vault.path());

        let resp = router
            .oneshot(
                Request::post("/api/token/ledger/check")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"path":"never-indexed.md","session_token":"sess-1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["verdict"], "unknown_doc");
    }
}
