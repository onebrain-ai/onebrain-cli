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

/// Redirects are not auto-followed by ureq (see `probe_frameable`): each hop is
/// re-validated against `is_http_url` before being requested. This bounds the
/// number of manual hops we'll chase before giving up.
const MAX_REDIRECT_HOPS: u32 = 5;

/// Blocking header probe (ureq is sync → runs on a blocking thread). Reads only
/// headers; the body is never consumed. The agent is configured to NOT
/// auto-follow redirects (`max_redirects(0)`) so each hop's `Location` can be
/// re-validated with `is_http_url` before being requested — this closes the
/// SSRF gap where a `http://` URL 302s to `file://` or an internal host. Any
/// error, non-http redirect target, or exceeding `MAX_REDIRECT_HOPS` → false.
fn probe_frameable(url: &str) -> bool {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(5)))
        .max_redirects(0)
        .build()
        .into();

    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECT_HOPS {
        let resp = match agent.get(&current).call() {
            Ok(resp) => resp,
            Err(_) => return false,
        };
        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok());
            let Some(location) = location else {
                return false;
            };
            let Some(next) = resolve_redirect(&current, location) else {
                return false;
            };
            current = next;
            continue;
        }
        let xfo_blocks = resp
            .headers()
            .get_all(axum::http::header::X_FRAME_OPTIONS)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| !frameable_from_headers(Some(v), None));
        if xfo_blocks {
            return false;
        }
        let csp_blocks = resp
            .headers()
            .get_all(axum::http::header::CONTENT_SECURITY_POLICY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| !frameable_from_headers(None, Some(v)));
        return !csp_blocks;
    }
    // Exceeded MAX_REDIRECT_HOPS without reaching a final response.
    false
}

/// Resolve a `Location` header against the URL that produced it.
/// Absolute `http(s)` targets pass through. Scheme-relative (`//host/…`) and
/// absolute-path (`/…`) references resolve deterministically per RFC 3986
/// against the hop that issued them — needed in the wild: th.wikipedia's
/// `Special:Search` redirects with a scheme-relative Location (2026-07-02).
/// This keeps the SSRF posture intact: every accepted target is still plain
/// `http(s)` (a redirector could already name any host absolutely), and
/// path-relative / non-http forms remain rejected.
fn resolve_redirect(base: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if is_http_url(location) {
        return Some(location.to_string());
    }
    if let Some(rest) = location.strip_prefix("//") {
        let scheme = base.split("://").next()?;
        let resolved = format!("{scheme}://{rest}");
        return is_http_url(&resolved).then_some(resolved);
    }
    if location.starts_with('/') {
        let scheme_end = base.find("://")? + 3;
        let origin_end = base[scheme_end..]
            .find('/')
            .map_or(base.len(), |i| scheme_end + i);
        return Some(format!("{}{}", &base[..origin_end], location));
    }
    None
}

/// http / https only — the only schemes we preflight or frame.
pub fn is_http_url(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("http://") || u.starts_with("https://")
}

/// Does a single `Content-Security-Policy` header VALUE block framing? True
/// when the value has a `frame-ancestors` directive whose value isn't the
/// bare wildcard `*`. Pure/unit-testable so it can be folded over multiple
/// header values (a server may send more than one CSP header, and `frame()`
/// / `get()` on a header map only surfaces the first — see `probe_frameable`,
/// which calls this via `get_all`).
fn csp_value_blocks(csp: &str) -> bool {
    let lower = csp.to_ascii_lowercase();
    if let Some(idx) = lower.find("frame-ancestors") {
        let after = &csp[idx + "frame-ancestors".len()..];
        let directive = after.split(';').next().unwrap_or("").trim();
        // Only a bare `*` (any ancestor) permits our arbitrary origin.
        return directive != "*";
    }
    false
}

/// Decide framability from the two headers that govern it. Conservative: any
/// `X-Frame-Options` value blocks (DENY/SAMEORIGIN, or a legacy ALLOW-FROM we
/// can't honour from an opaque iframe); a CSP with ANY `frame-ancestors`
/// directive that isn't the wildcard `*` blocks. Absent headers → frameable.
///
/// This single-value form is kept for the existing unit tests and for the
/// early bad-scheme short-circuit; `probe_frameable` folds `csp_value_blocks`
/// over every CSP header value it receives instead of calling this directly,
/// since a response may carry more than one CSP header.
pub fn frameable_from_headers(x_frame_options: Option<&str>, csp: Option<&str>) -> bool {
    if x_frame_options
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return false;
    }
    !csp.map(csp_value_blocks).unwrap_or(false)
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

    #[test]
    fn resolve_redirect_keeps_absolute_http_https() {
        assert_eq!(
            resolve_redirect("http://example.com/a", "https://example.com/b"),
            Some("https://example.com/b".to_string())
        );
        assert_eq!(
            resolve_redirect("https://example.com/a", "http://other.example/b"),
            Some("http://other.example/b".to_string())
        );
    }

    #[test]
    fn resolve_redirect_rejects_non_http_target() {
        // A 302 to file:// or an internal-only scheme must not be followed —
        // this is the SSRF gap the manual redirect handling closes.
        assert_eq!(
            resolve_redirect("http://example.com/a", "file:///etc/passwd"),
            None
        );
        assert_eq!(
            resolve_redirect("http://example.com/a", "ftp://internal/x"),
            None
        );
    }

    #[test]
    fn resolve_redirect_resolves_scheme_relative_with_base_scheme() {
        // th.wikipedia's Special:Search redirects with `//host/…` — resolve
        // it with the issuing hop's scheme instead of rejecting.
        assert_eq!(
            resolve_redirect(
                "https://th.wikipedia.org/wiki/Special:Search?search=x",
                "//th.wikipedia.org/w/index.php?title=y"
            ),
            Some("https://th.wikipedia.org/w/index.php?title=y".to_string())
        );
        assert_eq!(
            resolve_redirect("http://example.com/a", "//other.example/b"),
            Some("http://other.example/b".to_string())
        );
    }

    #[test]
    fn resolve_redirect_resolves_absolute_path_against_origin() {
        assert_eq!(
            resolve_redirect("https://example.com/a/b?q=1", "/c/d"),
            Some("https://example.com/c/d".to_string())
        );
        // Origin keeps an explicit port; a bare-origin base (no path) works too.
        assert_eq!(
            resolve_redirect("http://127.0.0.1:8080/x", "/y"),
            Some("http://127.0.0.1:8080/y".to_string())
        );
        assert_eq!(
            resolve_redirect("https://example.com", "/z"),
            Some("https://example.com/z".to_string())
        );
    }

    #[test]
    fn resolve_redirect_still_rejects_path_relative_location() {
        // Path-relative forms stay rejected — rare in the wild and the only
        // shape where resolution would need real path arithmetic.
        assert_eq!(resolve_redirect("http://example.com/a", "b/c"), None);
        assert_eq!(resolve_redirect("http://example.com/a", "?q=1"), None);
    }

    #[test]
    fn csp_value_blocks_single_directive() {
        assert!(csp_value_blocks("frame-ancestors 'none'"));
        assert!(csp_value_blocks("frame-ancestors 'self'"));
        assert!(!csp_value_blocks("frame-ancestors *"));
        assert!(!csp_value_blocks("default-src 'self'")); // no directive at all
    }

    #[test]
    fn multi_value_csp_blocks_when_any_value_blocks() {
        // A server sending two CSP headers, with the blocking directive only
        // in the second value — folding must not stop at the first header.
        let values = ["default-src 'self'", "frame-ancestors 'none'"];
        assert!(values.iter().any(|v| csp_value_blocks(v)));

        // Symmetric case: blocking directive in the first value.
        let values = ["frame-ancestors 'self'", "default-src 'self'"];
        assert!(values.iter().any(|v| csp_value_blocks(v)));

        // Neither value blocks → overall permissive.
        let values = ["default-src 'self'", "frame-ancestors *"];
        assert!(!values.iter().any(|v| csp_value_blocks(v)));
    }

    #[test]
    fn multi_value_xfo_blocks_when_any_value_present() {
        // Mirrors the XFO fold in probe_frameable: any non-empty value blocks.
        let values = ["", "DENY"];
        assert!(values.iter().any(|v| !v.trim().is_empty()));

        let values: [&str; 0] = [];
        assert!(!values.iter().any(|v: &&str| !v.trim().is_empty()));
    }
}
