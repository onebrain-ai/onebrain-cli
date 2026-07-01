//! Frameability preflight for the in-app webview: inspect a URL's response
//! headers so the frontend knows whether to embed it in an iframe or fall back
//! to a new tab. Any failure degrades to "not frameable" (safe fallback).

use axum::{
    extract::Query,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct PreflightParams {
    pub url: String,
}

#[derive(Serialize)]
pub struct PreflightResponse {
    pub frameable: bool,
}

/// `GET /api/webview/preflight?url=…` — fetch the URL's headers and report
/// whether it may be embedded in an iframe. Never errors the caller: any
/// problem (bad scheme, network failure, timeout) resolves to frameable=false
/// so the frontend cleanly falls back to a new tab.
pub async fn get_webview_preflight(Query(p): Query<PreflightParams>) -> Response {
    let frameable = if !is_http_url(&p.url) {
        false
    } else {
        let url = p.url.clone();
        tokio::task::spawn_blocking(move || probe_frameable(&url))
            .await
            .unwrap_or(false)
    };
    Json(PreflightResponse { frameable }).into_response()
}

/// Blocking header probe (ureq is sync → runs on a blocking thread). Reads only
/// headers; the body is never consumed. Redirects are followed by ureq with its
/// default cap. Any error → false.
fn probe_frameable(url: &str) -> bool {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(5)))
        .build()
        .into();
    match agent.get(url).call() {
        Ok(resp) => {
            let xfo = resp
                .headers()
                .get("x-frame-options")
                .and_then(|v| v.to_str().ok());
            let csp = resp
                .headers()
                .get("content-security-policy")
                .and_then(|v| v.to_str().ok());
            frameable_from_headers(xfo, csp)
        }
        Err(_) => false,
    }
}

/// http / https only — the only schemes we preflight or frame.
pub fn is_http_url(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("http://") || u.starts_with("https://")
}

/// Decide framability from the two headers that govern it. Conservative: any
/// `X-Frame-Options` value blocks (DENY/SAMEORIGIN, or a legacy ALLOW-FROM we
/// can't honour from an opaque iframe); a CSP with ANY `frame-ancestors`
/// directive that isn't the wildcard `*` blocks. Absent headers → frameable.
pub fn frameable_from_headers(x_frame_options: Option<&str>, csp: Option<&str>) -> bool {
    if x_frame_options
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return false;
    }
    if let Some(csp) = csp {
        // Find the frame-ancestors directive (case-insensitive) and read its value.
        let lower = csp.to_ascii_lowercase();
        if let Some(idx) = lower.find("frame-ancestors") {
            let after = &csp[idx + "frame-ancestors".len()..];
            let directive = after.split(';').next().unwrap_or("").trim();
            // Only a bare `*` (any ancestor) permits our arbitrary origin.
            return directive == "*";
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_https_only() {
        assert!(is_http_url("https://example.com"));
        assert!(is_http_url("http://example.com/a?b=c"));
        assert!(!is_http_url("ftp://example.com"));
        assert!(!is_http_url("file:///etc/passwd"));
        assert!(!is_http_url("javascript:alert(1)"));
        assert!(!is_http_url("example.com"));
    }

    #[test]
    fn x_frame_options_blocks() {
        assert!(!frameable_from_headers(Some("DENY"), None));
        assert!(!frameable_from_headers(Some("deny"), None));
        assert!(!frameable_from_headers(Some("SAMEORIGIN"), None));
        assert!(!frameable_from_headers(Some("Allow-From https://x"), None)); // unknown/legacy → treat as blocked
    }

    #[test]
    fn csp_frame_ancestors_blocks() {
        assert!(!frameable_from_headers(
            None,
            Some("frame-ancestors 'none'")
        ));
        assert!(!frameable_from_headers(
            None,
            Some("default-src 'self'; frame-ancestors 'self'")
        ));
        assert!(!frameable_from_headers(
            None,
            Some("frame-ancestors https://trusted.example")
        ));
    }

    #[test]
    fn absent_or_permissive_is_frameable() {
        assert!(frameable_from_headers(None, None));
        assert!(frameable_from_headers(None, Some("default-src 'self'"))); // no frame-ancestors directive
        assert!(frameable_from_headers(None, Some("frame-ancestors *")));
    }

    #[tokio::test]
    async fn non_http_scheme_is_not_frameable() {
        use axum::extract::Query;
        let resp = get_webview_preflight(Query(PreflightParams {
            url: "file:///etc/passwd".to_string(),
        }))
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        // Body asserts frameable:false without touching the network.
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], br#"{"frameable":false}"#);
    }
}
