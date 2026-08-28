//! Shared OAuth server state ([`AuthCtx`]) + the two PUBLIC discovery
//! documents: RFC 9728 §3.1 OAuth Protected Resource Metadata and RFC 8414
//! §3.2 Authorization Server Metadata. `/register` (Task 3), `/authorize`
//! (Task 4), and `/token` (Task 5) land in this same file, against this same
//! `AuthCtx`.
//!
//! ## Layer scoping (binding — see `server::build_gateway_router`)
//! Every route this file adds is reachable WITHOUT a bearer token — no
//! [`super::auth::middleware::require_bearer`] layer. A client that has no
//! token yet MUST be able to fetch these documents to learn where to get one
//! (RFC 9728 §5 / RFC 8414 §3); gating them would be a bootstrapping
//! deadlock. `build_gateway_router` enforces this by construction: it
//! applies the Bearer layer to the `/mcp` nest BEFORE merging this router's
//! routes in, so the layer never wraps them. See
//! `server::tests::well_known_routes_are_reachable_without_auth_while_mcp_stays_gated`
//! for the end-to-end proof.

use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use super::auth::AuthStore;

/// Pairing-attempt rate-limiter state (5 wrong pairing codes → 60s lockout —
/// Task 4's `/authorize` POST handler). Defined here now, empty, per the
/// plan's preflight ruling (`progress.md`, "AttemptState field must be added
/// to AuthCtx in T2 or T4"): [`AuthCtx`] should be complete at construction
/// rather than growing a field later just to satisfy a different task's
/// needs. Default-constructible; the real fields (attempt counts, a
/// lockout-until timestamp) land with the handler that reads/writes them.
#[derive(Debug, Default)]
pub struct AttemptState {}

/// Shared state for every gateway OAuth route — the well-known discovery
/// handlers below now, `/register`/`/authorize`/`/token` in Tasks 3-5 — AND
/// the `/mcp` Bearer gate ([`super::auth::middleware::require_bearer`]).
///
/// `store` MUST stay `Mutex<AuthStore>` — NEVER cloned out of the mutex — so
/// every access holds the lock across its full read-modify-write.
/// `AuthStore`'s on-disk JSON files are plain read-then-write with no
/// file-level locking of their own (see `store.rs`'s module docs), so two
/// concurrent in-process axum requests without this discipline could
/// double-spend a single-use auth code or race past refresh-token reuse
/// detection. This task only adds a READ (`check_access`, in the Bearer
/// gate) through the lock; Tasks 3-5's mutating `/authorize`/`/token`/
/// `/register` handlers share this SAME `store` field and MUST follow the
/// same hold-the-lock-across-the-whole-operation discipline (Task 1 security
/// review finding, binding requirement A on this task).
pub struct AuthCtx {
    pub store: Mutex<AuthStore>,
    /// The gateway's own OAuth issuer base URL — e.g. `http://127.0.0.1:7717`
    /// or a configured `public_url` (`gateway::resolve_issuer`). Set exactly
    /// once, from `on_bind` (so a `--port 0` ephemeral bind is resolved
    /// before anything reads it) — `run_server_from_router`'s #278 ordering
    /// (`server/mod.rs:368-396`) guarantees `on_bind` fires after the
    /// listener is confirmed up and before `axum::serve` starts accepting
    /// connections, so every request that reaches a handler observes a set
    /// issuer. Oneshot tests set this explicitly before building the router.
    pub issuer: OnceLock<String>,
    /// Constructed now (see [`AttemptState`]'s doc comment for the "why add
    /// it before it's used" rationale) but not yet READ anywhere — Task 4's
    /// `/authorize` POST handler is the first reader/writer. `#[allow]`ed
    /// explicitly rather than silently, matching this crate's other
    /// documented not-yet-wired-in allows (see `auth/mod.rs`'s module-level
    /// one) — this is a single field, so a blanket module allow would be
    /// overkill here.
    #[allow(dead_code)]
    pub attempts: Mutex<AttemptState>,
}

impl AuthCtx {
    pub fn new(store: AuthStore) -> Self {
        Self {
            store: Mutex::new(store),
            issuer: OnceLock::new(),
            attempts: Mutex::new(AttemptState::default()),
        }
    }

    /// The resolved issuer, or a loopback placeholder if somehow read before
    /// `on_bind` ran. Panics-free by design (`unwrap_or`, not `expect`) — the
    /// fallback is unreachable in practice given the #278 ordering guarantee
    /// (see the `issuer` field doc comment), but a middleware/handler must
    /// never crash a live request over it either way.
    pub fn issuer(&self) -> &str {
        self.issuer
            .get()
            .map(String::as_str)
            .unwrap_or("http://127.0.0.1")
    }
}

/// RFC 9728 §3.1 OAuth Protected Resource Metadata document. Served at both
/// the bare well-known path and the `/mcp`-suffixed variant — RFC 9728 §3.1's
/// path-insertion convention for a protected resource that itself lives at a
/// sub-path (`{issuer}/mcp` here); different MCP clients probe one or the
/// other, and the `/mcp` 401 challenge (`middleware::challenge`) always
/// points at the bare path. PUBLIC (see module docs): a client with no token
/// yet must be able to fetch this to learn its authorization server.
async fn protected_resource_metadata(State(ctx): State<Arc<AuthCtx>>) -> Json<Value> {
    let issuer = ctx.issuer();
    Json(json!({
        "resource": format!("{issuer}/mcp"),
        "authorization_servers": [issuer],
        "scopes_supported": ["brain"],
        "bearer_methods_supported": ["header"],
    }))
}

/// RFC 8414 §3.2 Authorization Server Metadata document. Every endpoint named
/// here (`/authorize`, `/token`, `/register`) lands in Tasks 3-5 against this
/// SAME `{issuer}` base — advertising them now documents the resource-
/// server/authorization-server contract even before the handlers exist.
/// `code_challenge_methods_supported: ["S256"]` is the field the binary e2e
/// test (Task 6) explicitly asserts on after following
/// `authorization_servers[0]` from the PRM document. PUBLIC — same
/// bootstrapping rationale as the PRM document above.
async fn authorization_server_metadata(State(ctx): State<Arc<AuthCtx>>) -> Json<Value> {
    let issuer = ctx.issuer();
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "registration_endpoint": format!("{issuer}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["brain"],
    }))
}

/// The public `/.well-known/*` discovery surface — deliberately built as a
/// fully self-contained `Router` (state applied here, not deferred to the
/// caller) so `server::build_gateway_router` can `.merge()` it directly
/// alongside the Bearer-gated `/mcp` nest without ever routing it through
/// [`super::auth::middleware::require_bearer`]. See the module docs for why
/// that matters.
pub fn well_known_router(ctx: Arc<AuthCtx>) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .with_state(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// The well-known handlers never touch `store` (only `ctx.issuer()`), so
    /// the returned `TempDir` guard exists purely to keep the auth store's
    /// backing directory alive for the duration of the test — dropping it at
    /// the end of each test cleans the directory up rather than leaking it.
    fn ctx_with_issuer(issuer: &str) -> (tempfile::TempDir, Arc<AuthCtx>) {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::open_at(dir.path().join("auth")).unwrap();
        let ctx = Arc::new(AuthCtx::new(store));
        ctx.issuer
            .set(issuer.to_string())
            .expect("issuer set once on a fresh AuthCtx");
        (dir, ctx)
    }

    async fn get_json(router: &Router, path: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                panic!(
                    "response was not JSON ({e}): {}",
                    String::from_utf8_lossy(&bytes)
                )
            })
        };
        (status, json)
    }

    #[tokio::test]
    async fn protected_resource_metadata_has_every_required_field() {
        let (_dir, ctx) = ctx_with_issuer("http://127.0.0.1:7717");
        let router = well_known_router(ctx);
        let (status, body) = get_json(&router, "/.well-known/oauth-protected-resource").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "resource": "http://127.0.0.1:7717/mcp",
                "authorization_servers": ["http://127.0.0.1:7717"],
                "scopes_supported": ["brain"],
                "bearer_methods_supported": ["header"],
            })
        );
    }

    /// RFC 9728 §3.1 path-insertion convention: the SAME document is also
    /// served at the `/mcp`-suffixed well-known path.
    #[tokio::test]
    async fn protected_resource_metadata_mcp_suffixed_variant_matches() {
        let (_dir, ctx) = ctx_with_issuer("http://127.0.0.1:7717");
        let router = well_known_router(ctx);
        let (bare_status, bare_body) =
            get_json(&router, "/.well-known/oauth-protected-resource").await;
        let (suffixed_status, suffixed_body) =
            get_json(&router, "/.well-known/oauth-protected-resource/mcp").await;
        assert_eq!(bare_status, StatusCode::OK);
        assert_eq!(suffixed_status, StatusCode::OK);
        assert_eq!(bare_body, suffixed_body);
    }

    #[tokio::test]
    async fn authorization_server_metadata_has_every_required_field() {
        let (_dir, ctx) = ctx_with_issuer("http://127.0.0.1:7717");
        let router = well_known_router(ctx);
        let (status, body) = get_json(&router, "/.well-known/oauth-authorization-server").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "issuer": "http://127.0.0.1:7717",
                "authorization_endpoint": "http://127.0.0.1:7717/authorize",
                "token_endpoint": "http://127.0.0.1:7717/token",
                "registration_endpoint": "http://127.0.0.1:7717/register",
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"],
                "scopes_supported": ["brain"],
            })
        );
    }

    /// `public_url` (via `AuthCtx.issuer`) must be honored everywhere an
    /// issuer-derived URL is emitted, in BOTH documents — not hardcoded to a
    /// loopback address anywhere in these handlers.
    #[tokio::test]
    async fn a_configured_issuer_is_honored_in_both_documents() {
        let (_dir, ctx) = ctx_with_issuer("https://gw.example.com");
        let router = well_known_router(ctx);

        let (_status, prm) = get_json(&router, "/.well-known/oauth-protected-resource").await;
        assert_eq!(prm["resource"], "https://gw.example.com/mcp");
        assert_eq!(
            prm["authorization_servers"],
            json!(["https://gw.example.com"])
        );

        let (_status, asm) = get_json(&router, "/.well-known/oauth-authorization-server").await;
        assert_eq!(asm["issuer"], "https://gw.example.com");
        assert_eq!(
            asm["authorization_endpoint"],
            "https://gw.example.com/authorize"
        );
        assert_eq!(asm["token_endpoint"], "https://gw.example.com/token");
        assert_eq!(
            asm["registration_endpoint"],
            "https://gw.example.com/register"
        );
    }

    #[tokio::test]
    async fn well_known_router_has_no_auth_gate_of_its_own() {
        // No `Authorization` header at all — every route here must still
        // answer 200. (The full layer-scoping proof — that `build_gateway_router`
        // never routes these through `require_bearer` either — lives in
        // `server::tests`, since it needs the merged router.)
        let (_dir, ctx) = ctx_with_issuer("http://127.0.0.1:7717");
        let router = well_known_router(ctx);
        for path in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-authorization-server",
        ] {
            let (status, _body) = get_json(&router, path).await;
            assert_eq!(status, StatusCode::OK, "{path} must be public");
        }
    }
}
