//! `/api/token/*` — the warm daemon's token-optimization endpoints (design
//! §5/§5b). The daemon is the sole owner of `token.redb` (memoization +
//! already-sent ledger, Track 3); these routes serve the ledger and gain
//! surfaces over HTTP so MCP + CLI clients (and the read-hook, Track 5/8) stay
//! lock-free HTTP clients of the one owner.
//!
//! ```text
//!   GET  /api/token/gain?by=&since=&history=  → PivotResult JSON (Tier-2 rollups)
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
//! `GET /api/token/gain` serves the real [`PivotResult`] from the Tier-2
//! rollup tables in the daemon-owned `token.redb` — the SAME
//! [`onebrain_token::gain::pivot::query`] engine the CLI `token gain` command
//! reads, and the same by-axes parsing ([`crate::commands::token_gain::parse_by`]),
//! so the route, the CLI `--json`, and the WebUI all agree on one code path
//! (design §5 "one pivot engine ... serves all three consumers"). An empty
//! pivot is a truthful "no gain data yet", never hardcoded. Version-skew
//! contract: a daemon too old to have the route answers 404 → the client skips
//! optimization; a present route answers 200 with the real-or-empty pivot.
//!
//! `GET /api/token/status`'s `level` reflects the vault's resolved
//! `token_optimization.level` (`resolve_level`, Track 4 / M3).

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
use crate::commands::token_gain::parse_by;
use onebrain_token::gain::pivot;
#[allow(unused_imports)] // PivotResult is referenced in rustdoc intra-doc links.
use onebrain_token::PivotResult;
use onebrain_token::{LedgerVerdict, PivotQuery, ReferenceEnvelope, TokenCache};

/// Resolve the effective optimization level for `vault_root` from
/// `token_optimization.level` in `onebrain.yml`, as its wire string
/// (`off|conservative|balanced|aggressive`). Best-effort: any config-load
/// failure falls back to the product default `conservative` — a server route
/// must never fail just because the config couldn't be read, and conservative
/// keeps the ledger inactive (safe) rather than emitting references.
fn resolve_level(vault_root: &Path) -> String {
    onebrain_core::config::load_vault_config_at(vault_root)
        .map(|cfg| cfg.token_optimization.level.to_string())
        .unwrap_or_else(|_| "conservative".to_string())
}

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
// GET /api/token/gain
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GainQuery {
    /// `time,dim` (either order, either alone) — parsed by the shared
    /// [`parse_by`] so the route and the CLI agree on the axes.
    by: Option<String>,
    /// Inclusive `YYYY-MM-DD` lower bound on the pivot window.
    since: Option<String>,
    /// Accepted for wire-compat with the CLI flags; the route serves the
    /// Tier-2 rollup pivot (design §5), which is what the WebUI dashboard
    /// consumes — the raw per-call log tail (`--history`) is a CLI-only view.
    #[serde(default)]
    history: Option<bool>,
}

/// Returns the real [`PivotResult`] from the daemon-owned `token.redb` Tier-2
/// rollups via the shared [`pivot::query`] engine. Empty pivot when there's
/// genuinely no gain data — never hardcoded. 503 when the daemon holds no
/// token cache; a bad `by=` value is a 400.
async fn get_token_gain(
    State(state): State<Arc<AppState>>,
    Query(q): Query<GainQuery>,
) -> Result<Response, ApiError> {
    let _ = q.history; // accepted for wire-compat; rollup pivot is the source.
    let cache = require_token_cache(&state)?;

    let (time, dim) = parse_by(q.by.as_deref()).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let query = PivotQuery {
        time,
        dim,
        since: q.since,
    };

    // The pivot is a synchronous redb scan of the rollup table — run it off the
    // async runtime, like the other token routes.
    let result = tokio::task::spawn_blocking(move || pivot::query(cache.database(), &query))
        .await
        .map_err(|e| ApiError::Internal(format!("token gain task join: {e}")))?
        .map_err(|e| ApiError::Internal(format!("token gain pivot: {e}")))?;
    Ok(Json(result).into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/token/status
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, PartialEq)]
struct TokenStatusResponse {
    /// Effective optimization level, resolved from the vault's
    /// `token_optimization.level` config (`resolve_level`, Track 4 / M3).
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

    let level = resolve_level(&root);

    let cache_bytes = tokio::task::spawn_blocking(move || {
        let collection = collection_name_readonly(&root).ok()?;
        let db_path = token_db_path(&collection_cache_dir(&collection));
        std::fs::metadata(db_path).ok().map(|m| m.len())
    })
    .await
    .map_err(|e| ApiError::Internal(format!("status task join: {e}")))?
    .unwrap_or(0);

    // The resolved level (T4): `off`/`conservative` keep the ledger inactive;
    // `balanced`/`aggressive` activate it (design §3b).
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
    /// **Dormant escape-hatch — every live caller sends `None`.** As of #255 the
    /// ledger keys on the doc's index identity (`Engine::doc_hash`) for the
    /// read-hook AND both `get` surfaces, so all three share one entry. When
    /// present, this pins the ledger key to THIS hash instead of the derived
    /// `doc_hash`.
    ///
    /// ⚠️ TRAP: sending a `content_hash` re-forks the ledger key away from the
    /// read-hook's `doc_hash` key, silently reintroducing the cross-surface
    /// dedup gap #255 fixes — a `get` keyed on a bespoke hash will never collide
    /// with a later `token check` keyed on `doc_hash`. Only set it if you
    /// understand that and deliberately want per-caller (non-shared) keying.
    #[serde(default)]
    content_hash: Option<String>,
    /// Size of the delivered body, credited to `bytes_saved` on an `unchanged`
    /// verdict (avoids a second engine read). Sent by the `get` surfaces
    /// alongside `content_hash: None`; the read-hook omits it (0 → best-effort
    /// engine read).
    #[serde(default)]
    bytes: Option<u64>,
}

#[derive(Debug, Serialize, PartialEq)]
struct LedgerCheckResponse {
    /// `first_send` | `unchanged` | `changed` | `no_session` | `unknown_doc`
    /// | `inactive`.
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

    let vault_root = require_vault_root(&state)?;

    // M3 — server-side level gate (design §3b): the already-sent ledger only
    // activates at `balanced`/`aggressive`. At `off`/`conservative` the route
    // must NEVER touch the ledger or emit a reference envelope — so a
    // conservative vault (the default) can't hand back references. Gated here,
    // before the cache/session checks, so it holds no matter which client
    // (MCP get, CLI, read-hook) calls the route.
    let level = resolve_level(vault_root);
    if !ledger_active_for_level(&level) {
        return Ok(Json(LedgerCheckResponse {
            verdict: "inactive".to_string(),
            reference: None,
        })
        .into_response());
    }

    // Normalize the incoming path to the index's vault-relative key form. The
    // read-hook's daemon leg sends an ABSOLUTE path (Claude Code's Read passes
    // absolute), while `search get` records the RELATIVE path; the daemon is
    // vault-bound, so normalize against its own root here — one shared
    // normalizer with every other ledger surface, so the key `(session, path,
    // hash)` collides across surfaces (#255). Idempotent on a relative path.
    let path = crate::commands::search_common::normalize_doc_path(&req.path, vault_root);

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
    let session = req.session_token.clone();
    let record = req.record;
    let provided_hash = req.content_hash.clone();
    let provided_bytes = req.bytes;

    let response = tokio::task::spawn_blocking(move || -> Result<LedgerCheckResponse, ApiError> {
        // Key on the doc's index identity (`Engine::doc_hash`) — the shared key
        // every live caller (read-hook AND both get surfaces) relies on, so a
        // `get` and a later `token check` of the same doc collide on ONE entry
        // (#255). An unknown/unindexed doc yields no hash, so the ledger can't
        // claim "unchanged".
        let current_hash = match provided_hash {
            // Dormant escape-hatch: a bespoke hash pins a per-caller key that
            // does NOT share with the read-hook (see `LedgerCheckRequest
            // ::content_hash`'s TRAP note). No live caller sets this.
            Some(h) => Some(h),
            None => match &engine {
                Some(shared) => {
                    let guard = shared.lock().unwrap_or_else(|p| p.into_inner());
                    guard.doc_hash(&path)
                }
                None => None,
            },
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
                // Credit the avoided inline size: the caller's own body size
                // when supplied, else a best-effort engine read (0 on error —
                // never fail the decision on a size estimate).
                let bytes_saved = provided_bytes
                    .or_else(|| {
                        engine.as_ref().and_then(|s| {
                            let guard = s.lock().unwrap_or_else(|p| p.into_inner());
                            guard.get(&path).ok().map(|body| body.len() as u64)
                        })
                    })
                    .unwrap_or(0);
                LedgerCheckResponse {
                    verdict: "unchanged".to_string(),
                    reference: Some(ReferenceEnvelope::new(
                        path.clone(),
                        sent_hash.clone(),
                        bytes_saved,
                    )),
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
    /// both the engine and the token cache. No `token_optimization` block, so
    /// the level resolves to the product default `conservative`.
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

    /// Like [`vault_and_cache`] but pins `token_optimization.level` — the
    /// ledger-verdict tests need `balanced` (level 2↑) so the M3 route gate
    /// lets the check run rather than short-circuiting to `inactive`.
    fn vault_and_cache_at_level(level: &str) -> (tempfile::TempDir, tempfile::TempDir) {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            format!(
                "search:\n  collection: token-api-test\ntoken_optimization:\n  level: {level}\n"
            ),
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
    async fn gain_route_empty_when_no_data() {
        // Route present (200, not 404) so clients feature-detect correctly;
        // a fresh token.redb with no rollups is a truthful empty pivot.
        let (vault, cache) = vault_and_cache();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_cache(vault.path());

        let resp = router
            .oneshot(
                Request::get("/api/token/gain?by=surface")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["rows"], serde_json::json!([]), "no data → empty rows");
        assert_eq!(v["totals"]["count"], 0);
        assert!(v.get("pending").is_none(), "no stub marker anymore");
    }

    /// The seam proof (plan step 3.5): seed a couple of gain events into the
    /// daemon-owned `token.redb`, roll them up, then hit the route and assert
    /// the response carries the SEEDED pivot rows — the real
    /// `onebrain_token::gain::pivot` engine over the real rollup tables, not a
    /// hardcoded shape.
    #[tokio::test]
    async fn gain_route_serves_seeded_rollup_rows() {
        use onebrain_token::gain::rollup;
        use onebrain_token::{CacheKind, Database, GainEvent, OptLevel, Surface};

        let (vault, cache) = vault_and_cache();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());

        // Seed the rollups BEFORE the daemon opens the DB (redb is single-writer
        // — the seeding handle is dropped before the router opens it).
        let cache_dir = collection_cache_dir(&collection_name_readonly(vault.path()).unwrap());
        let db_path = token_db_path(&cache_dir);
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let event = |ts, surface, before, after| GainEvent {
            ts,
            surface,
            transform: "whitespace".to_string(),
            level: OptLevel::Conservative,
            bytes_before: before,
            bytes_after: after,
            cache: CacheKind::None,
            session_token: None,
        };
        {
            let db = Database::create(&db_path).unwrap();
            rollup::update(&db, &event(1_783_728_000, Surface::CliSearch, 1000, 400)).unwrap();
            rollup::update(&db, &event(1_783_900_800, Surface::McpQuery, 500, 100)).unwrap();
        } // drop releases the redb lock so the daemon can take it

        let router = router_holding_cache(vault.path());
        let resp = router
            .oneshot(
                Request::get("/api/token/gain?by=surface")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // Two surfaces seeded → two pivot rows, and the grand total sums both.
        let rows = v["rows"].as_array().expect("rows array");
        assert_eq!(rows.len(), 2, "one row per seeded surface: {v}");
        let surfaces: Vec<&str> = rows.iter().filter_map(|r| r["dim"].as_str()).collect();
        assert!(surfaces.contains(&"cli_search"), "got {surfaces:?}");
        assert!(surfaces.contains(&"mcp_query"), "got {surfaces:?}");
        assert_eq!(v["totals"]["bytes_before"], 1500);
        assert_eq!(v["totals"]["bytes_after"], 500);
        assert_eq!(v["totals"]["count"], 2);
    }

    #[tokio::test]
    async fn gain_route_rejects_bad_by_axis() {
        let (vault, cache) = vault_and_cache();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_cache(vault.path());
        let resp = router
            .oneshot(
                Request::get("/api/token/gain?by=bogus")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn gain_route_503_without_held_cache() {
        let (vault, _cache) = vault_and_cache();
        let cfg =
            ServeConfig::localhost(Some(vault.path().to_path_buf()), 0, TOKEN.to_string(), None);
        let router = build_router(cfg);
        let resp = router
            .oneshot(
                Request::get("/api/token/gain")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// #257: a standalone `serve` (`open_token_cache = true`, no held engine)
    /// opens the token cache, so the Gain route answers 200 with an honest empty
    /// pivot instead of 503 — the dashboard lights up on the `--port` escape
    /// hatch, not just under the daemon.
    #[tokio::test]
    async fn gain_route_ok_with_open_token_cache_and_no_held_engine() {
        let (vault, cache) = vault_and_cache();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let mut cfg =
            ServeConfig::localhost(Some(vault.path().to_path_buf()), 0, TOKEN.to_string(), None);
        cfg.open_token_cache = true; // standalone serve, engine NOT held
        let router = build_router(cfg);
        let resp = router
            .oneshot(
                Request::get("/api/token/gain?by=surface")
                    .header("x-onebrain-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["rows"], serde_json::json!([]), "no data → empty pivot");
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
    async fn ledger_check_inactive_at_conservative_never_references() {
        // M3: at the default `conservative` level the route gate short-circuits
        // to `inactive` BEFORE any ledger access — a conservative vault can
        // never hand back a reference envelope, even for an already-sent doc.
        let (vault, cache) = vault_and_cache(); // no token_optimization → conservative
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let router = router_holding_cache(vault.path());

        let v = ledger_check(&router, r#"{"path":"a.md","session_token":"sess-1"}"#).await;
        assert_eq!(v["verdict"], "inactive");
        assert!(v.get("reference").is_none(), "no reference at conservative");
    }

    #[tokio::test]
    async fn ledger_check_no_session_returns_no_session_verdict() {
        // Empty session token → ledger silently inactive (design §3b): the
        // daemon never guesses a token, so the verdict is `no_session`. Needs a
        // ledger-active level (balanced) to reach the session check past the M3
        // gate.
        let (vault, cache) = vault_and_cache_at_level("balanced");
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

    /// Drive a `mode:"lex"` reindex through the held engine (no model load) so
    /// a real doc becomes indexed — the seam MAJOR 4 needs to exercise the
    /// non-`unknown_doc` verdict branches over HTTP.
    async fn lex_reindex(router: &axum::Router) {
        let resp = router
            .clone()
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
    }

    async fn ledger_check(router: &axum::Router, body: &str) -> serde_json::Value {
        let resp = router
            .clone()
            .oneshot(
                Request::post("/api/token/ledger/check")
                    .header("x-onebrain-token", TOKEN)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// MAJOR 4: end-to-end route coverage of the verdict path on an INDEXED
    /// doc — first_send (record) → unchanged (+ reference envelope) → edit →
    /// changed. Catches a serde-rename typo or a swapped current/sent hash that
    /// the `unknown_doc`-only tests never touch. Uses lex-only indexing so no
    /// embedding model is loaded.
    #[tokio::test]
    async fn ledger_check_verdict_path_first_send_unchanged_changed() {
        let (vault, cache) = vault_and_cache_at_level("balanced");
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        std::fs::write(vault.path().join("a.md"), "# A\noriginal body text").unwrap();
        let router = router_holding_cache(vault.path());

        // Index a.md (lex-only) so the engine's doc_hash resolves.
        lex_reindex(&router).await;

        // 1) First send, with record:true → verdict first_send, hash recorded.
        let v = ledger_check(
            &router,
            r#"{"path":"a.md","session_token":"sess-1","record":true}"#,
        )
        .await;
        assert_eq!(v["verdict"], "first_send");
        assert!(v.get("reference").is_none(), "no reference on first_send");

        // 2) Repeat send of unchanged content → unchanged + reference envelope.
        let v = ledger_check(&router, r#"{"path":"a.md","session_token":"sess-1"}"#).await;
        assert_eq!(v["verdict"], "unchanged");
        let r = &v["reference"];
        assert_eq!(r["doc_path"], "a.md");
        assert_eq!(r["sent_earlier"], true);
        assert_eq!(r["rematerialize"], "onebrain search get a.md --force");
        assert!(r["hash"].as_str().is_some_and(|h| !h.is_empty()));
        assert!(
            r["bytes_saved"].as_u64().unwrap() > 0,
            "a real indexed body credits non-zero bytes_saved"
        );

        // 3) Edit the doc + re-index → hash changes → verdict changed.
        std::fs::write(
            vault.path().join("a.md"),
            "# A\nEDITED body, different bytes",
        )
        .unwrap();
        lex_reindex(&router).await;
        let v = ledger_check(&router, r#"{"path":"a.md","session_token":"sess-1"}"#).await;
        assert_eq!(
            v["verdict"], "changed",
            "an edited doc (new hash) must read as changed, not unchanged"
        );
        assert!(v.get("reference").is_none(), "no reference on changed");
    }

    #[tokio::test]
    async fn ledger_check_unknown_doc_when_not_indexed() {
        // A real session token but a path the engine has never indexed:
        // doc_hash is None, so the ledger cannot claim "unchanged" — the
        // verdict is `unknown_doc`, never a false reference. Balanced so the
        // M3 gate lets the check run.
        let (vault, cache) = vault_and_cache_at_level("balanced");
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

    /// #255 daemon-leg path normalization: record under the RELATIVE key, then
    /// check with an ABSOLUTE path (exactly as the read-hook's daemon leg sends
    /// it). The route must normalize it to the same relative key and read
    /// `unchanged`. Before normalization the absolute path missed the relative
    /// index key → `unknown_doc`, so the read-hook never gated via the daemon.
    #[tokio::test]
    async fn ledger_check_normalizes_absolute_path_to_relative_key() {
        let (vault, cache) = vault_and_cache_at_level("balanced");
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        std::fs::write(
            vault.path().join("a.md"),
            "# A\nbody for the abs-path daemon test",
        )
        .unwrap();
        let router = router_holding_cache(vault.path());
        lex_reindex(&router).await;

        // Record under the relative key.
        let v = ledger_check(
            &router,
            r#"{"path":"a.md","session_token":"sess-1","record":true}"#,
        )
        .await;
        assert_eq!(v["verdict"], "first_send");

        // Check with the ABSOLUTE path → must normalize to "a.md" and match.
        let abs = vault.path().join("a.md");
        let body = serde_json::json!({
            "path": abs.to_str().unwrap(),
            "session_token": "sess-1",
        })
        .to_string();
        let v = ledger_check(&router, &body).await;
        assert_eq!(
            v["verdict"], "unchanged",
            "an absolute path must normalize to the recorded relative key and read unchanged"
        );
        assert_eq!(
            v["reference"]["doc_path"], "a.md",
            "the reference must report the normalized relative key"
        );
    }

    /// The dormant `content_hash` escape-hatch keys the ledger on a BESPOKE hash,
    /// NOT the doc's `doc_hash` — so a request that supplies one does NOT collide
    /// with a `doc_hash`-keyed entry (the documented #255 trap). Guards against a
    /// future change that would silently re-share the key. Record via the normal
    /// `doc_hash` path, then a `content_hash`-bearing check of the SAME doc must
    /// NOT read `unchanged`.
    #[tokio::test]
    async fn ledger_check_content_hash_does_not_collide_with_doc_hash_entry() {
        let (vault, cache) = vault_and_cache_at_level("balanced");
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        std::fs::write(vault.path().join("a.md"), "# A\nbody for the trap test").unwrap();
        let router = router_holding_cache(vault.path());
        lex_reindex(&router).await;

        // Record under the doc's `doc_hash` (no content_hash).
        let v = ledger_check(
            &router,
            r#"{"path":"a.md","session_token":"sess-1","record":true}"#,
        )
        .await;
        assert_eq!(v["verdict"], "first_send");

        // A content_hash-bearing check keys on the BESPOKE hash → does NOT match
        // the doc_hash entry → not `unchanged` (the documented trap).
        let v = ledger_check(
            &router,
            r#"{"path":"a.md","session_token":"sess-1","content_hash":"deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"}"#,
        )
        .await;
        assert_ne!(
            v["verdict"], "unchanged",
            "a bespoke content_hash must NOT collide with the doc_hash-keyed entry (the #255 trap)"
        );
    }
}
