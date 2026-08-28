//! Shared OAuth server state ([`AuthCtx`]) + the two PUBLIC discovery
//! documents (RFC 9728 §3.1 OAuth Protected Resource Metadata and RFC 8414
//! §3.2 Authorization Server Metadata) + RFC 7591 Dynamic Client
//! Registration (`POST /register`, Task 3, below — public clients only, see
//! [`register_client_handler`]'s doc comment). `/authorize` (Task 4) and
//! `/token` (Task 5) land in this same file next, against this same
//! `AuthCtx`.
//!
//! ## Layer scoping (binding — see `server::build_gateway_router`)
//! Every route this file adds is reachable WITHOUT a bearer token — no
//! [`super::auth::middleware::require_bearer`] layer. A client that has no
//! token yet MUST be able to fetch these documents (and register itself)
//! before it can obtain one (RFC 9728 §5 / RFC 8414 §3 / RFC 7591 §3);
//! gating any of them would be a bootstrapping deadlock. `build_gateway_router`
//! enforces this by construction: it applies the Bearer layer to the `/mcp`
//! nest BEFORE merging this file's routers in, so the layer never wraps
//! them. See
//! `server::tests::well_known_routes_are_reachable_without_auth_while_mcp_stays_gated`
//! and `server::tests::register_is_reachable_without_auth_on_the_real_router`
//! for the end-to-end proof.

use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::auth::{mint_secret_32, now_epoch_secs, AppType, AuthStore, RegisteredClient};

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

// ── RFC 7591 Dynamic Client Registration (Task 3) ──────────────────────────

/// `POST {issuer}/register` request body. Every field is optional at the
/// wire level (`#[serde(default)]`) even though `redirect_uris` is
/// semantically REQUIRED — deserialization failing on a missing field would
/// hand the caller axum's generic JSON-rejection body instead of this
/// handler's RFC 7591 §3.2.2 `{"error": ..., "error_description": ...}`
/// shape, so "required" is enforced by [`register_client_handler`]'s own
/// logic, not by the type. Unknown extra fields (`client_uri`, `contacts`,
/// `logo_uri`, ...) are silently ignored — RFC 7591 defines many optional
/// metadata fields this authorization server simply doesn't use yet.
#[derive(Debug, Deserialize)]
struct RegisterRequest {
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    application_type: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

/// `POST {issuer}/register` success body — the exact RFC 7591 §3.2.1 shape
/// from the task brief. `client_name` is `skip_serializing_if` (omitted
/// entirely, not emitted as `null`) when the caller didn't supply one, to
/// mirror the request field's own optionality. Field declaration order here
/// IS the emitted JSON key order (`serde_json` preserves struct field
/// order), matching the brief verbatim. There is deliberately no
/// `client_secret` field anywhere in this type — this authorization server
/// mints public clients only (see [`register_client_handler`]'s doc
/// comment) and must never emit one.
#[derive(Debug, Serialize)]
struct RegisterResponse {
    client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    redirect_uris: Vec<String>,
    grant_types: [&'static str; 2],
    response_types: [&'static str; 1],
    token_endpoint_auth_method: &'static str,
    application_type: AppType,
}

/// RFC 7591 §3.2.2 error body shape: `{"error": ..., "error_description": ...}`.
#[derive(Debug, Serialize)]
struct OAuthErrorBody {
    error: &'static str,
    error_description: String,
}

/// Build an RFC 7591 §3.2.2-shaped JSON error response. `description` is
/// always either a fixed string or an echo of the CALLER'S OWN submitted
/// (non-secret) `redirect_uri`/`application_type`/`token_endpoint_auth_method`
/// value — never a host path, a store error's raw text, or any credential —
/// per the "no host paths / no secrets in errors" constraint. Shared by
/// every mutating handler this file gains across Tasks 3-5, not just
/// `/register`.
fn oauth_error(status: StatusCode, code: &'static str, description: impl Into<String>) -> Response {
    (
        status,
        Json(OAuthErrorBody {
            error: code,
            error_description: description.into(),
        }),
    )
        .into_response()
}

/// Is `uri` a loopback redirect URI per RFC 8252 §7.3 — `http://localhost`
/// or `http://127.0.0.1`, with an optional `:<port>` and/or `/<path>` after?
///
/// This is a real HOST-BOUNDARY check, not a bare string-prefix match.
/// `"http://localhost"` is also a string PREFIX of
/// `"http://localhost.evil.example/cb"` and `"http://127.0.0.1"` is a prefix
/// of `"http://127.0.0.10/cb"` (a DIFFERENT loopback address in the
/// 127.0.0.0/8 range, not the one this AS accepts) — a naive `starts_with`
/// would wrongly accept both as "localhost"/"127.0.0.1". So the character
/// immediately after the host must be end-of-string, `:` (port), or `/`
/// (path); anything else (a `.` continuing the hostname, a bare digit
/// continuing the IP) is rejected. The port itself is not further
/// validated — the brief states port is ignored at authorize-time matching
/// too, so there is nothing meaningful to check beyond "a `:` follows".
fn is_loopback_redirect_uri(uri: &str) -> bool {
    for host in ["localhost", "127.0.0.1"] {
        let prefix = format!("http://{host}");
        if let Some(rest) = uri.strip_prefix(prefix.as_str()) {
            if rest.is_empty() || rest.starts_with(':') || rest.starts_with('/') {
                return true;
            }
        }
    }
    false
}

/// `POST {issuer}/register` — RFC 7591 Dynamic Client Registration, public
/// clients only (SEP-837 `application_type`). No Bearer gate (see the
/// module docs: a client with no token yet must be able to register to GET
/// one).
///
/// Validation (brief + RFC 7591 §3.2.2). Every `redirect_uris`-shaped
/// problem is reported as `invalid_redirect_uri`; every other metadata
/// problem is `invalid_client_metadata` — a consistent split this handler
/// applies itself (the RFC permits either code for most of these, it does
/// not mandate this exact split):
/// - `redirect_uris` is required and must be non-empty →
///   `invalid_redirect_uri` if missing/empty.
/// - `application_type` (SEP-837): absent defaults to `"web"`. `"web"` →
///   every URI must start with `https://` (host is intentionally
///   unchecked here — exact-match host allowlisting happens later, at
///   `/authorize`, per the brief). `"native"` → every URI must pass
///   [`is_loopback_redirect_uri`]. Any URI that fails its type's rule →
///   `invalid_redirect_uri`. Any `application_type` value other than
///   `"web"`/`"native"` → `invalid_client_metadata` (this AS only knows
///   these two shapes; RFC 8252 native custom-scheme redirects are
///   explicitly out of scope for this PR per the brief).
/// - `token_endpoint_auth_method`, if present, must be exactly `"none"` →
///   `invalid_client_metadata` otherwise. This is a public-client-only
///   authorization server (Task 1/2 design ruling: opaque tokens, no
///   client-secret storage anywhere in [`AuthStore`]) and must never mint
///   or persist a client secret — [`RegisterResponse`] has no
///   `client_secret` field at all, so there is no code path that could
///   emit one even by accident.
///
/// Persists via [`AuthStore::register_client`] with `ctx.store`'s lock held
/// across the ENTIRE call — never cloning `AuthStore` out of the mutex, per
/// `AuthCtx`'s doc comment (binding requirement carried from the Task 1
/// review) and mirroring [`super::auth::middleware::require_bearer`]'s own
/// `check_access` call. `register_client` itself does the full
/// load-clients → insert → save-clients sequence while that single lock
/// acquisition is held, so this handler's one `store.register_client(..)`
/// call already satisfies the "hold across the whole read-modify-write"
/// discipline; there is no separate existence check to add on top, since
/// `client_id` is a freshly `mint_secret_32()`-minted 256-bit value on every
/// call (collision-free in practice) and `register_client` is documented as
/// insert-or-overwrite by `client_id`.
async fn register_client_handler(
    State(ctx): State<Arc<AuthCtx>>,
    Json(req): Json<RegisterRequest>,
) -> Response {
    let app_type = match req.application_type.as_deref() {
        None => AppType::Web,
        Some("web") => AppType::Web,
        Some("native") => AppType::Native,
        Some(other) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                format!("unsupported application_type {other:?} — must be \"web\" or \"native\""),
            );
        }
    };

    if let Some(method) = req.token_endpoint_auth_method.as_deref() {
        if method != "none" {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                "token_endpoint_auth_method must be \"none\" — this authorization server \
                 issues public clients only and never mints a client secret",
            );
        }
    }

    if req.redirect_uris.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect_uris is required and must be a non-empty array",
        );
    }
    for uri in &req.redirect_uris {
        let valid = match app_type {
            AppType::Web => uri.starts_with("https://"),
            AppType::Native => is_loopback_redirect_uri(uri),
        };
        if !valid {
            let msg = match app_type {
                AppType::Web => {
                    format!("redirect_uri must use https:// for a web client: {uri:?}")
                }
                AppType::Native => format!(
                    "redirect_uri must be a loopback http://localhost or http://127.0.0.1 \
                     address for a native client: {uri:?}"
                ),
            };
            return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", msg);
        }
    }

    let client_id = mint_secret_32();
    let registered = RegisteredClient {
        client_id: client_id.clone(),
        client_name: req.client_name.clone(),
        redirect_uris: req.redirect_uris.clone(),
        application_type: app_type,
        created: now_epoch_secs(),
    };

    // Lock held across the FULL store mutation — see the doc comment above.
    let saved = {
        let store = ctx.store.lock().unwrap_or_else(|p| p.into_inner());
        store.register_client(registered)
    };
    if let Err(e) = saved {
        tracing::error!(error = %e, "failed to persist dynamically registered client");
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "failed to persist client registration",
        );
    }

    let body = RegisterResponse {
        client_id,
        client_name: req.client_name,
        redirect_uris: req.redirect_uris,
        grant_types: ["authorization_code", "refresh_token"],
        response_types: ["code"],
        token_endpoint_auth_method: "none",
        application_type: app_type,
    };
    (StatusCode::CREATED, Json(body)).into_response()
}

/// The `POST /register` route as its own small `Router` — mirrors
/// [`well_known_router`]'s shape (state applied here, fully self-contained)
/// so `server::build_gateway_router` can `.merge()` it in without ever
/// routing it through [`super::auth::middleware::require_bearer`]. Kept
/// separate from `well_known_router` rather than folded into it: `/register`
/// isn't a `.well-known/*` discovery document, it's the RFC 7591
/// registration endpoint those documents point at.
pub fn register_router(ctx: Arc<AuthCtx>) -> Router {
    Router::new()
        .route("/register", post(register_client_handler))
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

    /// POST `body` as `application/json` to `path` and parse the response.
    /// Mirrors [`get_json`]'s shape/error-handling exactly; used by the
    /// `/register` tests below.
    async fn post_json(router: &Router, path: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
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

    // ── POST /register (RFC 7591 Dynamic Client Registration, Task 3) ────

    fn register_router_with_issuer(issuer: &str) -> (tempfile::TempDir, Router) {
        let (dir, ctx) = ctx_with_issuer(issuer);
        let router = register_router(ctx);
        (dir, router)
    }

    /// Step 1 happy path: Claude-hosted web registration, `application_type`
    /// omitted (so it must default to `"web"` per SEP-837). Asserts the
    /// EXACT response shape from the brief, including that `client_name` is
    /// fully ABSENT (not `null`) since the request didn't supply one.
    #[tokio::test]
    async fn register_web_client_claude_ai_happy_path_exact_response_shape() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, mut body) = post_json(
            &router,
            "/register",
            json!({
                "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");

        let client_id = body["client_id"]
            .as_str()
            .unwrap_or_else(|| panic!("client_id missing or not a string: {body}"))
            .to_string();
        assert_eq!(
            client_id.len(),
            43,
            "client_id should be a mint_secret_32 value (43 base64url chars): {client_id}"
        );
        assert!(
            client_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "client_id contains a non-base64url char: {client_id}"
        );
        body["client_id"] = Value::Null; // normalize the random id before comparing the rest

        assert_eq!(
            body,
            json!({
                "client_id": null,
                "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
                "application_type": "web",
            }),
            "client_name must be fully ABSENT (not null) when not supplied"
        );
    }

    #[tokio::test]
    async fn register_web_client_with_client_name_echoes_it_back() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "client_name": "Claude",
                "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        assert_eq!(body["client_name"], json!("Claude"));
    }

    /// Step 1: native registration with BOTH accepted loopback hosts
    /// (`localhost` and `127.0.0.1`) plus explicit ports, in one request.
    #[tokio::test]
    async fn register_native_client_localhost_and_127_with_ports_happy_path() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "application_type": "native",
                "redirect_uris": [
                    "http://localhost:8080/callback",
                    "http://127.0.0.1:9999/callback",
                ],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        assert_eq!(body["application_type"], json!("native"));
        assert_eq!(
            body["redirect_uris"],
            json!([
                "http://localhost:8080/callback",
                "http://127.0.0.1:9999/callback"
            ])
        );
    }

    /// A bare loopback URI with no port and no path must also be accepted
    /// (the host-boundary check must treat end-of-string as valid, not just
    /// `:` and `/`).
    #[tokio::test]
    async fn register_native_client_bare_loopback_with_no_port_or_path_is_accepted() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, _body) = post_json(
            &router,
            "/register",
            json!({
                "application_type": "native",
                "redirect_uris": ["http://localhost", "http://127.0.0.1"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn register_rejects_empty_redirect_uris() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(&router, "/register", json!({"redirect_uris": []})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_redirect_uri"));
    }

    #[tokio::test]
    async fn register_rejects_missing_redirect_uris_field() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(&router, "/register", json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_redirect_uri"));
    }

    #[tokio::test]
    async fn register_rejects_plain_http_for_web_client() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({"redirect_uris": ["http://example.com/cb"]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_redirect_uri"));
    }

    /// One valid + one invalid URI in the same request must still reject the
    /// whole registration — no partial acceptance.
    #[tokio::test]
    async fn register_rejects_web_client_when_any_one_uri_is_not_https() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, _body) = post_json(
            &router,
            "/register",
            json!({
                "redirect_uris": [
                    "https://claude.ai/api/mcp/auth_callback",
                    "http://not-https.example/cb",
                ],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Brief: native MAY use https custom schemes per RFC 8252, but that's
    /// explicitly out of scope this PR — a non-loopback URI is rejected for
    /// native regardless of scheme.
    #[tokio::test]
    async fn register_rejects_non_loopback_https_for_native_client() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "application_type": "native",
                "redirect_uris": ["https://evil.example/cb"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_redirect_uri"));
    }

    /// Security-relevant host-boundary case: `"http://localhost.evil.example/cb"`
    /// is a string PREFIX of `"http://localhost"` but a completely different
    /// host — must be rejected, not wrongly accepted by a naive
    /// `starts_with` check.
    #[tokio::test]
    async fn register_rejects_localhost_prefix_confusion_host_for_native_client() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "application_type": "native",
                "redirect_uris": ["http://localhost.evil.example/cb"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_redirect_uri"));
    }

    /// Same host-boundary case for the IP form: `127.0.0.10` is a real,
    /// different loopback address, not `127.0.0.1` — must be rejected.
    #[tokio::test]
    async fn register_rejects_127_0_0_1_prefix_confusion_for_native_client() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "application_type": "native",
                "redirect_uris": ["http://127.0.0.10/cb"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_redirect_uri"));
    }

    #[tokio::test]
    async fn register_rejects_client_secret_auth_method() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
                "token_endpoint_auth_method": "client_secret_basic",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_client_metadata"));
        assert!(
            body["client_secret"].is_null(),
            "a rejected registration must never carry a client_secret: {body}"
        );
    }

    #[tokio::test]
    async fn register_accepts_explicit_none_auth_method() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
                "token_endpoint_auth_method": "none",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        assert_eq!(body["token_endpoint_auth_method"], json!("none"));
    }

    #[tokio::test]
    async fn register_rejects_unknown_application_type() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "application_type": "desktop",
                "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_client_metadata"));
    }

    /// No response — success OR error — from this handler may ever contain a
    /// `client_secret` key. Checked explicitly on the happy path in addition
    /// to the exact-shape assertion above, since this is the one property a
    /// security review will specifically look for.
    #[tokio::test]
    async fn register_success_response_never_includes_a_client_secret() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({"redirect_uris": ["https://claude.ai/api/mcp/auth_callback"]}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(body.get("client_secret").is_none(), "{body}");
    }

    /// The registration must actually persist through `ctx.store` — a
    /// subsequent `get_client` on the SAME store must see it, with the
    /// fields it was registered with.
    #[tokio::test]
    async fn register_persists_client_retrievable_via_get_client() {
        let (_dir, ctx) = ctx_with_issuer("http://127.0.0.1:7717");
        let router = register_router(ctx.clone());
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "client_name": "Claude",
                "application_type": "native",
                "redirect_uris": ["http://localhost/callback"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        let client_id = body["client_id"].as_str().unwrap().to_string();

        let stored = {
            let store = ctx.store.lock().unwrap();
            store.get_client(&client_id).unwrap()
        }
        .unwrap_or_else(|| panic!("client {client_id} was not persisted"));
        assert_eq!(stored.client_id, client_id);
        assert_eq!(stored.client_name, Some("Claude".to_string()));
        assert_eq!(stored.application_type, AppType::Native);
        assert_eq!(stored.redirect_uris, vec!["http://localhost/callback"]);
    }

    #[tokio::test]
    async fn register_router_has_no_auth_gate_of_its_own() {
        // No `Authorization` header at all — a client with no token yet must
        // be able to reach `/register` to obtain one. (The full
        // layer-scoping proof against the real merged router lives in
        // `server::tests::register_is_reachable_without_auth_on_the_real_router`.)
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, _body) = post_json(
            &router,
            "/register",
            json!({"redirect_uris": ["https://claude.ai/api/mcp/auth_callback"]}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
}
