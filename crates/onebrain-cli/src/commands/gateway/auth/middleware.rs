//! Bearer-token resource-server gate for the gateway's `/mcp` surface (RFC
//! 6750 §3-style `WWW-Authenticate` challenge, RFC 9728 `resource_metadata`
//! pointer). Wraps ONLY the `/mcp` nest — see `server::build_gateway_router`
//! for where this layer is (and is NOT) applied; the two `/.well-known/*`
//! discovery documents in `oauth_routes.rs` stay public so a client can
//! bootstrap OAuth before it has a token.
//!
//! Mirrors `server::auth::require_token`'s `from_fn_with_state` shape (the
//! daemon's own token gate), but the credential surface is deliberately
//! narrower: `Authorization: Bearer` ONLY — no `?token=` query, no cookie.
//! This is an API boundary for OAuth clients (ChatGPT/Claude connectors), not
//! a browser page load, so there's no CSRF-relevant "unauthenticated page
//! fetch" case to support here, and accepting a query/cookie credential would
//! just widen the attack surface for no benefit.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::super::oauth_routes::AuthCtx;

/// Identity attached to `req.extensions_mut()` by [`require_bearer`] on a
/// successful check — the only way a downstream handler learns who's
/// calling (there's no session/cookie state in this design). Cheap to clone;
/// carries no secret — the raw token itself never leaves the store lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub client_id: String,
    pub scope: String,
}

/// Axum middleware guarding the `/mcp` nest: requires a live, unrevoked,
/// unexpired ACCESS token presented as `Authorization: Bearer <token>`.
///
/// On success, inserts [`Principal`] into the request's extensions before
/// calling `next` — a downstream handler can read it via the
/// `Extension<Principal>` extractor (none do yet; this task only wires the
/// gate). On failure, returns a bare 401 with an RFC 9728 `WWW-Authenticate`
/// challenge and an EMPTY body — no secret, no path, nothing beyond "no
/// token" vs. "a token was presented and it was bad" (`error="invalid_token"`,
/// standard RFC 6750 §3.1 vocabulary, not new information leakage).
///
/// Binding requirement A (Task 1 review, carried into Task 2): the store
/// lookup holds `ctx.store`'s lock across the full `check_access` call —
/// never cloning `AuthStore` out of the mutex. This call is read-only, but
/// it's the SAME `ctx.store` field Tasks 3-5's mutating `/authorize`/
/// `/token`/`/register` handlers will lock through, so the discipline is
/// established here for every future caller to follow.
pub async fn require_bearer(
    State(ctx): State<Arc<AuthCtx>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(token) = extract_bearer(&req) else {
        return challenge(ctx.issuer(), false);
    };

    let checked = {
        let store = ctx.store.lock().unwrap_or_else(|p| p.into_inner());
        store.check_access(&token)
    };

    match checked {
        Ok(Some(record)) => {
            req.extensions_mut().insert(Principal {
                client_id: record.client_id,
                scope: record.scope,
            });
            next.run(req).await
        }
        // Unknown / wrong-kind / expired / revoked, OR a store I/O error
        // (e.g. a corrupt tokens.json) — both collapse to the SAME
        // "invalid_token" challenge. Distinguishing "your token is bad" from
        // "the server's store is broken" would hand a caller an oracle for
        // probing server-internal state, and there's no different recovery
        // action a client can take either way (get a new token via
        // `/authorize`). The store error itself is not logged here — a
        // future `/authorize`/`/token` handler hitting the same corrupt file
        // will surface it loudly via its own `Result` handling; this gate
        // just needs to fail closed.
        Ok(None) | Err(_) => challenge(ctx.issuer(), true),
    }
}

/// Extract the bearer credential from `Authorization: Bearer <token>` ONLY
/// (RFC 6750 §2.1) — no `?token=` query, no cookie. Deliberately narrower
/// than `server::auth::check_token`'s daemon-page credential surface (see
/// module docs): this is an API boundary for OAuth clients, not a browser
/// page load, so there's no CSRF-relevant safe-verb carve-out to make.
fn extract_bearer(req: &Request) -> Option<String> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// Build the 401 response: empty body, `WWW-Authenticate` naming the RFC
/// 9728 protected-resource-metadata document so a compliant client can
/// discover how to obtain a token. `bad_token_presented` adds
/// `error="invalid_token"` (RFC 6750 §3.1) when the caller DID present a
/// credential and it was rejected, vs. no credential at all.
fn challenge(issuer: &str, bad_token_presented: bool) -> Response {
    let mut value =
        format!(r#"Bearer resource_metadata="{issuer}/.well-known/oauth-protected-resource""#);
    if bad_token_presented {
        value.push_str(r#", error="invalid_token""#);
    }
    let mut resp = StatusCode::UNAUTHORIZED.into_response();
    match HeaderValue::from_str(&value) {
        Ok(hv) => {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, hv);
        }
        Err(e) => {
            // `issuer` comes from local config (`public_url` / the bound
            // loopback address) or the fixed `http://127.0.0.1` fallback —
            // never from the request — so this should be unreachable. Fail
            // closed (still a 401, just without the discovery hint) rather
            // than panic on a malformed config value.
            tracing::warn!(error = %e, "could not build WWW-Authenticate header");
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::Extension;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    use super::super::store::{AuthStore, TokenKind, TokenRecord};

    /// A router with a single `/mcp` route (a trivial "ok" handler, standing
    /// in for the real `nest_service`d rmcp service — the Bearer gate itself
    /// doesn't care what's behind it) gated by [`require_bearer`]. Mirrors
    /// the shape `server::build_gateway_router` wires for real.
    fn test_router(ctx: Arc<AuthCtx>) -> Router {
        Router::new()
            .route("/mcp", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(ctx, require_bearer))
    }

    fn ctx_with_issuer(root: &std::path::Path, issuer: &str) -> Arc<AuthCtx> {
        let store = AuthStore::open_at(root.join("auth")).unwrap();
        let ctx = Arc::new(AuthCtx::new(store));
        ctx.issuer
            .set(issuer.to_string())
            .expect("issuer set once on a fresh AuthCtx");
        ctx
    }

    async fn get_mcp(router: &Router, auth_header: Option<&str>) -> Response {
        let mut builder = Request::builder().method("GET").uri("/mcp");
        if let Some(h) = auth_header {
            builder = builder.header(header::AUTHORIZATION, h);
        }
        let req = builder.body(Body::empty()).unwrap();
        router.clone().oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn no_token_401_with_exact_www_authenticate_and_no_error_param_and_empty_body() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_issuer(dir.path(), "http://127.0.0.1:7717");
        let router = test_router(ctx);

        let resp = get_mcp(&router, None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            www,
            r#"Bearer resource_metadata="http://127.0.0.1:7717/.well-known/oauth-protected-resource""#,
            "no-token 401 must not carry error=\"invalid_token\""
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "401 body must be empty, got {body:?}");
    }

    #[tokio::test]
    async fn non_bearer_authorization_scheme_is_treated_as_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_issuer(dir.path(), "http://127.0.0.1:7717");
        let router = test_router(ctx);

        let resp = get_mcp(&router, Some("Basic dXNlcjpwYXNz")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            !www.contains("invalid_token"),
            "a non-Bearer scheme is not a presented (bad) bearer token: {www}"
        );
    }

    #[tokio::test]
    async fn garbage_token_401_with_invalid_token_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_issuer(dir.path(), "http://127.0.0.1:7717");
        let router = test_router(ctx);

        let resp = get_mcp(&router, Some("Bearer not-a-real-token")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            www,
            r#"Bearer resource_metadata="http://127.0.0.1:7717/.well-known/oauth-protected-resource", error="invalid_token""#
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "401 body must be empty, got {body:?}");
    }

    #[tokio::test]
    async fn valid_access_token_passes_through_and_sets_principal() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::open_at(dir.path().join("auth")).unwrap();
        let (access, _refresh) = store.issue_token_pair("client-1", "brain").unwrap();
        let ctx = Arc::new(AuthCtx::new(store));
        ctx.issuer.set("http://127.0.0.1:7717".to_string()).unwrap();

        async fn handler(Extension(p): Extension<Principal>) -> String {
            format!("{}:{}", p.client_id, p.scope)
        }
        let router =
            Router::new()
                .route("/mcp", get(handler))
                .layer(axum::middleware::from_fn_with_state(
                    ctx.clone(),
                    require_bearer,
                ));

        let resp = get_mcp(&router, Some(&format!("Bearer {}", access.token))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"client-1:brain");
    }

    #[tokio::test]
    async fn a_refresh_token_does_not_pass_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::open_at(dir.path().join("auth")).unwrap();
        let (_access, refresh) = store.issue_token_pair("client-1", "brain").unwrap();
        let ctx = Arc::new(AuthCtx::new(store));
        ctx.issuer.set("http://127.0.0.1:7717".to_string()).unwrap();
        let router = test_router(ctx);

        let resp = get_mcp(&router, Some(&format!("Bearer {}", refresh.token))).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn revoked_access_token_401_invalid_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::open_at(dir.path().join("auth")).unwrap();
        let (access, _refresh) = store.issue_token_pair("client-1", "brain").unwrap();
        store.revoke_token(&access.token).unwrap();
        let ctx = Arc::new(AuthCtx::new(store));
        ctx.issuer.set("http://127.0.0.1:7717".to_string()).unwrap();
        let router = test_router(ctx);

        let resp = get_mcp(&router, Some(&format!("Bearer {}", access.token))).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(www.contains(r#"error="invalid_token""#), "{www}");
    }

    #[tokio::test]
    async fn expired_access_token_401_invalid_token() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("auth");
        let store = AuthStore::open_at(root.clone()).unwrap();
        let (access, _refresh) = store.issue_token_pair("client-1", "brain").unwrap();

        // Directly age the persisted record past expiry (mirrors
        // `store.rs`'s own `check_access_rejects_expired_token` test):
        // `AuthStore` deliberately has no public "expire a token" API, so the
        // test reaches under it via the documented on-disk `tokens.json`
        // layout instead.
        let tokens_path = root.join("tokens.json");
        let mut tokens: std::collections::BTreeMap<String, TokenRecord> =
            serde_json::from_str(&std::fs::read_to_string(&tokens_path).unwrap()).unwrap();
        tokens.get_mut(&access.token).unwrap().expires = 1;
        assert_eq!(tokens[&access.token].kind, TokenKind::Access);
        std::fs::write(&tokens_path, serde_json::to_vec_pretty(&tokens).unwrap()).unwrap();

        let ctx = Arc::new(AuthCtx::new(store));
        ctx.issuer.set("http://127.0.0.1:7717".to_string()).unwrap();
        let router = test_router(ctx);

        let resp = get_mcp(&router, Some(&format!("Bearer {}", access.token))).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(www.contains(r#"error="invalid_token""#), "{www}");
    }

    /// Requirement A (Task 1 review, binding on Task 2): `AuthCtx.store`
    /// MUST be `Mutex<AuthStore>`, with every access holding the lock across
    /// its full operation — never cloned out. This task only adds a READ
    /// (`check_access`) through that lock, so there's no double-spend
    /// scenario to reproduce here yet — that lands with Tasks 3-5's mutating
    /// `/authorize`/`/token`/`/register` routes, which share this SAME
    /// `ctx.store`. What this test proves instead: many concurrent requests
    /// genuinely share ONE `Arc<AuthCtx>` — hence one `Mutex<AuthStore>` —
    /// and all resolve correctly under real multi-threaded contention
    /// (`flavor = "multi_thread"` so the lock is actually contended, not
    /// just cooperatively interleaved on one OS thread).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_check_access_calls_serialize_through_the_shared_mutex() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::open_at(dir.path().join("auth")).unwrap();
        let (access, _refresh) = store.issue_token_pair("client-1", "brain").unwrap();
        let token = access.token.clone();

        let ctx = Arc::new(AuthCtx::new(store));
        ctx.issuer.set("http://127.0.0.1:7717".to_string()).unwrap();
        let router = test_router(ctx);

        let mut handles = Vec::new();
        for _ in 0..20 {
            let router = router.clone();
            let token = token.clone();
            handles.push(tokio::spawn(async move {
                let req = Request::builder()
                    .method("GET")
                    .uri("/mcp")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap();
                router.oneshot(req).await.unwrap().status()
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), StatusCode::OK);
        }
    }
}
