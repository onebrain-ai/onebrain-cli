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
use rust_embed::RustEmbed;
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt; // for ServeDir::oneshot
use tower_http::services::ServeDir;

use super::AppState;

/// The placeholder a dist's `index.html` may contain; replaced verbatim with
/// the live token. SPAs that prefer the script-injection path simply omit it.
const TOKEN_PLACEHOLDER: &str = "__ONEBRAIN_TOKEN__";

/// The web UI built into the binary. The `webui/` folder is populated at build
/// time (the release CI builds onebrain-webui; local dev copies its dist in), and
/// is empty (just `.gitkeep`) in a fresh checkout — in which case `get("index.html")`
/// is `None` and `serve` falls back to the placeholder page.
#[derive(RustEmbed)]
#[folder = "webui/"]
struct WebAssets;

/// Whether this binary was built with a bundled web UI (the `webui/` folder was
/// populated at build time). With no `--dir`, an embedded build still serves the
/// full UI via [`serve_from_embedded`] — so `serve`'s startup banner uses this to
/// report "embedded web UI" instead of the API-only placeholder.
pub fn has_embedded_ui() -> bool {
    WebAssets::get("index.html").is_some()
}

/// The version of the bundled web UI, read from the embedded `version.json`
/// (emitted by the onebrain-webui build). `None` when no UI is bundled, or the
/// marker is absent/malformed (a pre-`version.json` dist) — `serve` then just
/// omits the version line rather than guessing.
pub fn webui_version() -> Option<String> {
    parse_webui_version(&WebAssets::get("version.json")?.data)
}

/// Extract the `version` field from a `version.json` body. Split out from
/// [`webui_version`] so it's unit-testable without an embedded asset and
/// reusable for a `--dir` dist read.
pub fn parse_webui_version(bytes: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Marker {
        version: String,
    }
    serde_json::from_slice::<Marker>(bytes)
        .ok()
        .map(|m| m.version)
        .filter(|v| !v.is_empty()) // an empty version would render a bare "v"
}

/// The release date of the bundled web UI, from the embedded `changelog.json`'s
/// top-level `released` field (onebrain-webui ≥ 0.1.1 emits it next to the same
/// build's `version.json`). `None` when absent/malformed — `serve` then shows
/// the version without a date.
pub fn webui_released() -> Option<String> {
    parse_webui_released(&WebAssets::get("changelog.json")?.data)
}

/// Extract the top-level `released` date from a `changelog.json` body. Split out
/// from [`webui_released`] so it's unit-testable without an embedded asset and
/// reusable for a `--dir` dist read.
pub fn parse_webui_released(bytes: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Changelog {
        released: Option<String>,
    }
    serde_json::from_slice::<Changelog>(bytes)
        .ok()
        .and_then(|c| c.released)
        .filter(|d| !d.is_empty())
}

/// The catch-all static handler, wired into the router via `Router::fallback`
/// so it answers every non-`/api` route. We can't hand a bare `ServeDir` to
/// `fallback` because we need to intercept `index.html` for token injection;
/// this handler:
/// - for the SPA entry (`/` or `/index.html`, or any unknown route via 404
///   fallback), returns the token-injected `index.html`;
/// - otherwise delegates to `ServeDir` for real static assets.
pub async fn serve_static(State(state): State<Arc<AppState>>, request: Request) -> Response {
    match &state.dist_dir {
        // An explicit dist (`--dir` / $ONEBRAIN_DIST) overrides the embedded build
        // — handy for webui development against a live daemon.
        Some(dist) => serve_from_dist(dist, &state.token, request).await,
        // Default: the web UI embedded in the binary (falls back to the placeholder
        // page if this binary was built without a bundled UI).
        None => serve_from_embedded(&state.token, request).await,
    }
}

/// Serve a request against the embedded web UI (`WebAssets`), mirroring
/// `serve_from_dist`: the entry shell + any unknown route returns the
/// token-injected `index.html`; real assets are returned by name.
async fn serve_from_embedded(token: &str, request: Request) -> Response {
    let path = request.uri().path().trim_start_matches('/').to_owned();
    if path.is_empty() || path == "index.html" {
        return embedded_index(token);
    }
    let accepts_gzip = accepts_gzip(&request);
    match WebAssets::get(&path) {
        Some(file) => {
            let mime = file.metadata.mimetype().to_string();
            let data = file.data.into_owned();
            // `assets/` files are gzip-precompressed at build time — detect the
            // gzip magic and hand them back with `Content-Encoding: gzip` (the
            // browser inflates; no server-side work), else inflate for a
            // non-gzip client. Raw files (version.json, …) fall through unchanged.
            if is_gzip(&data) {
                serve_maybe_gzipped(mime, data, accepts_gzip)
            } else {
                ([(header::CONTENT_TYPE, mime)], data).into_response()
            }
        }
        // Unknown route → SPA fallback to the injected entry shell.
        None => embedded_index(token),
    }
}

/// True if `data` begins with the gzip magic (RFC 1952 `1f 8b`). Web assets are
/// text or binary that never start with these two bytes, so this reliably tells
/// a build-time-gzipped asset apart from a raw one — no path/extension guessing,
/// so serving stays correct whether or not the build gzipped anything.
fn is_gzip(data: &[u8]) -> bool {
    data.starts_with(&[0x1f, 0x8b])
}

/// Whether the client's `Accept-Encoding` accepts gzip (all browsers do).
/// Case-insensitive, and honours an explicit `;q=0` refusal (RFC 9110 §12.5.3).
fn accepts_gzip(request: &Request) -> bool {
    request
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(gzip_is_acceptable))
        .unwrap_or(false)
}

/// True if one `Accept-Encoding` entry names gzip without a `q=0` (refusal).
fn gzip_is_acceptable(entry: &str) -> bool {
    let mut parts = entry.split(';').map(str::trim);
    if !parts
        .next()
        .map(|coding| coding.eq_ignore_ascii_case("gzip"))
        .unwrap_or(false)
    {
        return false;
    }
    // A `q=0` qualifier means "not acceptable".
    !parts.any(|p| p.strip_prefix("q=").and_then(|q| q.parse::<f32>().ok()) == Some(0.0))
}

/// Serve gzip-precompressed `data`: pass it through with `Content-Encoding: gzip`
/// when the client accepts gzip (the common browser path — zero server work),
/// otherwise inflate so a non-gzip client still receives valid bytes.
fn serve_maybe_gzipped(mime: String, data: Vec<u8>, accepts_gzip: bool) -> Response {
    // The body varies by Accept-Encoding (gzipped vs inflated) — advertise it so
    // any cache keys on the encoding (correct even though a localhost daemon has
    // no shared cache today).
    let vary = (header::VARY, "accept-encoding".to_string());
    if accepts_gzip {
        return (
            [
                (header::CONTENT_TYPE, mime),
                (header::CONTENT_ENCODING, "gzip".to_string()),
                vary,
            ],
            data,
        )
            .into_response();
    }
    match inflate(&data) {
        Some(raw) => ([(header::CONTENT_TYPE, mime), vary], raw).into_response(),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Inflate a gzip stream with flate2 (pure-Rust `miniz_oxide` — no C, so it
/// cross-compiles cleanly to every release target). Only the rare non-gzip
/// client hits this path.
fn inflate(gz: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(gz);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok().map(|_| out)
}

/// The embedded `index.html`, token-injected. `None` (no bundled UI) → placeholder.
fn embedded_index(token: &str) -> Response {
    match WebAssets::get("index.html") {
        Some(file) => html_response(inject_token(&String::from_utf8_lossy(&file.data), token)),
        None => placeholder_html(token).into_response(),
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
         {script}\
         </head><body>OneBrain daemon — no UI dist mounted</body></html>",
        script = token_script(token),
    );
    html_response(html)
}

/// The `<script>` that defines `window.__ONEBRAIN_TOKEN__`. The value is
/// JSON-escaped (`serde_json`) so it cannot break out of the string literal —
/// the token is fixed-charset hex today, but this keeps the injection safe by
/// construction rather than by assumption.
fn token_script(token: &str) -> String {
    let json = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".to_string());
    format!("<script>window.__ONEBRAIN_TOKEN__={json};</script>")
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

    let script = token_script(token);

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

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn is_gzip_detects_the_magic_bytes() {
        assert!(is_gzip(&gzip(b"payload")));
        assert!(is_gzip(&[0x1f, 0x8b, 0x08, 0x00]));
        assert!(!is_gzip(b"<!doctype html>")); // raw HTML
        assert!(!is_gzip(br#"{"version":"0.1.1"}"#)); // raw JSON
        assert!(!is_gzip(b"")); // empty
        assert!(!is_gzip(&[0x1f])); // one byte, not enough
    }

    #[test]
    fn inflate_roundtrips_a_gzip_stream() {
        let gz = gzip(b"console.log('hi')");
        assert_eq!(inflate(&gz).as_deref(), Some(&b"console.log('hi')"[..]));
        assert_eq!(inflate(b"not a gzip stream"), None); // garbage → None, no panic
    }

    #[test]
    fn accepts_gzip_reads_the_header() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        let req = |accept_encoding: Option<&str>| {
            let mut b = HttpRequest::builder().uri("/assets/x.js");
            if let Some(v) = accept_encoding {
                b = b.header(header::ACCEPT_ENCODING, v);
            }
            b.body(Body::empty()).unwrap()
        };
        assert!(accepts_gzip(&req(Some("gzip, deflate, br"))));
        assert!(accepts_gzip(&req(Some("gzip"))));
        assert!(accepts_gzip(&req(Some("deflate, gzip;q=0.5")))); // q>0 acceptable
        assert!(accepts_gzip(&req(Some("GZIP")))); // case-insensitive
        assert!(!accepts_gzip(&req(Some("gzip;q=0")))); // explicit refusal
        assert!(!accepts_gzip(&req(Some("gzip;q=0.0")))); // explicit refusal
        assert!(!accepts_gzip(&req(Some("deflate, br")))); // no gzip
        assert!(!accepts_gzip(&req(None))); // header absent
    }

    #[test]
    fn serve_maybe_gzipped_honors_accept_encoding() {
        let gz = gzip(b"console.log(1)");
        // Accepts gzip → passthrough + Content-Encoding: gzip (no server inflate),
        // with Vary: accept-encoding.
        let resp = serve_maybe_gzipped("text/javascript".into(), gz.clone(), true);
        assert_eq!(
            resp.headers().get(header::CONTENT_ENCODING).unwrap(),
            "gzip"
        );
        assert_eq!(resp.headers().get(header::VARY).unwrap(), "accept-encoding");
        // Doesn't accept gzip → inflated, no Content-Encoding, still Vary.
        let resp = serve_maybe_gzipped("text/javascript".into(), gz, false);
        assert!(resp.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(resp.headers().get(header::VARY).unwrap(), "accept-encoding");
    }

    #[test]
    fn parse_webui_version_reads_the_version_field() {
        assert_eq!(
            parse_webui_version(br#"{"version":"0.1.1"}"#).as_deref(),
            Some("0.1.1")
        );
        // The build emits a trailing newline — serde_json tolerates it.
        assert_eq!(
            parse_webui_version(b"{\"version\":\"1.2.3\"}\n").as_deref(),
            Some("1.2.3")
        );
        // Extra fields are ignored.
        assert_eq!(
            parse_webui_version(br#"{"version":"9.9.9","built":"today"}"#).as_deref(),
            Some("9.9.9")
        );
    }

    #[test]
    fn parse_webui_version_rejects_malformed_or_missing() {
        assert_eq!(parse_webui_version(b""), None); // empty body
        assert_eq!(parse_webui_version(b"not json"), None); // not JSON
        assert_eq!(parse_webui_version(br#"{"other":"x"}"#), None); // no `version`
        assert_eq!(parse_webui_version(br#"{"version":123}"#), None); // wrong type
        assert_eq!(parse_webui_version(br#"{"version":""}"#), None); // empty string
    }

    #[test]
    fn parse_webui_released_reads_the_date() {
        assert_eq!(
            parse_webui_released(br#"{"latest":"0.1.1","released":"2026-07-01","entries":[]}"#)
                .as_deref(),
            Some("2026-07-01")
        );
    }

    #[test]
    fn parse_webui_released_rejects_missing_or_malformed() {
        assert_eq!(parse_webui_released(b""), None); // empty body
        assert_eq!(parse_webui_released(b"not json"), None); // not JSON
        assert_eq!(parse_webui_released(br#"{"latest":"0.1.1"}"#), None); // no `released`
        assert_eq!(parse_webui_released(br#"{"released":null}"#), None); // explicit null
        assert_eq!(parse_webui_released(br#"{"released":""}"#), None); // empty string
        assert_eq!(parse_webui_released(br#"{"released":123}"#), None); // wrong type
    }

    #[test]
    fn webui_version_absent_when_no_ui_embedded() {
        // A fresh checkout embeds only `.gitkeep` (the CI test/coverage build),
        // so no `version.json` is present and the accessor reports None. Guarded
        // by `has_embedded_ui` so a locally dist-embedded build — where a real
        // `version.json` IS bundled — still passes.
        if !has_embedded_ui() {
            assert_eq!(webui_version(), None);
            assert_eq!(webui_released(), None);
        }
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
