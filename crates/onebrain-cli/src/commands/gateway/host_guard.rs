//! Host/Origin guard for the WHOLE gateway router (#404 items 1-2).
//!
//! `/mcp` already sits behind rmcp's own DNS-rebinding guard
//! (`StreamableHttpServerConfig::allowed_hosts`), but the merged OAuth,
//! discovery and `/approvals` routers had none: a DNS-rebinding page could
//! reach `/register`/`/authorize`/`/token` same-origin and read the
//! responses. [`require_allowed_host`] closes that for every route, and the
//! SAME [`allowed_hosts`] list is handed to rmcp so `/mcp` accepts the
//! tunnel hostname too (rmcp's default is loopback-only, which would 403
//! every tunnelled request).
//!
//! Rules, in order:
//! 1. `Host` (or, for HTTP/2, the request URI's authority) must parse as an
//!    authority with no userinfo — else **400**.
//! 2. Its host (case-insensitive, IPv6 brackets stripped, port ignored) must
//!    be a loopback host ([`LOOPBACK_HOSTS`]) or the configured `public_url`'s
//!    host — else **403**.
//! 3. `/approvals` and `/approvals/…` (the pairing-code-gated operator
//!    surface) additionally require a LOOPBACK host — the `public_url` host
//!    gets **403**, so the tunnel never exposes it (red-team item 5). Any
//!    reverse-proxy header ([`PROXY_HEADERS`]) on an `/approvals` request is
//!    **403** too: a tunnel or proxy that rewrites `Host` to the loopback
//!    origin would otherwise deliver internet requests as `Host: localhost`.
//!    Other routes ignore these headers (cloudflared adds them to every
//!    request).
//! 4. On a state-changing method (anything but GET/HEAD/OPTIONS) outside
//!    `/mcp`, [`request_origin_allowed`] must hold — else **403**:
//!    an `Origin` header, when present, must be the issuer's exact origin
//!    (scheme + host + port, default ports normalised), except that
//!    `Origin: null` passes when `Sec-Fetch-Site: same-origin` vouches for
//!    it. Absent `Origin` passes: non-browser clients don't send one, and a
//!    browser always does on a POST, so a cross-site browser request can't
//!    slip through by omitting it. `/mcp` is exempt (hub ruling): it needs
//!    `Authorization: Bearer`, which a cross-site page cannot attach without
//!    a CORS preflight the gateway never answers, so the rule would only
//!    break MCP clients that happen to send an `Origin`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::uri::Authority;
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::oauth_routes::AuthCtx;

/// Loopback hosts in normalised form (lower-case, IPv6 brackets stripped) —
/// the same three rmcp's own default `allowed_hosts` lists. Matching is
/// host-only, so each is allowed with or without a `:port`.
pub const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// Every host the gateway answers to: [`LOOPBACK_HOSTS`] plus the host of
/// `public_url` when it is set and parses. An unparseable `public_url` adds
/// nothing (fail closed) — `gateway run` already refuses to start on one
/// (`validate_public_url`), so this only matters for hand-built test state.
/// Also passed verbatim to rmcp's `with_allowed_hosts`, whose matcher
/// normalises the same way (lower-case, brackets stripped, port-less entry
/// = any port).
pub fn allowed_hosts(public_url: Option<&str>) -> Vec<String> {
    let mut hosts: Vec<String> = LOOPBACK_HOSTS.iter().map(|h| (*h).to_string()).collect();
    if let Some(host) = public_url.and_then(public_url_host) {
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts
}

fn public_url_host(url: &str) -> Option<String> {
    let uri: Uri = url.trim_end_matches('/').parse().ok()?;
    uri.host().filter(|h| !h.is_empty()).map(normalize_host)
}

fn normalize_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

/// `(scheme, host, port)` for a bare `http(s)://host[:port]` origin, default
/// port filled in; `None` for anything else (`null`, a path, userinfo, a
/// non-http scheme).
fn origin_tuple(value: &str) -> Option<(String, String, u16)> {
    let uri: Uri = value.trim().parse().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
        return None;
    }
    let authority = uri.authority()?;
    if authority.as_str().contains('@') {
        return None;
    }
    Some((
        scheme,
        normalize_host(authority.host()),
        authority.port_u16().unwrap_or(default_port),
    ))
}

/// Does a request's `Origin` equal the OAuth issuer's origin? Shared with
/// `oauth_routes`'s `POST /authorize` CSRF check (Task 4) so both surfaces
/// agree on exactly one definition.
pub(crate) fn origin_matches_issuer(origin: &str, issuer: &str) -> bool {
    match (origin_tuple(origin), origin_tuple(issuer)) {
        (Some(o), Some(i)) => o == i,
        _ => false,
    }
}

/// The ONE Origin rule, shared by [`require_allowed_host`] and
/// `oauth_routes`'s `POST /authorize` check: no `Origin` → allowed;
/// `Origin: null` → allowed only with `Sec-Fetch-Site: same-origin` (hub
/// ruling — a browser's own consent-form POST under a privacy setting that
/// still nulls the Origin); anything else must be the issuer's origin. A
/// non-UTF-8 `Origin` is refused.
pub(crate) fn request_origin_allowed(headers: &HeaderMap, issuer: &str) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    match origin.to_str() {
        Ok("null") => headers
            .get("sec-fetch-site")
            .is_some_and(|site| site.as_bytes() == b"same-origin"),
        Ok(o) => origin_matches_issuer(o, issuer),
        Err(_) => false,
    }
}

/// Headers a tunnel or reverse proxy adds to a forwarded request. Presence of
/// any one marks an `/approvals` request as not-really-loopback, whatever its
/// `Host` says. (`HeaderMap` names are lower-case, so matching is
/// case-insensitive.)
const PROXY_HEADERS: [&str; 5] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-real-ip",
    "cf-connecting-ip",
];

fn has_proxy_header(headers: &HeaderMap) -> bool {
    PROXY_HEADERS.iter().any(|name| headers.contains_key(*name))
}

/// Longest caller-supplied value the guard writes to a log line, in bytes.
const MAX_LOGGED_BYTES: usize = 128;

/// A caller-supplied header value made safe to log: lossy UTF-8, cut to
/// [`MAX_LOGGED_BYTES`] on a char boundary with a trailing `…` when cut — the
/// Origin branch is reachable unauthenticated, so an uncapped value would let
/// anyone write header-sized warn lines.
fn capped_for_log(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_LOGGED_BYTES {
        return text.into_owned();
    }
    let mut end = MAX_LOGGED_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// `/mcp` or anything under it (not `/mcpx`).
fn is_mcp_path(path: &str) -> bool {
    path == "/mcp" || path.starts_with("/mcp/")
}

/// `/approvals` or anything under it (not `/approvalsx`).
fn is_approvals_path(path: &str) -> bool {
    path == "/approvals" || path.starts_with("/approvals/")
}

/// State for [`require_allowed_host`]: the precomputed host list, plus the
/// shared [`AuthCtx`] so the Origin check reads the issuer at REQUEST time
/// (it is only set in `on_bind`, after this router is built).
pub struct HostGuard {
    allowed_hosts: Vec<String>,
    auth: Arc<AuthCtx>,
}

impl HostGuard {
    pub fn new(allowed_hosts: Vec<String>, auth: Arc<AuthCtx>) -> Self {
        Self {
            allowed_hosts,
            auth,
        }
    }
}

fn request_host(uri: &Uri, headers: &HeaderMap) -> Result<String, &'static str> {
    let raw = match headers.get(header::HOST) {
        Some(value) => value
            .to_str()
            .map_err(|_| "Bad Request: invalid Host header")?,
        None => uri
            .authority()
            .map(Authority::as_str)
            .ok_or("Bad Request: missing Host header")?,
    };
    if raw.contains('@') {
        return Err("Bad Request: invalid Host header");
    }
    let authority: Authority = raw
        .parse()
        .map_err(|_| "Bad Request: invalid Host header")?;
    // `":80"` parses as an authority with an empty host: never a real name.
    if authority.host().is_empty() {
        return Err("Bad Request: invalid Host header");
    }
    Ok(normalize_host(authority.host()))
}

fn is_state_changing(method: &Method) -> bool {
    !(*method == Method::GET || *method == Method::HEAD || *method == Method::OPTIONS)
}

fn plain(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        message,
    )
        .into_response()
}

/// The guard itself — see the module docs for the rules.
pub async fn require_allowed_host(
    State(guard): State<Arc<HostGuard>>,
    req: Request,
    next: Next,
) -> Response {
    let host = match request_host(req.uri(), req.headers()) {
        Ok(host) => host,
        Err(message) => return plain(StatusCode::BAD_REQUEST, message),
    };
    if !guard.allowed_hosts.contains(&host) {
        tracing::warn!(
            host = %capped_for_log(host.as_bytes()),
            "gateway rejected a request with a disallowed Host header (possible DNS rebinding)"
        );
        return plain(
            StatusCode::FORBIDDEN,
            "Forbidden: Host header is not allowed",
        );
    }
    let path = req.uri().path();
    if is_approvals_path(path) && !LOOPBACK_HOSTS.contains(&host.as_str()) {
        tracing::warn!(
            host = %capped_for_log(host.as_bytes()),
            "gateway refused /approvals on a non-loopback Host (operator surface is loopback-only)"
        );
        return plain(
            StatusCode::FORBIDDEN,
            "Forbidden: /approvals is loopback-only",
        );
    }
    if is_approvals_path(path) && has_proxy_header(req.headers()) {
        tracing::warn!(
            "gateway refused /approvals carrying a reverse-proxy header (operator surface is loopback-only)"
        );
        return plain(
            StatusCode::FORBIDDEN,
            "Forbidden: /approvals is loopback-only",
        );
    }
    if is_state_changing(req.method())
        && !is_mcp_path(path)
        && !request_origin_allowed(req.headers(), guard.auth.issuer())
    {
        tracing::warn!(
            origin = %req
                .headers()
                .get(header::ORIGIN)
                .map(|o| capped_for_log(o.as_bytes()))
                .unwrap_or_default(),
            "gateway rejected a state-changing request from a foreign Origin"
        );
        return plain(
            StatusCode::FORBIDDEN,
            "Forbidden: Origin header is not allowed",
        );
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::HeaderValue;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    use crate::commands::gateway::auth::AuthStore;

    const ISSUER: &str = "http://127.0.0.1:7717";

    /// A small router (`GET`/`POST /x`, `GET /approvals`,
    /// `POST /approvals/{id}`, `POST /mcp`, plus the lookalikes `POST /mcpx`
    /// and `GET /approvalsx` → "ok") behind the guard, with
    /// an `AuthCtx` whose issuer is `public_url` (trimmed) or the loopback
    /// test issuer — mirroring how `gateway::resolve_issuer` picks it.
    fn guard_router(public_url: Option<&str>) -> (tempfile::TempDir, Router) {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::open_at(dir.path().join("auth")).unwrap();
        let ctx = Arc::new(AuthCtx::new(store));
        ctx.issuer
            .set(
                public_url
                    .unwrap_or(ISSUER)
                    .trim_end_matches('/')
                    .to_string(),
            )
            .unwrap();
        let guard = Arc::new(HostGuard::new(allowed_hosts(public_url), ctx));
        let router = Router::new()
            .route("/x", get(|| async { "ok" }).post(|| async { "ok" }))
            .route("/approvals", get(|| async { "ok" }))
            .route("/approvals/{id}", axum::routing::post(|| async { "ok" }))
            .route("/mcp", axum::routing::post(|| async { "ok" }))
            // Lookalike prefixes, routed so the tests prove the PATH match
            // (not a 404) decides.
            .route("/mcpx", axum::routing::post(|| async { "ok" }))
            .route("/approvalsx", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                guard,
                require_allowed_host,
            ));
        (dir, router)
    }

    async fn call(
        router: &Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let resp = router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[test]
    fn allowed_hosts_is_loopback_only_without_public_url() {
        assert_eq!(allowed_hosts(None), vec!["localhost", "127.0.0.1", "::1"]);
    }

    /// `validate_public_url` accepts the bracketed `[::1]`; `allowed_hosts`
    /// stores the bracket-stripped form. They must agree: the IPv6 loopback
    /// public_url adds nothing new (already in the loopback list) and every
    /// shape `validate_public_url` accepts yields a host in the list.
    #[test]
    fn allowed_hosts_agrees_with_validate_public_url_on_ipv6() {
        for url in ["http://[::1]:7717", "http://[::1]", "https://[::1]:7717"] {
            assert!(super::super::validate_public_url(url).is_ok(), "{url}");
            assert_eq!(
                allowed_hosts(Some(url)),
                vec!["localhost", "127.0.0.1", "::1"],
                "{url}"
            );
        }
        assert!(allowed_hosts(Some("https://[2001:db8::1]")).contains(&"2001:db8::1".to_string()));
    }

    #[test]
    fn allowed_hosts_adds_the_normalised_public_url_host_once() {
        assert_eq!(
            allowed_hosts(Some("https://GW.Example.com:8443/")),
            vec!["localhost", "127.0.0.1", "::1", "gw.example.com"]
        );
        // A loopback public_url adds nothing new (deduplicated).
        assert_eq!(allowed_hosts(Some("http://[::1]:7717")).len(), 3);
        // An unparseable public_url fails CLOSED to loopback only.
        assert_eq!(allowed_hosts(Some("not a url")).len(), 3);
    }

    #[test]
    fn origin_matches_issuer_is_an_exact_scheme_host_port_match() {
        let issuer = "https://gw.example.com";
        assert!(origin_matches_issuer("https://gw.example.com", issuer));
        assert!(origin_matches_issuer("https://gw.example.com:443", issuer));
        assert!(origin_matches_issuer("https://GW.example.com", issuer));
        assert!(origin_matches_issuer(
            "http://127.0.0.1:7717",
            "http://127.0.0.1:7717"
        ));
        assert!(!origin_matches_issuer("http://gw.example.com", issuer));
        assert!(!origin_matches_issuer(
            "https://gw.example.com:8443",
            issuer
        ));
        assert!(!origin_matches_issuer("https://evil.example", issuer));
        assert!(!origin_matches_issuer("null", issuer));
        assert!(!origin_matches_issuer(
            "https://gw.example.com/path",
            issuer
        ));
        assert!(!origin_matches_issuer(
            "https://user@gw.example.com",
            issuer
        ));
        assert!(!origin_matches_issuer(
            "ftp://gw.example.com",
            "ftp://gw.example.com"
        ));
        assert!(!origin_matches_issuer(
            "http://localhost:7717",
            "http://127.0.0.1:7717"
        ));
    }

    #[tokio::test]
    async fn loopback_hosts_with_or_without_port_pass() {
        let (_dir, router) = guard_router(None);
        for host in [
            "localhost",
            "localhost:7717",
            "LOCALHOST:7717",
            "127.0.0.1",
            "127.0.0.1:7717",
            "[::1]",
            "[::1]:7717",
        ] {
            let (status, body) = call(&router, "GET", "/x", &[("host", host)]).await;
            assert_eq!(status, StatusCode::OK, "{host}: {body}");
        }
    }

    #[tokio::test]
    async fn foreign_and_lookalike_hosts_are_403() {
        let (_dir, router) = guard_router(None);
        for host in [
            "evil.example",
            "localhost.evil.example",
            "127.0.0.10:7717",
            "gw.example.com",
        ] {
            let (status, body) = call(&router, "GET", "/x", &[("host", host)]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{host}");
            assert_eq!(body, "Forbidden: Host header is not allowed");
        }
    }

    #[tokio::test]
    async fn public_url_host_passes_with_any_port_and_case_only_when_configured() {
        let (_dir, router) = guard_router(Some("https://gw.example.com"));
        for host in ["gw.example.com", "gw.example.com:443", "GW.EXAMPLE.COM"] {
            let (status, _) = call(&router, "GET", "/x", &[("host", host)]).await;
            assert_eq!(status, StatusCode::OK, "{host}");
        }
        let (status, _) = call(&router, "GET", "/x", &[("host", "other.example.com")]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn missing_host_is_400_but_a_uri_authority_is_accepted() {
        let (_dir, router) = guard_router(None);
        let (status, body) = call(&router, "GET", "/x", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, "Bad Request: missing Host header");

        // HTTP/2 carries the host in `:authority` (the URI), not `Host`.
        let (status, _) = call(&router, "GET", "http://localhost:7717/x", &[]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn an_empty_public_url_host_is_never_allowed() {
        assert_eq!(public_url_host("https://:443"), None);
        assert_eq!(allowed_hosts(Some("https://:443")), allowed_hosts(None));
    }

    #[tokio::test]
    async fn an_empty_host_with_a_port_is_400() {
        let (_dir, router) = guard_router(Some("https://:443"));
        for host in [":80", ":", ":443"] {
            let (status, body) = call(&router, "GET", "/x", &[("host", host)]).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{host:?}");
            assert_eq!(body, "Bad Request: invalid Host header");
        }
    }

    #[tokio::test]
    async fn malformed_hosts_are_400() {
        let (_dir, router) = guard_router(None);
        for host in ["evil.example@localhost", "local host", ""] {
            let (status, body) = call(&router, "GET", "/x", &[("host", host)]).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{host:?}");
            assert_eq!(body, "Bad Request: invalid Host header");
        }

        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/x")
            .header("host", HeaderValue::from_bytes(b"\xfflocalhost").unwrap())
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn state_changing_requests_need_the_issuer_origin_when_one_is_sent() {
        let (_dir, router) = guard_router(None);
        let evil = [("host", "localhost"), ("origin", "https://evil.example")];

        for method in ["POST", "PUT", "DELETE", "PATCH"] {
            let (status, body) = call(&router, method, "/x", &evil).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
            assert_eq!(body, "Forbidden: Origin header is not allowed");
        }
        // GET is not state-changing: a foreign Origin is irrelevant there.
        let (status, _) = call(&router, "GET", "/x", &evil).await;
        assert_eq!(status, StatusCode::OK);
        // `Origin: null` (sandboxed / privacy-stripped) is never the issuer.
        let (status, _) = call(
            &router,
            "POST",
            "/x",
            &[("host", "localhost"), ("origin", "null")],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // The issuer's own origin passes, and so does no Origin at all.
        let (status, _) = call(
            &router,
            "POST",
            "/x",
            &[("host", "localhost"), ("origin", ISSUER)],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(&router, "POST", "/x", &[("host", "localhost")]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn non_utf8_origin_on_a_post_is_403() {
        let (_dir, router) = guard_router(None);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/x")
            .header("host", "localhost")
            .header("origin", HeaderValue::from_bytes(b"\xffhttps://x").unwrap())
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Hub ruling: a browser that still serialises `Origin: null` on the
    /// consent form's own POST is recognisable by `Sec-Fetch-Site:
    /// same-origin` (a forbidden header a page cannot forge). `null` with
    /// `cross-site` — a sandboxed iframe or a `data:` page — stays 403.
    #[tokio::test]
    async fn null_origin_passes_only_with_same_origin_fetch_metadata() {
        let (_dir, router) = guard_router(None);
        let (status, _) = call(
            &router,
            "POST",
            "/x",
            &[
                ("host", "localhost"),
                ("origin", "null"),
                ("sec-fetch-site", "same-origin"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for site in ["cross-site", "same-site", "none"] {
            let (status, body) = call(
                &router,
                "POST",
                "/x",
                &[
                    ("host", "localhost"),
                    ("origin", "null"),
                    ("sec-fetch-site", site),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{site}");
            assert_eq!(body, "Forbidden: Origin header is not allowed");
        }
    }

    /// Hub ruling: `/mcp` is exempt from the Origin rule (Bearer auth
    /// already blocks cross-site use) — but NOT from the Host rule.
    #[tokio::test]
    async fn mcp_is_exempt_from_the_origin_rule_but_not_the_host_rule() {
        let (_dir, router) = guard_router(None);
        let (status, _) = call(
            &router,
            "POST",
            "/mcp",
            &[("host", "localhost"), ("origin", "https://evil.example")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(&router, "POST", "/mcp", &[("host", "evil.example")]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // A lookalike prefix is not `/mcp`.
        let (status, _) = call(
            &router,
            "POST",
            "/mcpx",
            &[("host", "localhost"), ("origin", "https://evil.example")],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    /// Red-team item 5: `/approvals` (pairing-code gated) answers ONLY on a
    /// loopback Host — the tunnel host gets 403 even though it is allowed
    /// everywhere else.
    #[tokio::test]
    async fn approvals_is_loopback_only() {
        let (_dir, router) = guard_router(Some("https://gw.example.com"));
        for (method, uri) in [("GET", "/approvals"), ("POST", "/approvals/a1")] {
            let (status, body) = call(&router, method, uri, &[("host", "gw.example.com")]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
            assert_eq!(body, "Forbidden: /approvals is loopback-only");
            for host in ["localhost:7717", "127.0.0.1:7717", "[::1]:7717"] {
                let (status, _) = call(&router, method, uri, &[("host", host)]).await;
                assert_eq!(status, StatusCode::OK, "{method} {uri} via {host}");
            }
        }
        // The tunnel host still reaches every other route.
        let (status, _) = call(&router, "GET", "/x", &[("host", "gw.example.com")]).await;
        assert_eq!(status, StatusCode::OK);
        // A lookalike prefix is not `/approvals`.
        let (status, _) = call(&router, "GET", "/approvalsx", &[("host", "gw.example.com")]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn capped_for_log_bounds_oversized_values_on_a_char_boundary() {
        assert_eq!(
            capped_for_log(b"https://evil.example"),
            "https://evil.example"
        );
        let long = "é".repeat(200); // 400 bytes, 2-byte chars
        let capped = capped_for_log(long.as_bytes());
        assert!(capped.ends_with('…'), "{capped}");
        assert!(capped.len() <= MAX_LOGGED_BYTES + '…'.len_utf8());
        assert_eq!(
            capped_for_log(&[0xff; 300]).chars().count(),
            MAX_LOGGED_BYTES / 3 + 1
        );
    }

    /// Fix round 1 (Ruling 4): a tunnel that rewrites `Host` to the loopback
    /// origin still adds a proxy header — `/approvals` refuses any of them,
    /// whatever the `Host`, before the pairing check.
    #[tokio::test]
    async fn approvals_refuses_any_proxy_header_even_on_a_loopback_host() {
        let (_dir, router) = guard_router(Some("https://gw.example.com"));
        for name in [
            "Forwarded",
            "X-Forwarded-For",
            "x-forwarded-host",
            "X-Real-IP",
            "CF-Connecting-IP",
        ] {
            for (method, uri) in [("GET", "/approvals"), ("POST", "/approvals/a1")] {
                let (status, body) = call(
                    &router,
                    method,
                    uri,
                    &[("host", "127.0.0.1:7717"), (name, "203.0.113.7")],
                )
                .await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{name} {method} {uri}");
                assert_eq!(body, "Forbidden: /approvals is loopback-only");
            }
        }
        let (status, _) = call(&router, "GET", "/approvals", &[("host", "127.0.0.1:7717")]).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// cloudflared adds these headers to EVERY request: only `/approvals`
    /// may care, so every other route (and the `/approvalsx` lookalike)
    /// still answers through the tunnel host and on loopback.
    #[tokio::test]
    async fn proxy_headers_do_not_affect_non_approvals_routes() {
        let (_dir, router) = guard_router(Some("https://gw.example.com"));
        let proxied = |host| {
            [
                ("host", host),
                ("forwarded", "for=203.0.113.7"),
                ("x-forwarded-for", "203.0.113.7"),
                ("x-forwarded-host", "gw.example.com"),
                ("x-real-ip", "203.0.113.7"),
                ("cf-connecting-ip", "203.0.113.7"),
            ]
        };
        for host in ["gw.example.com", "localhost:7717"] {
            for (method, uri) in [
                ("GET", "/x"),
                ("POST", "/x"),
                ("POST", "/mcp"),
                ("GET", "/approvalsx"),
            ] {
                let (status, body) = call(&router, method, uri, &proxied(host)).await;
                assert_eq!(status, StatusCode::OK, "{method} {uri} via {host}: {body}");
            }
        }
    }
}
