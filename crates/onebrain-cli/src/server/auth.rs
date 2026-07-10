//! Token-gating middleware for (almost) the ENTIRE HTTP surface — both `/api/*`
//! and the static SPA. Gating the static routes too means an unauthenticated
//! browser can't even load the page shell (which carries the token), closing the
//! hole where opening the URL with no `?token=` still let you in.
//!
//! The **one** unauthenticated route is `GET`/`HEAD /robots.txt` (see
//! [`require_token`]) — static public boilerplate with no vault data.
//!
//! The token is accepted four ways:
//!
//! - `Authorization: Bearer <token>` — the HTTP-standard scheme (any method).
//! - `X-OneBrain-Token: <token>` — the SPA's fetch header (any method).
//! - `?token=<token>` query — the page-load / `<img>` entry; on success the
//!   response sets an `onebrain_token` cookie. GET/HEAD only.
//! - `Cookie: onebrain_token=<token>` — carries auth to the browser's token-less
//!   static-asset + read requests after that first load. GET/HEAD only.
//!
//! The query + cookie schemes are restricted to safe verbs so they stay off the
//! CSRF surface (the SPA always sends a header for mutations). A request missing
//! a valid token gets `401 Unauthorized` before the handler runs.

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use super::AppState;

/// Header name for the simple custom-token scheme.
const TOKEN_HEADER: &str = "x-onebrain-token";
/// Cookie name carrying the session token after the first `?token=` load.
const TOKEN_COOKIE: &str = "onebrain_token";

/// Outcome of the token check.
enum AuthOutcome {
    /// Authenticated. `set_cookie` is true when the token arrived via the
    /// `?token=` query (the page-load / `<img>` entry); we echo it back as a
    /// cookie so the browser's *subsequent* token-less asset + read requests
    /// authenticate without re-appending the query.
    Ok {
        set_cookie: bool,
    },
    Denied,
}

/// Axum middleware: allow the request through only if it carries the correct
/// session token; otherwise short-circuit with `401`. Applied to the WHOLE
/// router (API **and** static), so an unauthenticated browser can't even load
/// the SPA shell (which carries the token) — closing the "open the page with no
/// `?token=` and you're in" hole. The sole exception is `GET`/`HEAD /robots.txt`
/// (static public boilerplate, handled at the top of the body before the gate).
///
/// Wired via `axum::middleware::from_fn_with_state` so it sees the shared
/// [`AppState`] (which holds the expected token) without a global.
pub async fn require_token(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    // `/robots.txt` is the single route served WITHOUT a token (GET/HEAD only).
    // It's static boilerplate — no vault data, no filesystem access — and the
    // well-known-file convention is that crawlers fetch it unauthenticated. The
    // "unauthenticated browser gets nothing of value" property is intact: the SPA
    // shell, every asset, and every `/api` route still 401. Verb-restricted so it
    // never widens the CSRF surface the token gate protects.
    if request.uri().path() == "/robots.txt" && matches!(request.method().as_str(), "GET" | "HEAD")
    {
        return robots_txt();
    }

    let outcome = check_token(&state.token, &request);
    // Read before `request` is consumed by `next`: behind a TLS tunnel/proxy the
    // edge sets `X-Forwarded-Proto: https`, and only then is `Secure` correct (a
    // Secure cookie is dropped over plain HTTP, which would break a localhost run).
    let secure = forwarded_https(&request);
    match outcome {
        AuthOutcome::Ok { set_cookie } => {
            // Record activity for the daemon's idle-shutdown loop: any
            // authenticated request keeps the daemon alive. `Relaxed` is fine —
            // the loop only needs an approximate "seconds since last request".
            state.last_activity.store(
                super::now_epoch_secs(),
                std::sync::atomic::Ordering::Relaxed,
            );
            let mut resp = next.run(request).await;
            if set_cookie {
                // HttpOnly: JS never needs to read it (the SPA reads the token
                // from the injected HTML for its API headers). SameSite=Strict:
                // the browser won't attach it to cross-site requests, so the
                // cookie can't be abused for CSRF reads of a non-loopback
                // (`$ONEBRAIN_BIND`) instance. Secure: added when served over
                // HTTPS (see above).
                let secure_attr = if secure { "; Secure" } else { "" };
                if let Ok(v) = HeaderValue::from_str(&format!(
                    "{TOKEN_COOKIE}={}; HttpOnly; SameSite=Strict; Path=/{secure_attr}",
                    state.token
                )) {
                    resp.headers_mut().append(header::SET_COOKIE, v);
                }
            }
            resp
        }
        // Bare 401 — no `WWW-Authenticate` challenge, because this is a
        // machine/SPA boundary, not an interactive browser-prompt login.
        AuthOutcome::Denied => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// The private-instance `robots.txt`: tell every crawler to index nothing. Static
/// boilerplate served without a token (see the exemption in [`require_token`]) —
/// it carries no vault data and touches no filesystem, so it leaks nothing the
/// bare `401` didn't already reveal.
fn robots_txt() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "User-agent: *\nDisallow: /\n",
    )
        .into_response()
}

/// True when the browser↔edge hop was HTTPS (a TLS tunnel / reverse proxy sets
/// `X-Forwarded-Proto: https`). Used to mark the auth cookie `Secure`.
fn forwarded_https(request: &Request) -> bool {
    request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// Does `request` present `expected`? Factored out so it's unit-testable without
/// constructing a `Next`. Header tokens authenticate ANY method; the query and
/// cookie schemes are restricted to safe verbs (see inline notes).
fn check_token(expected: &str, request: &Request) -> AuthOutcome {
    let headers = request.headers();

    // 1. `Authorization: Bearer <token>` — trim the scheme, compare the rest.
    //    Valid for any method (a custom header can't be set by a cross-site form).
    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        if constant_time_eq(bearer.as_bytes(), expected.as_bytes()) {
            return AuthOutcome::Ok { set_cookie: false };
        }
    }

    // 2. `X-OneBrain-Token: <token>` — the SPA's fetch header. Any method.
    if let Some(custom) = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) {
        if constant_time_eq(custom.as_bytes(), expected.as_bytes()) {
            return AuthOutcome::Ok { set_cookie: false };
        }
    }

    // The query + cookie schemes are accepted ONLY for safe, read-only verbs
    // (GET + HEAD). Both can be triggered cross-site (a `<img>` URL; an
    // auto-sent cookie), so allowing them on POST/PUT/DELETE would be a CSRF
    // hole — the SPA always sends a header for mutations.
    if matches!(request.method().as_str(), "GET" | "HEAD") {
        // 3. `?token=<token>` query — the page-load and `<img src=…&token=>`
        //    entry. We echo a cookie so the SPA's later token-less asset/API-read
        //    requests authenticate. The token is fixed-charset hex (no decoding).
        //    SECURITY: a `?token=` rides in the request URI, so do NOT enable
        //    HTTP access-logging (or a URI-logging reverse proxy) on a remote
        //    instance — it would capture the token. The daemon never logs URIs.
        if let Some(token) = request.uri().query().and_then(query_param_token) {
            if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
                return AuthOutcome::Ok { set_cookie: true };
            }
        }
        // 4. `Cookie: onebrain_token=<token>` — set on the entry load above, so
        //    the browser's token-less static-asset + read requests authenticate.
        //    SameSite=Strict keeps it off cross-site requests.
        if let Some(token) = cookie_token(headers) {
            if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
                return AuthOutcome::Ok { set_cookie: false };
            }
        }
    }

    AuthOutcome::Denied
}

/// Extract `onebrain_token` from a `Cookie: a=1; onebrain_token=xxx; b=2` header.
fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .map(|c| c.trim())
                .find_map(|c| c.strip_prefix("onebrain_token="))
        })
}

/// Extract the `token` value from a raw query string (`a=1&token=xxx&b=2`).
fn query_param_token(query: &str) -> Option<&str> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
}

/// Constant-time byte-slice equality.
///
/// A naïve `a == b` short-circuits on the FIRST differing byte, so the time it
/// takes to reject a guess leaks how many leading bytes were correct. An
/// attacker who can measure that timing can recover the token one byte at a
/// time. This matters here because the surface can be exposed beyond localhost
/// via `$ONEBRAIN_BIND` (single-tenant container/self-host, #205), where
/// request timing is observable. We therefore compare in time that depends only on the LENGTH,
/// not the contents:
///
/// 1. Length check first. (Length is not the secret — it's fixed at 32 hex
///    chars — so an early-out here leaks nothing useful, and it lets the XOR
///    loop assume equal lengths.)
/// 2. XOR every byte pair and OR the differences into an accumulator. We touch
///    every byte regardless of where the first mismatch is; the result is `0`
///    iff all bytes matched. No data-dependent branch, no early return.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::header::AUTHORIZATION;

    fn req_with(headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().uri("/api/config");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(Body::empty()).unwrap()
    }

    /// Did the token check authenticate the request (ignoring set_cookie)?
    fn authed(expected: &str, request: &Request) -> bool {
        matches!(check_token(expected, request), AuthOutcome::Ok { .. })
    }

    /// Did the token check authenticate AND ask to set the cookie?
    fn authed_sets_cookie(expected: &str, request: &Request) -> bool {
        matches!(
            check_token(expected, request),
            AuthOutcome::Ok { set_cookie: true }
        )
    }

    #[test]
    fn accepts_bearer_token() {
        let r = req_with(&[(AUTHORIZATION.as_str(), "Bearer secret123")]);
        assert!(authed("secret123", &r));
    }

    #[test]
    fn accepts_custom_header_token() {
        let r = req_with(&[(TOKEN_HEADER, "secret123")]);
        assert!(authed("secret123", &r));
    }

    #[test]
    fn rejects_missing_token() {
        let r = req_with(&[]);
        assert!(!authed("secret123", &r));
    }

    #[test]
    fn rejects_wrong_token() {
        let r = req_with(&[(AUTHORIZATION.as_str(), "Bearer nope")]);
        assert!(!authed("secret123", &r));
    }

    #[test]
    fn rejects_bearer_without_scheme_prefix() {
        // A raw token in Authorization without the `Bearer ` scheme is not
        // accepted on that header (it would be on X-OneBrain-Token).
        let r = req_with(&[(AUTHORIZATION.as_str(), "secret123")]);
        assert!(!authed("secret123", &r));
    }

    #[test]
    fn constant_time_eq_matches_only_identical_slices() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
        // Same length, differing content.
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        // Differing lengths (one a prefix of the other).
        assert!(!constant_time_eq(b"abc", b"abc123"));
        assert!(!constant_time_eq(b"abc123", b"abc"));
        // Two empties are equal (degenerate but well-defined).
        assert!(constant_time_eq(b"", b""));
    }

    fn req(method: &str, uri: &str) -> Request {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn accepts_query_token_on_get() {
        // an <img src="/api/vault/raw?path=x.png&token=…"> can authenticate a read
        let r = req("GET", "/api/vault/raw?path=x.png&token=secret123");
        assert!(authed("secret123", &r));
    }

    #[test]
    fn accepts_query_token_on_head() {
        // media-preview probes use HEAD — it's read-only, so query-token is allowed
        let r = req("HEAD", "/api/vault/raw?path=x.png&token=secret123");
        assert!(authed("secret123", &r));
    }

    #[test]
    fn rejects_query_token_on_non_get() {
        // query-token must NOT authenticate a mutating request (CSRF surface)
        let r = req("POST", "/api/vault/delete?path=x&token=secret123");
        assert!(!authed("secret123", &r));
    }

    #[test]
    fn rejects_wrong_query_token() {
        let r = req("GET", "/api/vault/raw?path=x&token=nope");
        assert!(!authed("secret123", &r));
    }

    #[test]
    fn query_token_on_get_asks_to_set_cookie() {
        // the page-load entry: a valid ?token= authenticates AND seeds the cookie
        let r = req("GET", "/?token=secret123");
        assert!(authed_sets_cookie("secret123", &r));
    }

    #[test]
    fn header_token_does_not_set_cookie() {
        // a fetch with the header is already self-authenticating — no cookie echo
        let r = req_with(&[(TOKEN_HEADER, "secret123")]);
        assert!(authed("secret123", &r) && !authed_sets_cookie("secret123", &r));
    }

    #[test]
    fn accepts_cookie_token_on_get() {
        // a token-less static-asset request authenticates via the seeded cookie
        let r = req_with(&[("cookie", "onebrain_token=secret123")]);
        assert!(authed("secret123", &r));
    }

    #[test]
    fn accepts_cookie_token_among_other_cookies() {
        let r = req_with(&[("cookie", "foo=1; onebrain_token=secret123; bar=2")]);
        assert!(authed("secret123", &r));
    }

    #[test]
    fn rejects_cookie_token_on_mutation() {
        // a cookie auto-sends cross-site → must NOT authenticate a mutating verb
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/vault/file?path=x");
        builder = builder.header("cookie", "onebrain_token=secret123");
        let r = builder.body(Body::empty()).unwrap();
        assert!(!authed("secret123", &r));
    }

    #[test]
    fn rejects_wrong_cookie_token() {
        let r = req_with(&[("cookie", "onebrain_token=nope")]);
        assert!(!authed("secret123", &r));
    }
}
