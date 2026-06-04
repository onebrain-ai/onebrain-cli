//! Static SPA serving + per-session token injection.
//!
//! Two cases:
//! 1. **`dist_dir` set** — serve it with `tower_http::services::ServeDir`,
//!    falling back to `index.html` for unknown routes (client-side routing).
//!    `index.html` is NOT served straight off disk: it's passed through
//!    [`inject_token`] so the browser SPA can read the session token.
//! 2. **`dist_dir` None** — serve a tiny built-in placeholder page so the API
//!    is still reachable for API-only testing (`serve` with no UI mounted).
//!
//! The token is injected by replacing a `__ONEBRAIN_TOKEN__` placeholder if the
//! HTML contains one, otherwise by inserting a `<script>` that sets
//! `window.__ONEBRAIN_TOKEN__` just before `</head>`. Either way the SPA reads
//! `window.__ONEBRAIN_TOKEN__` and never hard-codes the secret.
//
// step 2b: PWA wiring hooks in here — serve `service-worker.js` at the dist
//          root with `Service-Worker-Allowed: /` + no-cache, and
//          `manifest.webmanifest` with the right MIME; long-cache hashed assets,
//          no-cache the entry HTML.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt; // for ServeDir::oneshot
use tower_http::services::ServeDir;

use super::AppState;

/// The placeholder a dist's `index.html` may contain; replaced verbatim with
/// the live token. SPAs that prefer the script-injection path simply omit it.
const TOKEN_PLACEHOLDER: &str = "__ONEBRAIN_TOKEN__";

/// The catch-all static handler, wired into the router via `Router::fallback`
/// so it answers every non-`/api` route. We can't hand a bare `ServeDir` to
/// `fallback` because we need to intercept `index.html` for token injection;
/// this handler:
/// - for the SPA entry (`/` or `/index.html`, or any unknown route via 404
///   fallback), returns the token-injected `index.html`;
/// - otherwise delegates to `ServeDir` for real static assets.
pub async fn serve_static(State(state): State<Arc<AppState>>, request: Request) -> Response {
    match &state.dist_dir {
        Some(dist) => serve_from_dist(dist, &state.token, request).await,
        // No UI mounted — always return the injected placeholder page.
        None => placeholder_html(&state.token).into_response(),
    }
}

/// Serve a request against a real dist directory, injecting the token into any
/// `index.html` response (whether requested directly or via SPA fallback).
async fn serve_from_dist(dist: &Path, token: &str, request: Request) -> Response {
    let path = request.uri().path().to_owned();

    // The entry shell: explicit `/` or `/index.html`. Serve the injected HTML.
    if path == "/" || path == "/index.html" {
        return serve_injected_index(dist, token).await;
    }

    // Try the asset on disk via ServeDir. `oneshot` drives the service once.
    // On a 404 we fall back to the SPA shell (client-side routing) — that's
    // what makes deep links like `/v/explorer` load the app instead of 404ing.
    let serve_dir = ServeDir::new(dist);
    match serve_dir.oneshot(request).await {
        Ok(res) if res.status() != StatusCode::NOT_FOUND => res.map(Body::new),
        // 404 (or a ServeDir error) → SPA fallback to the injected index.
        _ => serve_injected_index(dist, token).await,
    }
}

/// Read `<dist>/index.html`, inject the token, and return it as an HTML
/// response. If the file is missing, fall back to the built-in placeholder so
/// `serve --dir <empty>` still yields a usable (token-bearing) page.
async fn serve_injected_index(dist: &Path, token: &str) -> Response {
    let index = dist.join("index.html");
    match std::fs::read_to_string(&index) {
        Ok(html) => html_response(inject_token(&html, token)),
        Err(_) => placeholder_html(token).into_response(),
    }
}

/// The built-in placeholder served when no dist is mounted (or its index is
/// missing). One line of body text + the token script so an API-only `serve`
/// still hands the token to anything that loads the root.
fn placeholder_html(token: &str) -> Response {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>OneBrain daemon</title>\
         <script>window.{TOKEN_VAR}=\"{token}\";</script>\
         </head><body>OneBrain daemon — no UI dist mounted</body></html>",
        TOKEN_VAR = "__ONEBRAIN_TOKEN__",
    );
    html_response(html)
}

/// Inject the session token into an HTML document.
///
/// Strategy (in order):
/// 1. If the HTML contains the `__ONEBRAIN_TOKEN__` placeholder, replace every
///    occurrence with the live token (the dist opted into explicit injection).
/// 2. Else, insert a `<script>window.__ONEBRAIN_TOKEN__="<token>"</script>`
///    immediately before the first `</head>`.
/// 3. Else (no `</head>` at all — minimal/partial HTML), prepend the script so
///    the token is still defined before any other script runs.
pub fn inject_token(html: &str, token: &str) -> String {
    if html.contains(TOKEN_PLACEHOLDER) {
        return html.replace(TOKEN_PLACEHOLDER, token);
    }

    let script = format!("<script>window.__ONEBRAIN_TOKEN__=\"{token}\";</script>");

    if let Some(idx) = html.find("</head>") {
        let mut out = String::with_capacity(html.len() + script.len());
        out.push_str(&html[..idx]);
        out.push_str(&script);
        out.push_str(&html[idx..]);
        out
    } else {
        format!("{script}{html}")
    }
}

/// Wrap an HTML string in a `200 text/html` response.
fn html_response(html: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_replaces_placeholder_when_present() {
        let html = "<head><script>const t='__ONEBRAIN_TOKEN__';</script></head>";
        let out = inject_token(html, "deadbeef");
        assert!(out.contains("const t='deadbeef'"));
        assert!(!out.contains("__ONEBRAIN_TOKEN__"));
    }

    #[test]
    fn inject_inserts_script_before_head_close() {
        let html = "<html><head><title>x</title></head><body></body></html>";
        let out = inject_token(html, "cafe");
        assert!(out.contains("window.__ONEBRAIN_TOKEN__=\"cafe\""));
        // Script lands before </head>.
        let script_at = out.find("__ONEBRAIN_TOKEN__").unwrap();
        let head_close = out.find("</head>").unwrap();
        assert!(script_at < head_close, "token script must precede </head>");
    }

    #[test]
    fn inject_prepends_when_no_head() {
        let html = "<body>just a fragment</body>";
        let out = inject_token(html, "f00d");
        assert!(out.starts_with("<script>window.__ONEBRAIN_TOKEN__=\"f00d\""));
        assert!(out.contains("just a fragment"));
    }

    #[tokio::test]
    async fn placeholder_page_carries_the_token() {
        use axum::body::to_bytes;
        use axum::http::header::CONTENT_TYPE;

        // The built-in no-dist page must still define the token global. Assert
        // on the ACTUAL response `placeholder_html` produces, not a separately
        // rebuilt string (the previous test discarded the body and re-derived
        // the expected HTML, so it could never have failed).
        let response = placeholder_html("abc123");

        // It's an HTML response.
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8"),
        );

        // The body actually contains the token-setting script.
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("window.__ONEBRAIN_TOKEN__=\"abc123\""),
            "placeholder body must set the token global: {body}"
        );
        assert!(
            body.contains("no UI dist mounted"),
            "placeholder body should carry the no-dist notice: {body}"
        );
    }
}
