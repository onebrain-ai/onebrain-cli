//! Security response headers, applied to EVERY response (API, static, and the
//! auth middleware's 401s). Wired as the outermost layer in [`super::build_router`].
//!
//! These harden the browser side so the surface is safer the moment it's put
//! behind a TLS tunnel/proxy (the recommended way to expose it):
//! - `Content-Security-Policy` — confines the SPA to its own origin; blocks
//!   framing, `<base>` injection, and plugin/object embeds.
//! - `X-Frame-Options: DENY` + CSP `frame-ancestors 'none'` — clickjacking.
//! - `X-Content-Type-Options: nosniff` — MIME-sniffing.
//! - `Referrer-Policy: no-referrer` — stops the `?token=` entry URL leaking to
//!   any external link clicked inside a rendered note.
//! - `Strict-Transport-Security` — only when the edge hop was HTTPS
//!   (`X-Forwarded-Proto: https`), so a plain-HTTP localhost run isn't pinned.

use axum::{
    extract::Request,
    http::{header, HeaderMap, HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};

/// SPA-safe policy: `'self'` for everything, plus the inline token script/styles
/// the build emits, `data:`/`https:` images (vault data-URIs + README badges), and
/// `data:` fonts (the web UI's Office-doc preview inlines slide/text fonts as data-URIs).
/// Tightening `script-src` to a nonce is a follow-up (would need the static
/// handler + this layer to share a per-response nonce).
const CSP: &str = "default-src 'self'; \
script-src 'self' 'unsafe-inline'; \
style-src 'self' 'unsafe-inline'; \
img-src 'self' data: https:; \
font-src 'self' data:; \
connect-src 'self'; \
object-src 'none'; \
base-uri 'self'; \
form-action 'self'; \
frame-ancestors 'none'";

/// Middleware: run the request, then stamp the security headers on the response.
pub async fn security_headers(request: Request, next: Next) -> Response {
    // Did the browser↔edge hop use HTTPS? A TLS tunnel / reverse proxy sets
    // `X-Forwarded-Proto: https`; only then is HSTS appropriate.
    let https = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("https"))
        .unwrap_or(false);

    let mut resp = next.run(request).await;
    let h = resp.headers_mut();
    set(h, header::CONTENT_SECURITY_POLICY, CSP);
    set(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(h, header::X_FRAME_OPTIONS, "DENY");
    set(h, header::REFERRER_POLICY, "no-referrer");
    set(
        h,
        HeaderName::from_static("cross-origin-opener-policy"),
        "same-origin",
    );
    if https {
        set(
            h,
            header::STRICT_TRANSPORT_SECURITY,
            "max-age=31536000; includeSubDomains",
        );
    }
    resp
}

/// Insert (overwrite) a header from a static string. Static values can't fail to
/// parse, so a bad value is a programming error we silently skip rather than
/// panic the request.
fn set(h: &mut HeaderMap, name: HeaderName, value: &'static str) {
    h.insert(name, HeaderValue::from_static(value));
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    async fn through(proto: Option<&str>) -> Response {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(security_headers));
        let mut req = Request::builder().uri("/");
        if let Some(p) = proto {
            req = req.header("x-forwarded-proto", p);
        }
        app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn stamps_the_core_security_headers() {
        let resp = through(None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let h = resp.headers();
        assert!(h.get(header::CONTENT_SECURITY_POLICY).is_some());
        assert_eq!(h.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        assert_eq!(h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(h.get(header::REFERRER_POLICY).unwrap(), "no-referrer");
        // CSP forbids framing + object embeds.
        let csp = h
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(csp.contains("object-src 'none'"));
        // `data:` fonts are allowed so the web UI's Office-doc preview can render
        // the slide/text fonts PowerPoint/Word embed as data-URIs.
        assert!(csp.contains("font-src 'self' data:"));
    }

    #[tokio::test]
    async fn hsts_only_when_forwarded_https() {
        // plain HTTP (localhost) → no HSTS (don't pin a cert the user doesn't have)
        assert!(through(None)
            .await
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .is_none());
        // behind a TLS tunnel/proxy → HSTS
        assert!(through(Some("https"))
            .await
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .is_some());
    }
}
