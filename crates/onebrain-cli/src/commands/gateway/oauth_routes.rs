//! Shared OAuth server state ([`AuthCtx`]) + the two PUBLIC discovery
//! documents (RFC 9728 §3.1 OAuth Protected Resource Metadata and RFC 8414
//! §3.2 Authorization Server Metadata) + RFC 7591 Dynamic Client
//! Registration (`POST /register`, Task 3 — public clients only, see
//! [`register_client_handler`]'s doc comment) + the `/authorize` consent
//! flow (Task 4, below — see [`authorize_get_handler`]/
//! [`authorize_post_handler`]'s doc comments). `/token` (Task 5) lands in
//! this same file next, against this same `AuthCtx`.
//!
//! ## `/authorize` — the one human checkpoint (Task 4)
//! This is the ONLY place in the whole OAuth surface where a human, not a
//! client, makes the authorization decision: a GET renders an HTML consent
//! page (client name/scope/redirect host), a POST re-validates the ENTIRE
//! request server-side (hidden form fields are attacker-controlled input,
//! never trusted state) and gates issuing an [`super::auth::AuthCode`]
//! behind the device-pairing code ([`AuthStore::verify_pairing`]) with a
//! rate limiter ([`AttemptState`]). Two binding security properties, proven
//! by the test suite at the bottom of this file:
//! - **Never redirect on an unvalidated `redirect_uri`** (RFC 6749
//!   §4.1.2.1): an unknown `client_id` or an unregistered `redirect_uri`
//!   renders an in-page 400 error, full stop — redirecting to an
//!   attacker-supplied, unvalidated URI would itself BE the open redirect.
//!   Every other validation failure (bad PKCE, bad scope, bad `resource`,
//!   …) redirects with `error=invalid_request&state=…`, because at that
//!   point `redirect_uri` has already been proven to belong to the client.
//! - **Every value interpolated into the consent/error HTML is
//!   [`html_escape`]d** — client_name and state are the two fields a
//!   dynamically-registered client fully controls, so both are exercised by
//!   an explicit XSS regression test.
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

use axum::extract::{Form, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::auth::{mint_secret_32, now_epoch_secs, AppType, AuthStore, RegisteredClient};

/// Pairing-attempt rate-limiter state: 5 consecutive wrong pairing-code
/// submissions → 60s lockout, enforced by [`authorize_post_handler`] through
/// [`AuthCtx::attempts`]. Global (not per-client/per-IP) — there is exactly
/// one pairing code for the whole gateway (a single human at the machine),
/// so one counter is the right shape, matching [`super::auth::store::PairingState`]'s
/// own "exactly one active pairing code" design.
///
/// Both fields are mutated ONLY while holding [`AuthCtx::attempts`]'s lock
/// across the full read-decide-write sequence in `authorize_post_handler` —
/// see that handler's doc comment for the exact lock-ordering discipline
/// (`attempts` locked first, `store` nested inside, never the reverse).
#[derive(Debug, Default)]
pub struct AttemptState {
    /// Consecutive WRONG pairing-code submissions since the last correct
    /// code or the last natural lockout expiry. Reset to 0 on either.
    consecutive_failures: u32,
    /// `Some(epoch_secs)` when a lockout is in effect (the moment it lifts);
    /// `None` when not locked. Set once `consecutive_failures` reaches 5;
    /// cleared (along with the counter) the next time a request observes
    /// `now >= locked_until`.
    locked_until: Option<u64>,
}

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
    /// Pairing-attempt rate-limiter, read and written by
    /// [`authorize_post_handler`] on every `POST /authorize`. See
    /// [`AttemptState`]'s doc comment for the field-level rate-limit
    /// semantics and the lock-ordering discipline shared with `store`.
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
/// immediately after the host must be end-of-string, `:` (port) followed by
/// a WELL-FORMED port, or `/` (path); anything else (a `.` continuing the
/// hostname, a bare digit continuing the IP) is rejected.
///
/// **Fix round 1 (security review, Critical):** a `:`-prefixed remainder
/// used to be accepted outright with no further check on what followed the
/// colon. Per RFC 3986 §3.2 a URI authority is `[userinfo@]host[:port]`, so
/// `http://127.0.0.1:80@evil.com/cb` — `rest` == `":80@evil.com/cb"` —
/// used to pass (`rest.starts_with(':')` is true) even though a real parser
/// reads this as `userinfo = "127.0.0.1:80"`, `host = "evil.com"`: a
/// "native" client could register an attacker-controlled redirect disguised
/// as loopback and exfiltrate the auth code at `/authorize` later. Now a
/// `:` must be followed by ONE OR MORE ASCII digits (an actual port number,
/// still unvalidated in VALUE — ignored at authorize-time matching per the
/// brief, same as before) and then end-of-string or `/` — nothing else, in
/// particular no `@`. `http://127.0.0.1:` (colon, zero digits) is rejected
/// too (`digits > 0` below).
fn is_loopback_redirect_uri(uri: &str) -> bool {
    for host in ["localhost", "127.0.0.1"] {
        let prefix = format!("http://{host}");
        let Some(rest) = uri.strip_prefix(prefix.as_str()) else {
            continue;
        };
        if let Some(port_rest) = rest.strip_prefix(':') {
            let digits = port_rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(port_rest.len());
            let after_port = &port_rest[digits..];
            if digits > 0 && (after_port.is_empty() || after_port.starts_with('/')) {
                return true;
            }
        } else if rest.is_empty() || rest.starts_with('/') {
            return true;
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

// ── /authorize consent flow (Task 4) ───────────────────────────────────────

/// Hand-rolled HTML escaping for the four dangerous characters plus the two
/// quote styles (`& < > " '`) — mirrors this crate's other hand-rolled
/// encoders ([`super::auth::core::base64url_nopad`],
/// `daemon_client::urlencode`) rather than adding a new html-escaping crate
/// dependency. Iterates the ORIGINAL input character-by-character (never
/// re-scanning already-emitted output), so there is no double-escaping
/// hazard from encoding `&` before `<`/`>`/etc. — every replacement is
/// independent. Used on EVERY value interpolated into the consent/error HTML
/// below; see the `xss_*` tests for proof against a `client_name`/`state`
/// payload of `"/><script>alert(1)</script>`.
fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// `GET`/`POST {issuer}/authorize` shared query/form shape. Every field is
/// `Option<String>` (`#[serde(default)]`) — a missing/empty field is a
/// VALIDATION failure this handler reports itself (per-field, with the
/// correct 400-page-vs-redirect split), not a generic extractor rejection.
/// `pairing_code` is POST-only (ignored by the GET path, which never reads
/// it) — kept on the same struct so both `authorize_get_handler` and
/// `authorize_post_handler` can run the identical [`validate_authorize_request`]
/// pipeline over one shape, which is exactly what "the POST handler
/// re-validates EVERYTHING server-side" (binding requirement) needs: hidden
/// form fields are attacker-controlled input, so POST must re-derive
/// `ValidatedAuthorize` from scratch rather than trusting anything the GET
/// response rendered.
#[derive(Debug, Deserialize, Default)]
struct AuthorizeParams {
    #[serde(default)]
    response_type: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    code_challenge: Option<String>,
    #[serde(default)]
    code_challenge_method: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    pairing_code: Option<String>,
}

/// A `/authorize` request whose `client_id` is known AND whose
/// `redirect_uri` is registered for that client — the two RFC 6749
/// §4.1.2.1 preconditions for it ever being safe to redirect the user-agent
/// anywhere. Every field here is a NORMALIZED, validated value (`scope`/
/// `resource` defaulted when absent, `state` empty-string-collapsed to
/// `None`), never the raw caller-supplied string.
struct ValidatedAuthorize {
    client: RegisteredClient,
    redirect_uri: String,
    code_challenge: String,
    resource: String,
    scope: String,
    state: Option<String>,
}

/// Every way [`validate_authorize_request`] can fail, split exactly along
/// the RFC 6749 §4.1.2.1 line the brief draws:
/// - [`Self::NoRedirect`]: `client_id` unknown, or `redirect_uri` missing/not
///   registered for that client. Renders an in-page 400 — redirecting an
///   unvalidated `redirect_uri` would itself be the open-redirect hole.
/// - [`Self::Redirect`]: every OTHER problem (bad `response_type`, bad PKCE,
///   bad `resource`, bad `scope`) — `redirect_uri` is already proven to
///   belong to the client at this point, so the standard OAuth error
///   redirect (`error=invalid_request&state=…`) applies.
enum AuthorizeError {
    NoRedirect(String),
    Redirect {
        redirect_uri: String,
        error: &'static str,
        state: Option<String>,
    },
}

/// PKCE S256 challenge shape (RFC 7636 §4.1/§4.2): base64url (unpadded)
/// alphabet, 43-128 characters. This is a SHAPE check only — the actual
/// verifier match happens at `/token` (Task 5) via
/// [`super::auth::pkce_s256_matches`]; `/authorize` never sees a verifier.
fn is_valid_code_challenge(challenge: &str) -> bool {
    let len = challenge.len();
    (43..=128).contains(&len)
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Does `presented` match one of `client`'s registered `redirect_uris`, per
/// its `application_type`?
/// - `Web`: exact string match (RFC 6749 §3.1.2 exact-match requirement).
/// - `Native`: PORT-INSENSITIVE loopback match (RFC 8252 §7.3 — a native app
///   gets a fresh OS-assigned ephemeral port each run, so pinning the exact
///   port at registration time would make every subsequent `/authorize` fail
///   for a legitimately-reinstalled/relaunched client). `presented` MUST
///   independently pass [`is_loopback_redirect_uri`] — reusing that
///   ALREADY-REVIEWED host-boundary check (never reimplemented here) is what
///   keeps this immune to the same userinfo-confusion shape
///   (`http://127.0.0.1:80@evil.com/cb`) Task 3's registration check was
///   hardened against; only once a URI is proven loopback-shaped does
///   [`loopback_host_and_path`] strip the port for the comparison.
fn redirect_uri_registered(client: &RegisteredClient, presented: &str) -> bool {
    match client.application_type {
        AppType::Web => client.redirect_uris.iter().any(|r| r == presented),
        AppType::Native => {
            if !is_loopback_redirect_uri(presented) {
                return false;
            }
            let presented_parts = loopback_host_and_path(presented);
            client
                .redirect_uris
                .iter()
                .any(|r| loopback_host_and_path(r) == presented_parts)
        }
    }
}

/// Split an ALREADY loopback-validated URI (see [`redirect_uri_registered`]
/// — this is never called on an unvalidated string) into `(host, path)`,
/// with any `:<port>` dropped — the port-insensitive comparison key for
/// native redirect_uri matching. Purely a normalization helper on a value
/// [`is_loopback_redirect_uri`] already proved has one of the exact
/// shapes `http://{localhost,127.0.0.1}[:<digits>](/…)?` — it does not
/// re-decide safety, only where the port digits sit so they can be
/// discarded.
fn loopback_host_and_path(uri: &str) -> Option<(&'static str, &str)> {
    for host in ["localhost", "127.0.0.1"] {
        let prefix = format!("http://{host}");
        if let Some(rest) = uri.strip_prefix(prefix.as_str()) {
            let path = match rest.strip_prefix(':') {
                Some(port_rest) => {
                    let digits = port_rest
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(port_rest.len());
                    &port_rest[digits..]
                }
                None => rest,
            };
            return Some((host, path));
        }
    }
    None
}

/// Display-only `host[:port]` extraction for the consent page (e.g.
/// `claude.ai` or `127.0.0.1:9999`) — NOT a security decision. The actual
/// "is this redirect_uri allowed for this client" check already happened in
/// [`redirect_uri_registered`] before this is ever called; a malformed
/// result here can only make the displayed text look odd, never bypass
/// anything. Always run through [`html_escape`] before interpolation, same
/// as every other value on this page.
fn display_host(uri: &str) -> &str {
    let authority = uri.split("://").nth(1).unwrap_or(uri);
    let end = authority.find(['/', '?', '#']).unwrap_or(authority.len());
    let authority = &authority[..end];
    authority.rsplit('@').next().unwrap_or(authority)
}

/// Re-derive a fully validated [`ValidatedAuthorize`] from `params`, or the
/// specific [`AuthorizeError`] naming why not. Shared verbatim by
/// [`authorize_get_handler`] and [`authorize_post_handler`] — see
/// [`AuthorizeParams`]'s doc comment for why re-running this on the RAW POST
/// body (never trusting a previously-rendered form) is the binding
/// requirement it satisfies.
///
/// Validation order matters: `client_id` and `redirect_uri` are checked
/// FIRST and fail closed with [`AuthorizeError::NoRedirect`] (see
/// [`AuthorizeError`]'s doc comment for the RFC 6749 §4.1.2.1 rationale);
/// every check after that point redirects, because `redirect_uri` is by then
/// proven safe.
fn validate_authorize_request(
    ctx: &AuthCtx,
    params: &AuthorizeParams,
) -> Result<ValidatedAuthorize, AuthorizeError> {
    let client_id = params
        .client_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AuthorizeError::NoRedirect("missing client_id".to_string()))?;

    let client = {
        let store = ctx.store.lock().unwrap_or_else(|p| p.into_inner());
        store.get_client(client_id)
    };
    let client = match client {
        Ok(Some(c)) => c,
        Ok(None) => return Err(AuthorizeError::NoRedirect("unknown client_id".to_string())),
        Err(e) => {
            tracing::error!(error = %e, "client store lookup failed during /authorize");
            return Err(AuthorizeError::NoRedirect(
                "internal error resolving client".to_string(),
            ));
        }
    };

    let redirect_uri = params
        .redirect_uri
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AuthorizeError::NoRedirect("missing redirect_uri".to_string()))?
        .to_string();

    if !redirect_uri_registered(&client, &redirect_uri) {
        return Err(AuthorizeError::NoRedirect(
            "redirect_uri is not registered for this client".to_string(),
        ));
    }

    // `redirect_uri` is proven safe from here on — every further problem
    // redirects there with an error instead of rendering an in-page 400.
    let state = params.state.clone().filter(|s| !s.is_empty());
    let invalid = |error: &'static str| AuthorizeError::Redirect {
        redirect_uri: redirect_uri.clone(),
        error,
        state: state.clone(),
    };

    if params.response_type.as_deref() != Some("code") {
        return Err(invalid("invalid_request"));
    }

    // PKCE: S256 only — `plain` and an absent method are both rejected
    // (OAuth 2.1 drops `plain` support entirely).
    if params.code_challenge_method.as_deref() != Some("S256") {
        return Err(invalid("invalid_request"));
    }
    let code_challenge = match params.code_challenge.as_deref() {
        Some(c) if is_valid_code_challenge(c) => c.to_string(),
        _ => return Err(invalid("invalid_request")),
    };

    // RFC 8707 `resource`: absent → this gateway's own `/mcp` resource;
    // present → must equal it exactly (this AS mints tokens for exactly one
    // resource, so there is nothing else a caller could legitimately name).
    let issuer = ctx.issuer();
    let expected_resource = format!("{issuer}/mcp");
    let resource = match params.resource.as_deref() {
        None => expected_resource,
        Some(r) if r == expected_resource => r.to_string(),
        Some(_) => return Err(invalid("invalid_request")),
    };

    // scope ⊆ {"brain"} — the only scope this gateway knows; absent/empty
    // defaults to it.
    let scope = match params.scope.as_deref() {
        None | Some("") | Some("brain") => "brain".to_string(),
        Some(_) => return Err(invalid("invalid_request")),
    };

    Ok(ValidatedAuthorize {
        client,
        redirect_uri,
        code_challenge,
        resource,
        scope,
        state,
    })
}

/// RFC 3986 §2.3 "unreserved" characters, carved out of
/// [`NON_ALPHANUMERIC`]'s otherwise-encode-everything set. Without this,
/// `percent_encode_value` would encode `-`/`_` too — which matters here
/// specifically because [`super::auth::core::base64url_nopad`]'s alphabet
/// (used for every auth code, token, and `client_id` this store mints) is
/// exactly `[A-Za-z0-9-_]`: an auth `code` value containing `-`/`_` must
/// come through this encoder byte-for-byte unchanged, or a client (or this
/// file's own tests) naively slicing `code=` out of the query string would
/// grab a percent-encoded value that doesn't match the raw one the store
/// indexed it under.
const QUERY_VALUE_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Percent-encode one query-VALUE (never a full URL) via the ALREADY-a-
/// dependency `percent_encoding` crate (see `server/translate.rs`'s own use
/// of it) — not a new crate dependency, and safer than hand-rolling yet
/// another encoder for a security-sensitive redirect URL: `state`/`iss` are
/// echoed into a `Location` header, and an under-encoded value (a raw `&` or
/// `#`) could inject an extra query parameter or truncate the URL.
fn percent_encode_value(value: &str) -> String {
    utf8_percent_encode(value, QUERY_VALUE_ENCODE_SET).to_string()
}

/// Append `pairs` as a query string onto `base`, percent-encoding every
/// value. Uses `&` to extend an existing query, `?` to start a fresh one —
/// none of this AS's registered `redirect_uri`s carry a query today, but a
/// client's registered URI legally could (RFC 6749 §3.1.2 permits it), so
/// this must not assume `base` is bare.
fn append_query(base: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = base.to_string();
    let mut sep = if base.contains('?') { '&' } else { '?' };
    for (key, value) in pairs {
        out.push(sep);
        out.push_str(key);
        out.push('=');
        out.push_str(&percent_encode_value(value));
        sep = '&';
    }
    out
}

/// Wrap an HTML string in a response with the given status. Mirrors
/// `server/static.rs`'s own `html_response` (private to that module, hence
/// not reused directly) — same `text/html; charset=utf-8` content type.
fn html_response(status: StatusCode, html: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

/// The in-page error the brief calls "400 error page, NEVER redirect" —
/// used ONLY for the two RFC 6749 §4.1.2.1 preconditions (unknown
/// `client_id`, unregistered `redirect_uri`) and for genuine server errors.
/// `message` is always a FIXED, developer-authored string from this file —
/// never raw caller input — but is still run through [`html_escape`] as
/// defense in depth (matching every other interpolation on this page)
/// rather than relying on that invariant holding forever.
fn error_page(status: StatusCode, message: &str) -> Response {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>Authorization error</title></head><body>\
         <h1>Authorization error</h1><p>{msg}</p></body></html>",
        msg = html_escape(message)
    );
    html_response(status, html)
}

/// Build a `302 Found` to `location`, or an [`error_page`] if `location`
/// somehow isn't a valid header value. Mirrors
/// [`super::auth::middleware::challenge`]'s explicit, fail-closed
/// `HeaderValue::from_str` pattern rather than trusting the tuple/array
/// `IntoResponse` sugar to handle a conversion failure the same way.
fn redirect_response(location: String) -> Response {
    match HeaderValue::from_str(&location) {
        Ok(hv) => {
            let mut resp = StatusCode::FOUND.into_response();
            resp.headers_mut().insert(header::LOCATION, hv);
            resp
        }
        Err(e) => {
            tracing::error!(error = %e, "could not build /authorize redirect Location header");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build redirect",
            )
        }
    }
}

/// The RFC 6749 §4.1.2.1 error redirect: `redirect_uri?error=…&state=…`
/// (`state` omitted entirely when the original request didn't send one,
/// rather than emitting `state=`).
fn redirect_with_error(redirect_uri: &str, error: &str, state: Option<&str>) -> Response {
    let mut pairs: Vec<(&str, &str)> = vec![("error", error)];
    if let Some(s) = state {
        pairs.push(("state", s));
    }
    redirect_response(append_query(redirect_uri, &pairs))
}

/// The RFC 6749 §4.1.2 + RFC 9207 §2.1 success redirect:
/// `redirect_uri?code=…[&state=…]&iss={issuer}`.
fn redirect_with_code(ctx: &AuthCtx, req: &ValidatedAuthorize, code: &str) -> Response {
    let mut pairs: Vec<(&str, &str)> = vec![("code", code)];
    if let Some(s) = req.state.as_deref() {
        pairs.push(("state", s));
    }
    let issuer = ctx.issuer();
    pairs.push(("iss", issuer));
    redirect_response(append_query(&req.redirect_uri, &pairs))
}

/// Render the consent form: client name (untrusted — a DCR client sets this
/// itself) + the redirect_uri's HOST (the one value that's actually anchored
/// to a validated redirect target — see the module-level CIMD/Claude
/// guidance note) + scope, one `pairing_code` text input, and hidden fields
/// carrying the whole VALIDATED (not raw-caller) request so a POST can
/// re-derive and re-check everything from scratch. `notice`, when present,
/// is a generic status line (wrong code / locked out) — see
/// [`authorize_post_handler`] for why it never names a specific reason.
///
/// EVERY interpolated value is [`html_escape`]d — `client_name` and `state`
/// are the two a caller fully controls (client_name via `/register`'s
/// `client_name` field, state via this very request), so both are covered
/// by the `xss_*` regression tests below.
fn render_consent_form(req: &ValidatedAuthorize, notice: Option<&str>) -> Response {
    let client_name = req
        .client
        .client_name
        .as_deref()
        .unwrap_or("(unnamed application)");
    let notice_html = match notice {
        Some(msg) => format!("<p class=\"notice\">{}</p>", html_escape(msg)),
        None => String::new(),
    };
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>Authorize access</title></head><body>\
         <h1>Authorize {client_name}</h1>\
         <p>This application is requesting access to: <strong>{scope}</strong></p>\
         <p>It will redirect to: <code>{host}</code></p>\
         {notice_html}\
         <form method=\"post\" action=\"/authorize\">\
         <input type=\"hidden\" name=\"response_type\" value=\"code\">\
         <input type=\"hidden\" name=\"client_id\" value=\"{client_id}\">\
         <input type=\"hidden\" name=\"redirect_uri\" value=\"{redirect_uri}\">\
         <input type=\"hidden\" name=\"code_challenge\" value=\"{code_challenge}\">\
         <input type=\"hidden\" name=\"code_challenge_method\" value=\"S256\">\
         <input type=\"hidden\" name=\"resource\" value=\"{resource}\">\
         <input type=\"hidden\" name=\"scope\" value=\"{scope}\">\
         <input type=\"hidden\" name=\"state\" value=\"{state}\">\
         <label for=\"pairing_code\">Pairing code (shown on your gateway)</label>\
         <input type=\"text\" id=\"pairing_code\" name=\"pairing_code\" autocomplete=\"off\">\
         <button type=\"submit\">Authorize</button>\
         </form></body></html>",
        client_name = html_escape(client_name),
        scope = html_escape(&req.scope),
        host = html_escape(display_host(&req.redirect_uri)),
        notice_html = notice_html,
        client_id = html_escape(&req.client.client_id),
        redirect_uri = html_escape(&req.redirect_uri),
        code_challenge = html_escape(&req.code_challenge),
        resource = html_escape(&req.resource),
        state = html_escape(req.state.as_deref().unwrap_or("")),
    );
    html_response(StatusCode::OK, html)
}

/// Generic failure lines — deliberately uninformative. Neither names which
/// field was wrong, how many attempts remain, nor when a lockout lifts (the
/// brief's "do not leak remaining-attempts or unlock-time precisely").
const WRONG_PAIRING_CODE_MESSAGE: &str = "Incorrect pairing code. Please try again.";
const LOCKED_OUT_MESSAGE: &str = "Too many attempts. Please try again in a minute.";

/// `GET {issuer}/authorize` — validates the request and renders the consent
/// form, or fails per [`AuthorizeError`]'s NoRedirect/Redirect split. Never
/// touches `ctx.attempts` — the pairing rate limiter only applies to actual
/// pairing-code SUBMISSIONS (`POST`).
async fn authorize_get_handler(
    State(ctx): State<Arc<AuthCtx>>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    match validate_authorize_request(&ctx, &params) {
        Ok(validated) => render_consent_form(&validated, None),
        Err(AuthorizeError::NoRedirect(msg)) => error_page(StatusCode::BAD_REQUEST, &msg),
        Err(AuthorizeError::Redirect {
            redirect_uri,
            error,
            state,
        }) => redirect_with_error(&redirect_uri, error, state.as_deref()),
    }
}

/// `POST {issuer}/authorize` (form-urlencoded) — the pairing gate.
///
/// Step 1: re-run [`validate_authorize_request`] over the RAW posted form —
/// hidden fields are attacker-controlled input, so this NEVER trusts that a
/// GET rendered them; a tampered `client_id`/`redirect_uri` fails exactly
/// like a fresh GET would (400 page, no redirect, for the two
/// RFC-6749-critical fields; redirect-with-error for everything else).
///
/// Step 2: the rate-limited pairing check. Lock ORDER is `attempts` first,
/// `store` nested inside — held as ONE continuous critical section across
/// "check lockout → verify pairing code → record the result", which is what
/// makes "5 consecutive failures → 60s lockout, and a CORRECT code inside
/// that window still gets the generic locked response" true even under
/// concurrent POSTs (two racing wrong submissions can't both slip in under
/// the threshold, and a correct-code submission that arrives while locked
/// can't observe a lock that a concurrently-expiring window just cleared out
/// from under it). `store` is never held while a second `attempts` lock is
/// taken elsewhere, so this ordering can't deadlock against any other code
/// path in this file.
///
/// Step 3 (success only): [`AuthStore::issue_code`] — a fresh, single-use
/// code bound to client_id+redirect_uri+code_challenge+resource+scope,
/// 60s(-class) TTL — then a 302 with `code`/`state`/`iss`.
async fn authorize_post_handler(
    State(ctx): State<Arc<AuthCtx>>,
    Form(params): Form<AuthorizeParams>,
) -> Response {
    let validated = match validate_authorize_request(&ctx, &params) {
        Ok(v) => v,
        Err(AuthorizeError::NoRedirect(msg)) => {
            return error_page(StatusCode::BAD_REQUEST, &msg);
        }
        Err(AuthorizeError::Redirect {
            redirect_uri,
            error,
            state,
        }) => return redirect_with_error(&redirect_uri, error, state.as_deref()),
    };

    let now = now_epoch_secs();
    let mut attempts = ctx.attempts.lock().unwrap_or_else(|p| p.into_inner());

    if let Some(until) = attempts.locked_until {
        if now < until {
            drop(attempts);
            return render_consent_form(&validated, Some(LOCKED_OUT_MESSAGE));
        }
        // Lockout window elapsed naturally — clear it before proceeding.
        attempts.locked_until = None;
        attempts.consecutive_failures = 0;
    }

    let pairing_code = params.pairing_code.as_deref().unwrap_or("");
    let pairing_ok = {
        let store = ctx.store.lock().unwrap_or_else(|p| p.into_inner());
        store.verify_pairing(pairing_code)
    };
    let pairing_ok = match pairing_ok {
        Ok(ok) => ok,
        Err(e) => {
            drop(attempts);
            tracing::error!(error = %e, "pairing store I/O error during /authorize");
            return error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error verifying pairing code",
            );
        }
    };

    if !pairing_ok {
        attempts.consecutive_failures += 1;
        let locked_now = attempts.consecutive_failures >= 5;
        if locked_now {
            attempts.locked_until = Some(now_epoch_secs() + 60);
        }
        drop(attempts);
        let message = if locked_now {
            LOCKED_OUT_MESSAGE
        } else {
            WRONG_PAIRING_CODE_MESSAGE
        };
        return render_consent_form(&validated, Some(message));
    }

    // Correct code: clear the limiter and mint a fresh auth code.
    attempts.consecutive_failures = 0;
    attempts.locked_until = None;
    drop(attempts);

    let issued = {
        let store = ctx.store.lock().unwrap_or_else(|p| p.into_inner());
        store.issue_code(
            &validated.client.client_id,
            &validated.redirect_uri,
            &validated.code_challenge,
            &validated.resource,
            &validated.scope,
        )
    };
    match issued {
        Ok(auth_code) => redirect_with_code(&ctx, &validated, &auth_code.code),
        Err(e) => {
            tracing::error!(error = %e, "failed to persist minted authorization code");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to issue authorization code",
            )
        }
    }
}

/// The `/authorize` GET+POST route as its own small `Router` — mirrors
/// [`register_router`]'s shape (state applied here, fully self-contained) so
/// `server::build_gateway_router` can `.merge()` it in without ever routing
/// it through [`super::auth::middleware::require_bearer`]: a client with no
/// token yet must be able to complete the authorization flow to GET one.
pub fn authorize_router(ctx: Arc<AuthCtx>) -> Router {
    Router::new()
        .route(
            "/authorize",
            get(authorize_get_handler).post(authorize_post_handler),
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

    /// Fix round 1 (security review, Critical) — userinfo-host confusion:
    /// `http://127.0.0.1:80@evil.com/cb` is NOT a loopback redirect, even
    /// though the string starts with `http://127.0.0.1:`. Per RFC 3986 §3.2
    /// (`[userinfo@]host[:port]`) a real parser reads this as
    /// `userinfo = "127.0.0.1:80"`, `host = "evil.com"` — the actual
    /// destination is attacker-controlled. Must be rejected.
    #[tokio::test]
    async fn register_rejects_127_0_0_1_userinfo_confusion_for_native_client() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "application_type": "native",
                "redirect_uris": ["http://127.0.0.1:80@evil.com/cb"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_redirect_uri"));
    }

    /// Same userinfo-confusion shape for the `localhost` host form.
    #[tokio::test]
    async fn register_rejects_localhost_userinfo_confusion_for_native_client() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "application_type": "native",
                "redirect_uris": ["http://localhost:8080@evil.com/cb"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_redirect_uri"));
    }

    /// Fix round 1 edge case: a `:` with zero digits after it (no port at
    /// all, just a bare trailing colon) must also be rejected — it's not a
    /// well-formed port, and accepting it would leave the door open to
    /// "empty port, then something" shapes.
    #[tokio::test]
    async fn register_rejects_bare_trailing_colon_with_no_port_for_native_client() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "application_type": "native",
                "redirect_uris": ["http://127.0.0.1:"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], json!("invalid_redirect_uri"));
    }

    /// The happy-path loopback shapes must still all pass after the fix —
    /// a regression guard that the stricter port parsing didn't collaterally
    /// reject well-formed URIs.
    #[tokio::test]
    async fn register_accepts_well_formed_loopback_shapes_after_userinfo_confusion_fix() {
        let (_dir, router) = register_router_with_issuer("http://127.0.0.1:7717");
        let (status, body) = post_json(
            &router,
            "/register",
            json!({
                "application_type": "native",
                "redirect_uris": [
                    "http://127.0.0.1:9999/callback",
                    "http://localhost/callback",
                    "http://127.0.0.1",
                ],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
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

    // ── html_escape (Task 4) ─────────────────────────────────────────────

    #[test]
    fn html_escape_escapes_all_five_dangerous_chars() {
        assert_eq!(html_escape(r#"&<>"'"#), "&amp;&lt;&gt;&quot;&#x27;");
    }

    #[test]
    fn html_escape_leaves_plain_text_untouched() {
        assert_eq!(html_escape("Claude Desktop 1.0"), "Claude Desktop 1.0");
    }

    // ── is_valid_code_challenge / redirect_uri_registered / display_host ──

    #[test]
    fn is_valid_code_challenge_accepts_43_to_128_base64url_len() {
        assert!(is_valid_code_challenge(&"A".repeat(43)));
        assert!(is_valid_code_challenge(&"A".repeat(128)));
        assert!(!is_valid_code_challenge(&"A".repeat(42)));
        assert!(!is_valid_code_challenge(&"A".repeat(129)));
    }

    #[test]
    fn is_valid_code_challenge_rejects_non_base64url_char() {
        let mut s = "A".repeat(42);
        s.push('!'); // 43 chars total, one invalid char
        assert!(!is_valid_code_challenge(&s));
    }

    #[test]
    fn redirect_uri_registered_web_requires_exact_string_match() {
        let client = RegisteredClient {
            client_id: "c1".to_string(),
            client_name: None,
            redirect_uris: vec!["https://claude.ai/cb".to_string()],
            application_type: AppType::Web,
            created: 0,
        };
        assert!(redirect_uri_registered(&client, "https://claude.ai/cb"));
        assert!(!redirect_uri_registered(&client, "https://claude.ai/cb2"));
        assert!(!redirect_uri_registered(
            &client,
            "https://claude.ai:443/cb"
        ));
    }

    /// Also proves the port-insensitive native path doesn't reopen the
    /// userinfo-confusion hole Task 3 closed in `is_loopback_redirect_uri` —
    /// `redirect_uri_registered` reuses that check as its gate.
    #[test]
    fn redirect_uri_registered_native_is_port_insensitive_but_host_and_path_exact() {
        let client = RegisteredClient {
            client_id: "c1".to_string(),
            client_name: None,
            redirect_uris: vec!["http://127.0.0.1:9999/callback".to_string()],
            application_type: AppType::Native,
            created: 0,
        };
        assert!(redirect_uri_registered(
            &client,
            "http://127.0.0.1:1234/callback"
        ));
        assert!(redirect_uri_registered(
            &client,
            "http://127.0.0.1/callback"
        ));
        assert!(!redirect_uri_registered(
            &client,
            "http://127.0.0.1:1234/other"
        ));
        assert!(!redirect_uri_registered(
            &client,
            "http://localhost:1234/callback"
        ));
        assert!(!redirect_uri_registered(
            &client,
            "http://127.0.0.1:80@evil.com/callback"
        ));
    }

    #[test]
    fn display_host_extracts_authority_without_scheme_path_or_userinfo() {
        assert_eq!(
            display_host("https://claude.ai/api/mcp/auth_callback"),
            "claude.ai"
        );
        assert_eq!(
            display_host("http://127.0.0.1:9999/callback"),
            "127.0.0.1:9999"
        );
        assert_eq!(
            display_host("https://user:pass@example.com/cb"),
            "example.com"
        );
    }

    // ── GET /authorize + POST /authorize (Task 4) ──────────────────────────

    /// RFC 7636 Appendix B's `code_challenge` vector — a real, valid-shaped
    /// 43-char base64url value. `/authorize` never verifies it against a
    /// verifier (that's `/token`, Task 5); it only checks the SHAPE, so any
    /// well-formed value works here.
    const CODE_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    /// The brief's exact XSS probe: breaks out of a `value="…"` attribute
    /// (leading `"` then `/>`) and injects a `<script>` tag.
    const XSS_PAYLOAD: &str = "\"/><script>alert(1)</script>";

    fn authorize_fixture(issuer: &str) -> (tempfile::TempDir, Arc<AuthCtx>, Router) {
        let (dir, ctx) = ctx_with_issuer(issuer);
        let router = authorize_router(ctx.clone());
        (dir, ctx, router)
    }

    fn register_web_client(ctx: &AuthCtx, name: Option<&str>, redirect_uri: &str) -> String {
        let client_id = mint_secret_32();
        let client = RegisteredClient {
            client_id: client_id.clone(),
            client_name: name.map(str::to_string),
            redirect_uris: vec![redirect_uri.to_string()],
            application_type: AppType::Web,
            created: now_epoch_secs(),
        };
        ctx.store.lock().unwrap().register_client(client).unwrap();
        client_id
    }

    fn register_native_client(ctx: &AuthCtx, redirect_uris: &[&str]) -> String {
        let client_id = mint_secret_32();
        let client = RegisteredClient {
            client_id: client_id.clone(),
            client_name: Some("Native Test Client".to_string()),
            redirect_uris: redirect_uris.iter().map(|s| s.to_string()).collect(),
            application_type: AppType::Native,
            created: now_epoch_secs(),
        };
        ctx.store.lock().unwrap().register_client(client).unwrap();
        client_id
    }

    fn build_query(pairs: &[(&str, &str)]) -> String {
        pairs
            .iter()
            .map(|(k, v)| format!("{k}={}", percent_encode_value(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// The standard valid GET/POST param set for a given client/redirect —
    /// `resource`/`scope` deliberately omitted so every test that starts
    /// from this also exercises their "absent → defaulted" paths.
    fn valid_params<'a>(
        client_id: &'a str,
        redirect_uri: &'a str,
        state: &'a str,
    ) -> Vec<(&'a str, &'a str)> {
        vec![
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("code_challenge", CODE_CHALLENGE),
            ("code_challenge_method", "S256"),
            ("state", state),
        ]
    }

    async fn get_authorize(router: &Router, pairs: &[(&str, &str)]) -> Response {
        let req = Request::builder()
            .method("GET")
            .uri(format!("/authorize?{}", build_query(pairs)))
            .body(Body::empty())
            .unwrap();
        router.clone().oneshot(req).await.unwrap()
    }

    async fn post_authorize(router: &Router, pairs: &[(&str, &str)]) -> Response {
        let req = Request::builder()
            .method("POST")
            .uri("/authorize")
            .header(
                axum::http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(Body::from(build_query(pairs)))
            .unwrap();
        router.clone().oneshot(req).await.unwrap()
    }

    async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    // ── Step 1: GET validation matrix ──────────────────────────────────────

    #[tokio::test]
    async fn get_authorize_unknown_client_id_renders_400_error_page_never_redirects() {
        let (_dir, _ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let params = valid_params("does-not-exist", "https://claude.ai/cb", "s1");
        let resp = get_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(
            resp.headers().get(header::LOCATION).is_none(),
            "must never redirect on an unknown client_id"
        );
    }

    #[tokio::test]
    async fn get_authorize_mismatched_redirect_uri_renders_400_error_page_never_redirects() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        let params = valid_params(&client_id, "https://evil.example/cb", "s1");
        let resp = get_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(
            resp.headers().get(header::LOCATION).is_none(),
            "must never redirect on an unregistered redirect_uri — that IS the open redirect"
        );
    }

    #[tokio::test]
    async fn get_authorize_native_client_port_insensitive_redirect_uri_is_accepted() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_native_client(&ctx, &["http://127.0.0.1:9999/callback"]);
        // Different port than registered — must still be accepted.
        let params = valid_params(&client_id, "http://127.0.0.1:5555/callback", "s1");
        let resp = get_authorize(&router, &params).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "port-insensitive match must render the form"
        );
    }

    #[tokio::test]
    async fn get_authorize_plain_code_challenge_method_is_rejected_via_redirect() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        let mut params = valid_params(&client_id, "https://claude.ai/cb", "s1");
        // Overwrite code_challenge_method with "plain".
        for pair in params.iter_mut() {
            if pair.0 == "code_challenge_method" {
                pair.1 = "plain";
            }
        }
        let resp = get_authorize(&router, &params).await;
        assert_eq!(
            resp.status(),
            StatusCode::FOUND,
            "must redirect (redirect_uri already valid)"
        );
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.starts_with("https://claude.ai/cb?"), "{location}");
        assert!(location.contains("error=invalid_request"), "{location}");
        assert!(location.contains("state=s1"), "{location}");
    }

    #[tokio::test]
    async fn get_authorize_missing_code_challenge_is_rejected_via_redirect() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        let params: Vec<(&str, &str)> = vec![
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", "https://claude.ai/cb"),
            ("code_challenge_method", "S256"),
            ("state", "s1"),
        ];
        let resp = get_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.contains("error=invalid_request"), "{location}");
    }

    #[tokio::test]
    async fn get_authorize_wrong_resource_is_rejected_via_redirect() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        let mut params = valid_params(&client_id, "https://claude.ai/cb", "s1");
        params.push(("resource", "http://evil.example/mcp"));
        let resp = get_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.contains("error=invalid_request"), "{location}");
    }

    #[tokio::test]
    async fn get_authorize_happy_path_renders_form_with_host_and_hidden_state() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(
            &ctx,
            Some("Claude"),
            "https://claude.ai/api/mcp/auth_callback",
        );
        let params = valid_params(
            &client_id,
            "https://claude.ai/api/mcp/auth_callback",
            "opaque-state-123",
        );
        let resp = get_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("claude.ai"), "{body}");
        assert!(body.contains(r#"name="pairing_code""#), "{body}");
        assert!(
            body.contains(r#"value="opaque-state-123""#),
            "hidden state field missing: {body}"
        );
        assert!(body.contains("Claude"), "{body}");
    }

    /// SECURITY (binding): a `client_name`/`state` of
    /// `"/><script>alert(1)</script>` must appear ONLY in escaped form; the
    /// raw payload must be entirely absent from the response body.
    #[tokio::test]
    async fn get_authorize_xss_in_client_name_and_state_is_escaped_raw_payload_absent() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, Some(XSS_PAYLOAD), "https://claude.ai/cb");
        let params = valid_params(&client_id, "https://claude.ai/cb", XSS_PAYLOAD);
        let resp = get_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;

        assert!(
            !body.contains("<script>alert(1)</script>"),
            "raw XSS payload leaked unescaped into the response: {body}"
        );
        assert!(
            !body.contains(r#""/><script>"#),
            "raw attribute-breakout payload leaked unescaped: {body}"
        );
        assert!(
            body.contains("&quot;/&gt;&lt;script&gt;alert(1)&lt;/script&gt;"),
            "escaped payload must be present (proves it round-tripped through html_escape, \
             not simply omitted): {body}"
        );
    }

    // ── Step 2: POST /authorize (the pairing gate) ──────────────────────────

    #[tokio::test]
    async fn post_authorize_wrong_pairing_code_rerenders_form_and_mints_no_code() {
        let (dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        ctx.store.lock().unwrap().pairing_code().unwrap(); // ensure one exists

        let mut params = valid_params(&client_id, "https://claude.ai/cb", "s1");
        params.push(("pairing_code", "WRONG-CODE"));
        let resp = post_authorize(&router, &params).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "must re-render, not redirect"
        );
        assert!(resp.headers().get(header::LOCATION).is_none());
        let body = body_text(resp).await;
        assert!(body.contains(WRONG_PAIRING_CODE_MESSAGE), "{body}");

        let codes_path = dir.path().join("auth").join("codes.json");
        if codes_path.exists() {
            let contents = std::fs::read_to_string(&codes_path).unwrap();
            let map: std::collections::BTreeMap<String, Value> =
                serde_json::from_str(&contents).unwrap();
            assert!(
                map.is_empty(),
                "no AuthCode may be minted on a wrong pairing code: {contents}"
            );
        }
    }

    #[tokio::test]
    async fn post_authorize_five_wrong_codes_lock_out_even_a_subsequently_correct_code() {
        let (dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        let real_code = ctx.store.lock().unwrap().pairing_code().unwrap();

        let base = valid_params(&client_id, "https://claude.ai/cb", "s1");

        for attempt in 1..=5 {
            let mut params = base.clone();
            params.push(("pairing_code", "WRONG-CODE"));
            let resp = post_authorize(&router, &params).await;
            assert_eq!(resp.status(), StatusCode::OK, "attempt {attempt}");
            let body = body_text(resp).await;
            if attempt < 5 {
                assert!(
                    body.contains(WRONG_PAIRING_CODE_MESSAGE),
                    "attempt {attempt} should be a plain wrong-code message: {body}"
                );
            } else {
                assert!(
                    body.contains(LOCKED_OUT_MESSAGE),
                    "5th consecutive failure must trip the lockout: {body}"
                );
            }
        }

        // The 6th submission, even with the CORRECT pairing code, must still
        // be rejected — the lockout takes precedence over checking the code
        // at all.
        let mut params = base.clone();
        params.push(("pairing_code", real_code.as_str()));
        let resp = post_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get(header::LOCATION).is_none());
        let body = body_text(resp).await;
        assert!(
            body.contains(LOCKED_OUT_MESSAGE),
            "a correct code inside the lockout window must still be rejected: {body}"
        );

        let codes_path = dir.path().join("auth").join("codes.json");
        if codes_path.exists() {
            let contents = std::fs::read_to_string(&codes_path).unwrap();
            let map: std::collections::BTreeMap<String, Value> =
                serde_json::from_str(&contents).unwrap();
            assert!(
                map.is_empty(),
                "lockout must prevent minting even with the right code"
            );
        }
    }

    /// Directly ages `ctx.attempts.locked_until` into the past (rather than
    /// sleeping out a real 60s window) and proves a subsequent correct code
    /// succeeds — the natural-expiry reset path.
    #[tokio::test]
    async fn post_authorize_lockout_clears_naturally_after_it_elapses() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        let real_code = ctx.store.lock().unwrap().pairing_code().unwrap();

        {
            let mut attempts = ctx.attempts.lock().unwrap();
            attempts.consecutive_failures = 5;
            attempts.locked_until = Some(now_epoch_secs().saturating_sub(1));
        }

        let mut params = valid_params(&client_id, "https://claude.ai/cb", "s1");
        params.push(("pairing_code", real_code.as_str()));
        let resp = post_authorize(&router, &params).await;
        assert_eq!(
            resp.status(),
            StatusCode::FOUND,
            "an elapsed lockout must not block a correct code"
        );
    }

    #[tokio::test]
    async fn post_authorize_correct_code_mints_bound_auth_code_and_redirects_with_iss() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        let real_code = ctx.store.lock().unwrap().pairing_code().unwrap();

        let mut params = valid_params(&client_id, "https://claude.ai/cb", "s1");
        params.push(("pairing_code", real_code.as_str()));
        let resp = post_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.starts_with("https://claude.ai/cb?"), "{location}");
        assert!(location.contains("state=s1"), "{location}");
        assert!(
            location.contains("iss=http%3A%2F%2F127.0.0.1%3A7717"),
            "{location}"
        );

        let code = location
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();

        let issued = ctx.store.lock().unwrap().consume_code(&code).unwrap();
        let issued = issued.unwrap_or_else(|| panic!("no AuthCode minted for code in {location}"));
        assert_eq!(issued.client_id, client_id);
        assert_eq!(issued.redirect_uri, "https://claude.ai/cb");
        assert_eq!(issued.code_challenge, CODE_CHALLENGE);
        assert_eq!(issued.resource, "http://127.0.0.1:7717/mcp");
        assert_eq!(issued.scope, "brain");
    }

    #[tokio::test]
    async fn post_authorize_replayed_success_mints_a_second_independent_auth_code() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        let real_code = ctx.store.lock().unwrap().pairing_code().unwrap();

        let mut params = valid_params(&client_id, "https://claude.ai/cb", "s1");
        params.push(("pairing_code", real_code.as_str()));

        let extract_code = |location: &str| -> String {
            location
                .split("code=")
                .nth(1)
                .unwrap()
                .split('&')
                .next()
                .unwrap()
                .to_string()
        };

        let resp1 = post_authorize(&router, &params).await;
        let loc1 = resp1
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let code1 = extract_code(&loc1);

        let resp2 = post_authorize(&router, &params).await;
        let loc2 = resp2
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let code2 = extract_code(&loc2);

        assert_ne!(code1, code2, "each successful POST must mint a fresh code");

        let store = ctx.store.lock().unwrap();
        assert!(
            store.consume_code(&code1).unwrap().is_some(),
            "code1 must be independently redeemable"
        );
        assert!(
            store.consume_code(&code2).unwrap().is_some(),
            "code2 must be independently redeemable"
        );
    }

    #[tokio::test]
    async fn post_authorize_unknown_client_id_renders_400_never_redirects() {
        let (_dir, _ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let mut params = valid_params("does-not-exist", "https://claude.ai/cb", "s1");
        params.push(("pairing_code", "IRRELEVANT"));
        let resp = post_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(resp.headers().get(header::LOCATION).is_none());
    }

    /// The core "re-validate everything server-side" requirement: a tampered
    /// hidden `redirect_uri` field must be rejected exactly like a fresh
    /// GET would reject it — 400, no redirect — even though the pairing
    /// code presented is genuinely correct (the request never gets far
    /// enough to check it).
    #[tokio::test]
    async fn post_authorize_tampered_redirect_uri_renders_400_never_redirects_even_with_right_code()
    {
        let (dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        let real_code = ctx.store.lock().unwrap().pairing_code().unwrap();

        let mut params = valid_params(&client_id, "https://attacker.example/steal", "s1");
        params.push(("pairing_code", real_code.as_str()));
        let resp = post_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(
            resp.headers().get(header::LOCATION).is_none(),
            "a forged redirect_uri must never be redirected to, even with a valid pairing code"
        );

        let codes_path = dir.path().join("auth").join("codes.json");
        if codes_path.exists() {
            let contents = std::fs::read_to_string(&codes_path).unwrap();
            let map: std::collections::BTreeMap<String, Value> =
                serde_json::from_str(&contents).unwrap();
            assert!(
                map.is_empty(),
                "no code may be minted against an unvalidated redirect_uri"
            );
        }
    }

    /// The wrong-code re-render path reuses [`render_consent_form`] — this
    /// proves that reuse doesn't reopen the XSS hole covered above.
    #[tokio::test]
    async fn post_authorize_wrong_code_rerender_still_escapes_client_name_xss() {
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, Some(XSS_PAYLOAD), "https://claude.ai/cb");
        ctx.store.lock().unwrap().pairing_code().unwrap();

        let mut params = valid_params(&client_id, "https://claude.ai/cb", "s1");
        params.push(("pairing_code", "WRONG-CODE"));
        let resp = post_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(
            !body.contains("<script>alert(1)</script>"),
            "raw XSS payload leaked in the wrong-code re-render: {body}"
        );
    }

    #[tokio::test]
    async fn authorize_router_has_no_auth_gate_of_its_own() {
        // No `Authorization` header at all — a client with no token yet must
        // be able to complete the consent flow to obtain one.
        let (_dir, ctx, router) = authorize_fixture("http://127.0.0.1:7717");
        let client_id = register_web_client(&ctx, None, "https://claude.ai/cb");
        let params = valid_params(&client_id, "https://claude.ai/cb", "s1");
        let resp = get_authorize(&router, &params).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
