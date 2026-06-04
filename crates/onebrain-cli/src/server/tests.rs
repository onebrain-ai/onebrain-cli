//! Router-level tests for the HTTP surface.
//!
//! These drive the assembled [`build_router`] via `tower::ServiceExt::oneshot`
//! against an isolated `tempfile::tempdir()` vault — NO socket is bound and the
//! user's real vault is never touched. They cover the end-to-end request path
//! (routing + auth middleware + handler), complementing the pure unit tests in
//! the sibling modules.

use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt; // `.collect()` on the response body
use std::fs;
use std::path::Path;
use tower::ServiceExt; // `oneshot`

const TOKEN: &str = "test-token-abc123";

/// Stand up an isolated temp vault with a config + a couple notes + a subfolder,
/// returning the held tempdir (drop = cleanup) and a router pointed at it.
///
/// `dist`: optional dist dir to mount (for the static/SPA tests).
fn vault_router(dist: Option<std::path::PathBuf>) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Minimal valid onebrain.yml.
    fs::write(root.join("onebrain.yml"), "qmd_collection: test-vault\n").unwrap();
    // A couple notes + a subfolder.
    fs::create_dir_all(root.join("01-projects")).unwrap();
    fs::write(root.join("README.md"), "# Top note\n").unwrap();
    fs::write(root.join("01-projects/alpha.md"), "# Alpha\nbody\n").unwrap();

    let cfg = ServeConfig::localhost(Some(root.to_path_buf()), 0, TOKEN.to_string(), dist);
    let router = build_router(cfg);
    (dir, router)
}

/// Build a router with NO vault bound (`vault_root: None`) — the daemon's
/// "started without a real vault" state. The vault endpoints must return 503
/// (fix A) instead of touching any filesystem. No vault dir is needed.
fn no_vault_router() -> Router {
    let cfg = ServeConfig::localhost(None, 0, TOKEN.to_string(), None);
    build_router(cfg)
}

/// Send an authenticated GET and return `(status, body-as-string)`.
async fn get_authed(router: &Router, uri: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .uri(uri)
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    send(router, req).await
}

/// Send any request and collect `(status, body-as-string)`.
async fn send(router: &Router, req: Request<Body>) -> (StatusCode, String) {
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// ─────────────────────────────────────────────────────────────────────────
// /api/vault/tree
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn vault_tree_lists_relative_entries_with_kinds() {
    let (_dir, router) = vault_router(None);
    let (status, body) = get_authed(&router, "/api/vault/tree").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let entries = json["entries"].as_array().unwrap();

    // Collect (path, kind) pairs for assertion.
    let has = |p: &str, kind: &str| entries.iter().any(|e| e["path"] == p && e["kind"] == kind);
    assert!(has("README.md", "file"), "README.md file missing: {body}");
    assert!(has("01-projects", "dir"), "subfolder dir missing: {body}");
    assert!(
        has("01-projects/alpha.md", "file"),
        "nested note missing: {body}"
    );
    // onebrain.yml itself is a file in the listing.
    assert!(has("onebrain.yml", "file"), "config file missing: {body}");
    // Paths are relative — no absolute leak.
    assert!(
        entries
            .iter()
            .all(|e| !e["path"].as_str().unwrap().starts_with('/')),
        "a tree path leaked an absolute prefix: {body}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// /api/vault/file
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn vault_file_returns_content_and_rev() {
    let (_dir, router) = vault_router(None);
    let (status, body) = get_authed(&router, "/api/vault/file?path=01-projects/alpha.md").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["path"], "01-projects/alpha.md");
    assert!(
        json["content"].as_str().unwrap().contains("# Alpha"),
        "content missing: {body}"
    );
    // `rev` is present + non-empty (mtime-nanos string).
    assert!(
        !json["rev"].as_str().unwrap().is_empty(),
        "rev missing: {body}"
    );
}

#[tokio::test]
async fn vault_file_traversal_with_dotdot_is_rejected_400() {
    let (_dir, router) = vault_router(None);
    let (status, body) = get_authed(&router, "/api/vault/file?path=../../../../etc/passwd").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    // And it definitely did NOT read /etc/passwd.
    assert!(
        !body.contains("root:"),
        "traversal must not leak /etc/passwd content: {body}"
    );
}

#[tokio::test]
async fn vault_file_absolute_path_is_rejected_400() {
    let (_dir, router) = vault_router(None);
    // `%2Fetc%2Fpasswd` decodes to `/etc/passwd`.
    let (status, _body) = get_authed(&router, "/api/vault/file?path=%2Fetc%2Fpasswd").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn vault_file_missing_is_404() {
    let (_dir, router) = vault_router(None);
    let (status, _body) = get_authed(&router, "/api/vault/file?path=does-not-exist.md").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ─────────────────────────────────────────────────────────────────────────
// /api/config
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn config_returns_parsed_yaml() {
    let (_dir, router) = vault_router(None);
    let (status, body) = get_authed(&router, "/api/config").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["qmd_collection"], "test-vault");
    // Folder defaults round-trip through the config struct.
    assert_eq!(json["folders"]["inbox"], "00-inbox");
}

// ─────────────────────────────────────────────────────────────────────────
// Auth
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn api_without_token_is_401() {
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .uri("/api/config")
        .body(Body::empty())
        .unwrap();
    let (status, _body) = send(&router, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_with_wrong_token_is_401() {
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .uri("/api/config")
        .header("X-OneBrain-Token", "wrong")
        .body(Body::empty())
        .unwrap();
    let (status, _body) = send(&router, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_with_bearer_token_is_200() {
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .uri("/api/config")
        .header("Authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let (status, _body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK);
}

// ─────────────────────────────────────────────────────────────────────────
// No vault bound → 503 on every vault endpoint (fix A). The static surface +
// token still work, but NO filesystem is exposed — in particular a daemon that
// started without a real vault must NOT serve `/etc/passwd`.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn no_vault_tree_is_503() {
    let router = no_vault_router();
    let (status, body) = get_authed(&router, "/api/vault/tree").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert!(body.contains("no vault bound"), "body: {body}");
}

#[tokio::test]
async fn no_vault_file_is_503_not_etc_passwd() {
    let router = no_vault_router();
    // The exact attack the blocker describes: with no vault, this must NOT walk
    // out to /etc/passwd — it must refuse with 503 before any path resolution.
    let (status, body) = get_authed(&router, "/api/vault/file?path=etc/passwd").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert!(
        !body.contains("root:"),
        "no-vault file read must not leak /etc/passwd: {body}"
    );
}

#[tokio::test]
async fn no_vault_config_is_503() {
    let router = no_vault_router();
    let (status, body) = get_authed(&router, "/api/config").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
}

// ─────────────────────────────────────────────────────────────────────────
// Per-request error mapping for the file + config endpoints (fix D + F).
// These drive the full router (routing + auth + handler + spawn_blocking) so
// they complement the pure `read_vault_file` unit tests in `api.rs`.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn vault_file_directory_path_is_400() {
    // `01-projects` is a directory in the fixture vault → 400 "not a file".
    let (_dir, router) = vault_router(None);
    let (status, body) = get_authed(&router, "/api/vault/file?path=01-projects").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(body.contains("not a file"), "body: {body}");
}

#[tokio::test]
async fn vault_file_empty_path_is_400() {
    let (_dir, router) = vault_router(None);
    let (status, body) = get_authed(&router, "/api/vault/file?path=").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn vault_file_non_utf8_is_422() {
    // Write a file with invalid UTF-8 bytes into the fixture vault, then read it.
    let (dir, router) = vault_router(None);
    fs::write(dir.path().join("binary.md"), [0xFFu8, 0xFE, 0x00]).unwrap();
    let (status, body) = get_authed(&router, "/api/vault/file?path=binary.md").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert!(body.contains("UTF-8"), "body: {body}");
}

#[tokio::test]
async fn vault_file_over_cap_is_413() {
    // A file just past the 10 MB cap → 413 (refused before the read).
    let (dir, router) = vault_router(None);
    let big = dir.path().join("big.md");
    let f = fs::File::create(&big).unwrap();
    f.set_len(10 * 1024 * 1024 + 1).unwrap(); // sparse, cheap
    let (status, body) = get_authed(&router, "/api/vault/file?path=big.md").await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "body: {body}");
    assert!(body.contains("too large"), "body: {body}");
}

#[tokio::test]
async fn config_missing_is_404() {
    // Build a router over a vault dir whose config we then delete: a bound vault
    // (Some) but no config file → 404 (fix D).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("onebrain.yml"), "qmd_collection: x\n").unwrap();
    let cfg = ServeConfig::localhost(Some(root.to_path_buf()), 0, TOKEN.to_string(), None);
    let router = build_router(cfg);
    // Remove the config so the load fails with VaultYamlMissing.
    fs::remove_file(root.join("onebrain.yml")).unwrap();
    let (status, body) = get_authed(&router, "/api/config").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
}

#[tokio::test]
async fn config_malformed_is_400() {
    // A config that exists but is invalid YAML → 400 (fix D).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("onebrain.yml"), "not: : valid\n").unwrap();
    let cfg = ServeConfig::localhost(Some(root.to_path_buf()), 0, TOKEN.to_string(), None);
    let router = build_router(cfg);
    let (status, body) = get_authed(&router, "/api/config").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

// ─────────────────────────────────────────────────────────────────────────
// Static / SPA fallback + token injection
// ─────────────────────────────────────────────────────────────────────────

/// Write a dist dir with an `index.html` and a real asset, returning its path
/// (held by the passed-in tempdir).
fn make_dist(dir: &Path) -> std::path::PathBuf {
    let dist = dir.join("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::write(
        dist.join("index.html"),
        "<html><head><title>OneBrain</title></head><body>app shell</body></html>",
    )
    .unwrap();
    fs::write(dist.join("app.js"), "console.log('real asset');").unwrap();
    dist
}

#[tokio::test]
async fn spa_fallback_serves_index_for_unknown_route() {
    let holder = tempfile::tempdir().unwrap();
    let dist = make_dist(holder.path());
    let (_vault, router) = vault_router(Some(dist));

    // An unknown, non-api deep link must return the SPA shell (client routing).
    let req = Request::builder()
        .uri("/v/explorer/some/deep/link")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("app shell"), "expected SPA shell: {body}");
}

#[tokio::test]
async fn served_index_contains_the_injected_token() {
    let holder = tempfile::tempdir().unwrap();
    let dist = make_dist(holder.path());
    let (_vault, router) = vault_router(Some(dist));

    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&format!("window.__ONEBRAIN_TOKEN__=\"{TOKEN}\"")),
        "served index must carry the injected token: {body}"
    );
}

#[tokio::test]
async fn real_static_asset_is_served_directly() {
    let holder = tempfile::tempdir().unwrap();
    let dist = make_dist(holder.path());
    let (_vault, router) = vault_router(Some(dist));

    let req = Request::builder()
        .uri("/app.js")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("real asset"), "expected the JS asset: {body}");
}

#[tokio::test]
async fn no_dist_serves_placeholder_with_token() {
    let (_dir, router) = vault_router(None);
    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("no UI dist mounted"),
        "placeholder body: {body}"
    );
    assert!(
        body.contains(TOKEN),
        "placeholder must carry the token: {body}"
    );
}

#[tokio::test]
async fn static_routes_are_open_without_token() {
    // Static serving is intentionally NOT token-gated (the token rides in the
    // HTML). The root must load with no auth header.
    let (_dir, router) = vault_router(None);
    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let (status, _body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK);
}

// ─────────────────────────────────────────────────────────────────────────
// Full bind + serve + graceful-shutdown integration (fix E).
//
// Unlike every test above (which drives the in-memory `Router` via `oneshot`),
// this exercises the REAL `run_server` path: it binds an ephemeral TCP port,
// fires genuine HTTP/1.1 requests over a `tokio::net::TcpStream`, and then
// asserts that resolving the shutdown future drains the server and returns
// `Ok(())`. This is the only coverage of the socket bind + graceful drain.
//
// Shutdown is driven by a shared `AtomicBool` polled by the shutdown future —
// the workspace tokio build doesn't enable the `sync` feature (no `oneshot`
// channel), and `time` IS enabled, so a short poll loop is the dep-free signal.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_server_binds_serves_and_shuts_down_cleanly() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    // Temp vault so `/api/config` has something real to return.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("onebrain.yml"), "qmd_collection: itest-vault\n").unwrap();

    // Bind an ephemeral port ourselves to learn the real address, then hand the
    // bound port to `ServeConfig` (run_server re-binds it; on 127.0.0.1 the OS
    // reassigns the same free port reliably for a localhost test). To avoid any
    // re-bind race we instead read the port from our probe listener, drop it,
    // and let run_server bind it.
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let cfg = ServeConfig::localhost(
        Some(root.to_path_buf()),
        port,
        TOKEN.to_string(),
        None, // API-only (placeholder static page)
    );

    // Shared shutdown flag; the future polls it until set.
    let flag = Arc::new(AtomicBool::new(false));
    let flag_for_future = flag.clone();
    let shutdown = async move {
        while !flag_for_future.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };

    // Spawn the real server on a task.
    let server = tokio::spawn(async move { run_server(cfg, shutdown).await });

    // Wait until the port actually accepts a connection (bind is async).
    let addr = format!("127.0.0.1:{port}");
    wait_until_listening(&addr).await;

    // (1) An AUTHED GET /api/config returns 200.
    let (status, body) = http_get(&addr, "/api/config", Some(TOKEN)).await;
    assert_eq!(status, 200, "authed config should be 200; body: {body}");
    assert!(
        body.contains("itest-vault"),
        "config body should echo the vault config: {body}"
    );

    // (2) An UNAUTHED GET /api/config returns 401.
    let (status, _body) = http_get(&addr, "/api/config", None).await;
    assert_eq!(status, 401, "unauthed config should be 401");

    // (3) Trip the shutdown flag and assert the server drains + returns Ok(()).
    flag.store(true, Ordering::SeqCst);
    let joined = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server task did not finish within 5s (graceful shutdown hung)")
        .expect("server task panicked");
    assert!(
        joined.is_ok(),
        "run_server should return Ok(()) on graceful shutdown, got: {joined:?}"
    );

    // ── tiny raw-TCP HTTP/1.1 helpers (no reqwest/hyper dev-dep) ──

    /// Poll-connect to `addr` until it accepts, up to ~2s.
    async fn wait_until_listening(addr: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if TcpStream::connect(addr).await.is_ok() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("server never started listening on {addr}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Send a minimal HTTP/1.1 GET (optionally with the token header) and return
    /// `(status_code, body)`. `Connection: close` so the server closes the
    /// socket after the response and our read-to-end terminates.
    async fn http_get(addr: &str, path: &str, token: Option<&str>) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let auth = match token {
            Some(t) => format!("X-OneBrain-Token: {t}\r\n"),
            None => String::new(),
        };
        let req =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{auth}\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        let text = String::from_utf8_lossy(&raw).into_owned();

        // Parse the status code from the status line: `HTTP/1.1 200 OK`.
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("no status code in response: {text}"));

        // Body is everything after the blank line separating headers from body.
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (status, body)
    }
}
