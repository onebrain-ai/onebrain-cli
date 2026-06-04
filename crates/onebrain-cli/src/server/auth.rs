//! Token-gating middleware for the `/api/*` routes.
//!
//! The token is delivered to the browser SPA via the injected `index.html`
//! (see [`super::r#static`]); from then on the SPA presents it on every API
//! call. We accept it two ways so both `curl` and `fetch`/`DaemonClient`
//! callers are easy:
//!
//! - `Authorization: Bearer <token>` — the HTTP-standard scheme.
//! - `X-OneBrain-Token: <token>` — a simpler custom header for fetch clients
//!   that would rather not touch `Authorization`.
//!
//! A request missing both (or carrying a wrong token) gets `401 Unauthorized`
//! before the handler runs. Static routes never pass through this layer.

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use super::AppState;

/// Header name for the simple custom-token scheme.
const TOKEN_HEADER: &str = "x-onebrain-token";

/// Axum middleware: allow the request through only if it carries the correct
/// session token; otherwise short-circuit with `401`.
///
/// Wired via `axum::middleware::from_fn_with_state` so it sees the shared
/// [`AppState`] (which holds the expected token) without a global.
pub async fn require_token(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if request_has_valid_token(&state.token, &request) {
        next.run(request).await
    } else {
        // Bare 401 — no `WWW-Authenticate` challenge, because this is a
        // machine/SPA boundary, not an interactive browser-prompt login.
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Pure predicate: does `request` present `expected` via either accepted
/// header? Factored out so it's unit-testable without constructing a `Next`.
fn request_has_valid_token(expected: &str, request: &Request) -> bool {
    let headers = request.headers();

    // 1. `Authorization: Bearer <token>` — trim the scheme, compare the rest.
    if let Some(bearer) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        if constant_time_eq(bearer.as_bytes(), expected.as_bytes()) {
            return true;
        }
    }

    // 2. `X-OneBrain-Token: <token>`.
    if let Some(custom) = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) {
        if constant_time_eq(custom.as_bytes(), expected.as_bytes()) {
            return true;
        }
    }

    false
}

/// Constant-time byte-slice equality.
///
/// A naïve `a == b` short-circuits on the FIRST differing byte, so the time it
/// takes to reject a guess leaks how many leading bytes were correct. An
/// attacker who can measure that timing can recover the token one byte at a
/// time. This matters here because the surface can be exposed beyond localhost
/// via `serve --host 0.0.0.0` (single-tenant self-host), where request timing is
/// observable. We therefore compare in time that depends only on the LENGTH,
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

    #[test]
    fn accepts_bearer_token() {
        let r = req_with(&[(AUTHORIZATION.as_str(), "Bearer secret123")]);
        assert!(request_has_valid_token("secret123", &r));
    }

    #[test]
    fn accepts_custom_header_token() {
        let r = req_with(&[(TOKEN_HEADER, "secret123")]);
        assert!(request_has_valid_token("secret123", &r));
    }

    #[test]
    fn rejects_missing_token() {
        let r = req_with(&[]);
        assert!(!request_has_valid_token("secret123", &r));
    }

    #[test]
    fn rejects_wrong_token() {
        let r = req_with(&[(AUTHORIZATION.as_str(), "Bearer nope")]);
        assert!(!request_has_valid_token("secret123", &r));
    }

    #[test]
    fn rejects_bearer_without_scheme_prefix() {
        // A raw token in Authorization without the `Bearer ` scheme is not
        // accepted on that header (it would be on X-OneBrain-Token).
        let r = req_with(&[(AUTHORIZATION.as_str(), "secret123")]);
        assert!(!request_has_valid_token("secret123", &r));
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
}
