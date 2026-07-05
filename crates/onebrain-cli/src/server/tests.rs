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

/// Send an authed request with a body + content-type; return (status, body).
async fn send_authed(
    router: &Router,
    method: &str,
    uri: &str,
    content_type: &str,
    body: &str,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-OneBrain-Token", TOKEN)
        .header("content-type", content_type)
        .body(Body::from(body.to_string()))
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

#[tokio::test]
async fn read_endpoints_reject_tooling_dir_paths() {
    // Tooling dirs are hidden from the tree + can hold secrets (e.g.
    // .claude/settings.local.json); reads must refuse them like writes do, so a
    // direct path read can't bypass the tree's hiding.
    let (_dir, router) = vault_router(None);
    for path in [
        ".claude/settings.local.json",
        ".git/config",
        ".obsidian/app.json",
    ] {
        let (s, b) = get_authed(&router, &format!("/api/vault/file?path={path}")).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "file {path}: {b}");
        let (s2, b2) = get_authed(&router, &format!("/api/vault/raw?path={path}")).await;
        assert_eq!(s2, StatusCode::BAD_REQUEST, "raw {path}: {b2}");
    }
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/vault/file
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_file_creates_note() {
    let (_dir, router) = vault_router(None);
    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/vault/file?path=01-projects/new.md",
        "text/markdown",
        "# New\nhi\n",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["path"], "01-projects/new.md");
    assert!(!json["rev"].as_str().unwrap().is_empty());

    // Second create on the same path → 409.
    let (status2, _) = send_authed(
        &router,
        "POST",
        "/api/vault/file?path=01-projects/new.md",
        "text/markdown",
        "dupe",
    )
    .await;
    assert_eq!(status2, StatusCode::CONFLICT);
}

// ─────────────────────────────────────────────────────────────────────────
// PUT /api/vault/file
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn put_file_saves_and_bumps_rev() {
    let (_dir, router) = vault_router(None);

    let (_s, body) = get_authed(&router, "/api/vault/file?path=01-projects/alpha.md").await;
    let rev = serde_json::from_str::<serde_json::Value>(&body).unwrap()["rev"]
        .as_str()
        .unwrap()
        .to_string();

    let req = Request::builder()
        .method("PUT")
        .uri("/api/vault/file?path=01-projects/alpha.md")
        .header("X-OneBrain-Token", TOKEN)
        .header("content-type", "text/markdown")
        .header("If-Match", &rev)
        .body(Body::from("# Alpha edited\n"))
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let req2 = Request::builder()
        .method("PUT")
        .uri("/api/vault/file?path=01-projects/alpha.md")
        .header("X-OneBrain-Token", TOKEN)
        .header("content-type", "text/markdown")
        .header("If-Match", &rev) // stale now
        .body(Body::from("conflict"))
        .unwrap();
    let (status2, body2) = send(&router, req2).await;
    assert_eq!(status2, StatusCode::CONFLICT, "body: {body2}");
    assert!(serde_json::from_str::<serde_json::Value>(&body2).unwrap()["rev"].is_string());
}

#[tokio::test]
async fn put_missing_file_is_404() {
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .method("PUT")
        .uri("/api/vault/file?path=does/not/exist.md")
        .header("X-OneBrain-Token", TOKEN)
        .header("content-type", "text/markdown")
        .header("If-Match", "*")
        .body(Body::from("x"))
        .unwrap();
    let (status, _) = send(&router, req).await;
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
async fn robots_txt_is_served_without_a_token() {
    // The single unauthenticated route: a token-less GET /robots.txt returns the
    // private-instance robots.txt (Disallow: /), NOT a 401 like every other route.
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .uri("/robots.txt")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("User-agent: *"), "body: {body}");
    assert!(body.contains("Disallow: /"), "body: {body}");
}

#[tokio::test]
async fn robots_txt_head_is_served_without_a_token() {
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .method("HEAD")
        .uri("/robots.txt")
        .body(Body::empty())
        .unwrap();
    let (status, _body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn robots_txt_post_without_token_is_still_401() {
    // The exemption is GET/HEAD only — a mutating verb on the same path must NOT
    // slip past the token gate (keeps the exemption off the CSRF surface).
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .method("POST")
        .uri("/robots.txt")
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
// POST /api/vault/move
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn move_renames_and_returns_result() {
    let (dir, router) = vault_router(None);
    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/vault/move",
        "application/json",
        r#"{"from":"01-projects/alpha.md","to":"01-projects/beta.md"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["from"], "01-projects/alpha.md");
    assert_eq!(json["to"], "01-projects/beta.md");
    // filesystem actually moved
    assert!(
        !dir.path().join("01-projects/alpha.md").exists(),
        "source still present"
    );
    assert!(
        dir.path().join("01-projects/beta.md").exists(),
        "dest missing"
    );
}

#[tokio::test]
async fn move_missing_source_is_404() {
    let (_dir, router) = vault_router(None);
    let (status, _) = send_authed(
        &router,
        "POST",
        "/api/vault/move",
        "application/json",
        r#"{"from":"nope/missing.md","to":"01-projects/x.md"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn move_to_existing_dest_is_409() {
    let (_dir, router) = vault_router(None);
    let (status, _) = send_authed(
        &router,
        "POST",
        "/api/vault/move",
        "application/json",
        r#"{"from":"01-projects/alpha.md","to":"README.md"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn post_create_in_new_subdir_creates_parents() {
    let (dir, router) = vault_router(None);
    let (status, _) = send_authed(
        &router,
        "POST",
        "/api/vault/file?path=04-resources/deep/new.md",
        "text/markdown",
        "# Deep\n",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(dir.path().join("04-resources/deep/new.md").exists());
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
        .header("X-OneBrain-Token", TOKEN)
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

    let req = Request::builder()
        .uri("/")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
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
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("real asset"), "expected the JS asset: {body}");
}

#[tokio::test]
async fn no_dir_serve_injects_the_token() {
    // With no --dir / $ONEBRAIN_DIST, `serve` falls back to the EMBEDDED web UI
    // (or, when the binary was built without a bundled UI, the placeholder page).
    // Either way the entry shell must carry the session token. (The placeholder
    // itself is unit-tested in server::static::placeholder_page_carries_the_token.)
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .uri("/")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(TOKEN),
        "the no-dir entry shell must carry the token: {body}"
    );
}

#[tokio::test]
async fn every_response_carries_security_headers() {
    // The headers layer is outermost, so even an unauthenticated 401 is stamped.
    let (_dir, router) = vault_router(None);
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let h = res.headers();
    assert_eq!(h.get("x-frame-options").unwrap(), "SAMEORIGIN");
    assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(h.get("referrer-policy").unwrap(), "no-referrer");
    assert!(h.get("content-security-policy").is_some());
}

#[tokio::test]
async fn static_routes_require_a_token() {
    // The static SPA is token-gated too — closing the hole where opening the URL
    // with no `?token=` still loaded the (token-bearing) shell.
    let (_dir, router) = vault_router(None);
    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let (status, _body) = send(&router, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "static must be gated");
}

#[tokio::test]
async fn static_entry_with_query_token_seeds_the_cookie() {
    // The page-load entry: `/?token=…` authenticates AND sets the onebrain_token
    // cookie so the browser's later token-less asset requests authenticate.
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .uri(format!("/?token={TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let set_cookie = res
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        set_cookie.contains(&format!("onebrain_token={TOKEN}")),
        "must seed the token cookie: {set_cookie}"
    );
    assert!(
        set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"),
        "cookie must be HttpOnly + SameSite=Strict: {set_cookie}"
    );

    // And the seeded cookie then authenticates a token-less asset/read request.
    let req2 = Request::builder()
        .uri("/")
        .header("cookie", format!("onebrain_token={TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let (status2, _b) = send(&router, req2).await;
    assert_eq!(
        status2,
        StatusCode::OK,
        "cookie must authenticate the next load"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// DELETE /api/vault/file
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_file_moves_to_trash() {
    let (dir, router) = vault_router(None);
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/vault/file?path=01-projects/alpha.md")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["trashed_to"], ".trash/01-projects/alpha.md");
    assert!(
        !dir.path().join("01-projects/alpha.md").exists(),
        "source still present"
    );
    assert!(
        dir.path().join(".trash/01-projects/alpha.md").exists(),
        "trashed copy missing"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// resolve_in_vault_create — unit tests (no async needed)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn resolve_create_allows_missing_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let p = super::api::resolve_in_vault_create(root, "01-projects/brand-new.md");
    assert!(
        p.is_ok(),
        "missing target should be allowed for create: {p:?}"
    );
    assert_eq!(
        p.unwrap(),
        root.canonicalize()
            .unwrap()
            .join("01-projects/brand-new.md")
    );
}

#[test]
fn resolve_create_rejects_parent_escape() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(super::api::resolve_in_vault_create(root, "../evil.md").is_err());
    assert!(super::api::resolve_in_vault_create(root, "/etc/passwd").is_err());
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/vault/tree — excluded directories
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tree_excludes_trash_and_dotdirs() {
    let (_dir, router) = vault_router(None);
    // Delete README.md so .trash/ exists with content.
    let del = Request::builder()
        .method("DELETE")
        .uri("/api/vault/file?path=README.md")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (del_status, _) = send(&router, del).await;
    assert_eq!(
        del_status,
        StatusCode::OK,
        "delete should succeed so .trash is populated"
    );

    let (status, body) = get_authed(&router, "/api/vault/tree").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains(".trash"), "tree leaked .trash: {body}");
    assert!(!body.contains(".git"), "tree leaked .git: {body}");
    assert!(!body.contains(".obsidian"), "tree leaked .obsidian: {body}");
}

// ─────────────────────────────────────────────────────────────────────────
// POST + DELETE /api/vault/folder
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_and_delete_folder() {
    let (dir, router) = vault_router(None);

    let (s1, b1) = send_authed(
        &router,
        "POST",
        "/api/vault/folder?path=01-projects/zeta",
        "text/plain",
        "",
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "body: {b1}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&b1).unwrap()["path"],
        "01-projects/zeta"
    );
    assert!(
        dir.path().join("01-projects/zeta").is_dir(),
        "folder not created on disk"
    );

    let req = Request::builder()
        .method("DELETE")
        .uri("/api/vault/folder?path=01-projects/zeta")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (s2, b2) = send(&router, req).await;
    assert_eq!(s2, StatusCode::OK, "body: {b2}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&b2).unwrap()["trashed_to"],
        ".trash/01-projects/zeta"
    );
    assert!(
        !dir.path().join("01-projects/zeta").exists(),
        "folder still present"
    );
    assert!(
        dir.path().join(".trash/01-projects/zeta").is_dir(),
        "folder not in trash"
    );
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

// ─────────────────────────────────────────────────────────────────────────
// FIX 1: large note round-trip (body limit matches the 10MB read cap)
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn large_note_round_trips() {
    let (_dir, router) = vault_router(None);

    // 3 MB body — above the 2MB axum default but well under the 10MB cap.
    let big_body = "x".repeat(3 * 1024 * 1024);

    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/vault/file?path=01-projects/large.md",
        "text/markdown",
        &big_body,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "3MB POST should be CREATED, not 413; body: {body}"
    );

    // GET it back and verify the content length matches.
    let (get_status, get_body) =
        get_authed(&router, "/api/vault/file?path=01-projects/large.md").await;
    assert_eq!(
        get_status,
        StatusCode::OK,
        "GET after large POST; body: {get_body}"
    );
    let json: serde_json::Value = serde_json::from_str(&get_body).unwrap();
    let content = json["content"].as_str().unwrap();
    assert_eq!(
        content.len(),
        big_body.len(),
        "round-tripped content length mismatch"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// FIX 2: writes to tooling dirs are rejected
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn write_to_tooling_dir_is_rejected() {
    let (_dir, router) = vault_router(None);

    // POST into .git
    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/vault/file?path=.git/hack.md",
        "text/markdown",
        "evil\n",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "POST into .git must be 400; body: {body}"
    );

    // POST a folder into .obsidian
    let (status2, body2) = send_authed(
        &router,
        "POST",
        "/api/vault/folder?path=.obsidian/x",
        "text/plain",
        "",
    )
    .await;
    assert_eq!(
        status2,
        StatusCode::BAD_REQUEST,
        "POST folder into .obsidian must be 400; body: {body2}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// FIX 3: DELETE /api/vault/file must not trash a directory
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_file_on_directory_is_rejected() {
    let (_dir, router) = vault_router(None);

    // `01-projects` exists as a directory in the fixture vault.
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/vault/file?path=01-projects")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "DELETE file on a directory must be 400; body: {body}"
    );
    assert!(
        body.contains("not a file"),
        "error message should mention 'not a file': {body}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// FIX 4: move endpoint exercises wikilink rewrite
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn move_rewrites_incoming_wikilinks() {
    let (_dir, router) = vault_router(None);

    // Create a referencing note via the API.
    let (create_status, create_body) = send_authed(
        &router,
        "POST",
        "/api/vault/file?path=01-projects/ref.md",
        "text/markdown",
        "see [[alpha]]\n",
    )
    .await;
    assert_eq!(
        create_status,
        StatusCode::CREATED,
        "create ref.md failed; body: {create_body}"
    );

    // Move alpha.md → renamed.md via the HTTP endpoint.
    let (move_status, move_body) = send_authed(
        &router,
        "POST",
        "/api/vault/move",
        "application/json",
        r#"{"from":"01-projects/alpha.md","to":"01-projects/renamed.md"}"#,
    )
    .await;
    assert_eq!(
        move_status,
        StatusCode::OK,
        "move failed; body: {move_body}"
    );

    let move_json: serde_json::Value = serde_json::from_str(&move_body).unwrap();
    let links_rewritten = move_json["links_rewritten"]
        .as_u64()
        .expect("links_rewritten must be a number");
    assert!(
        links_rewritten >= 1,
        "expected at least 1 link rewritten but got {links_rewritten}; move result: {move_body}"
    );

    // GET ref.md and verify the link was updated.
    let (get_status, get_body) =
        get_authed(&router, "/api/vault/file?path=01-projects/ref.md").await;
    assert_eq!(get_status, StatusCode::OK, "GET ref.md; body: {get_body}");
    let ref_json: serde_json::Value = serde_json::from_str(&get_body).unwrap();
    let ref_content = ref_json["content"].as_str().unwrap();
    assert!(
        !ref_content.contains("[[alpha]]"),
        "old link [[alpha]] must be gone from ref.md; content: {ref_content}"
    );
    assert!(
        ref_content.contains("[[renamed]]"),
        "new link [[renamed]] must appear in ref.md; content: {ref_content}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Tooling-dir guard bypass vectors (hardened fix)
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn write_to_tooling_dir_bypass_rejected() {
    let (_dir, router) = vault_router(None);

    // Each of these must return 400: leading ./, uppercase, and nested component.
    for path in [
        ".git/hack.md",
        "./.git/hack.md",
        ".GIT/hack.md",
        "01-projects/.git/x.md",
    ] {
        let (status, body) = send_authed(
            &router,
            "POST",
            &format!("/api/vault/file?path={path}"),
            "text/markdown",
            "x",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "path {path} should be rejected (400); body: {body}"
        );
    }

    // Nested .obsidian via the folder endpoint.
    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/vault/folder?path=01-projects/.obsidian",
        "text/plain",
        "",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "nested .obsidian folder should be rejected (400); body: {body}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// FIX 5: write routes reject path traversal
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn write_route_rejects_traversal() {
    let (_dir, router) = vault_router(None);

    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/vault/file?path=../escape.md",
        "text/markdown",
        "evil\n",
    )
    .await;
    // 400 or 404 — any client error except 401
    assert!(
        status.is_client_error() && status != StatusCode::UNAUTHORIZED,
        "traversal POST must be rejected (4xx ≠ 401); got {status}; body: {body}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// /api/vault/tasks
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn vault_tasks_returns_only_dated_checkboxes_outside_skipped_folders() {
    let (dir, router) = vault_router(None);
    // A dated task (kept), an undated checkbox (excluded), and a dated task in a
    // skipped folder (07-logs — excluded).
    fs::write(
        dir.path().join("01-projects/todo.md"),
        "# Todo\n\n- [ ] ship it ⏫ 📅 2026-06-30\n- [ ] no date here\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("07-logs")).unwrap();
    fs::write(
        dir.path().join("07-logs/s.md"),
        "- [ ] log task 📅 2026-06-30\n",
    )
    .unwrap();

    let (status, body) = get_authed(&router, "/api/vault/tasks").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let tasks = json["tasks"].as_array().unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "only the one dated, non-skipped task: {body}"
    );
    let t = &tasks[0];
    assert_eq!(t["file"], "01-projects/todo.md");
    assert_eq!(t["due"], "2026-06-30");
    assert_eq!(t["done"], false);
    assert_eq!(t["text"], "ship it ⏫", "📅 stripped, priority kept");
    assert_eq!(t["line"], 3, "1-based line within the note");
}

/// The task scan reads the project/area folder names from onebrain.yml, and a
/// trailing slash on those names (a natural hand-edit) must not break it. Here
/// the vault renames its folders to `Work/` + `Life/` (with trailing slashes) —
/// tasks under those are returned, the default `01-projects` is now excluded,
/// and the trailing slash does NOT produce a `//` prefix that matches nothing.
#[tokio::test]
async fn vault_tasks_honors_custom_folder_names_with_trailing_slash() {
    let (dir, router) = vault_router(None);
    fs::write(
        dir.path().join("onebrain.yml"),
        "qmd_collection: test-vault\nfolders:\n  projects: Work/\n  areas: Life/\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("Work")).unwrap();
    fs::create_dir_all(dir.path().join("Life")).unwrap();
    fs::write(
        dir.path().join("Work/p.md"),
        "- [ ] work task 📅 2026-06-30\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("Life/h.md"),
        "- [ ] life task 📅 2026-06-30\n",
    )
    .unwrap();
    // A task in the default 01-projects must now be EXCLUDED (folders are custom).
    fs::write(
        dir.path().join("01-projects/x.md"),
        "- [ ] default-folder task 📅 2026-06-30\n",
    )
    .unwrap();

    let (status, body) = get_authed(&router, "/api/vault/tasks").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let files: Vec<&str> = json["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["file"].as_str().unwrap())
        .collect();
    assert_eq!(files.len(), 2, "only the custom-folder tasks: {body}");
    assert!(files.contains(&"Work/p.md"));
    assert!(files.contains(&"Life/h.md"));
    assert!(
        !files.contains(&"01-projects/x.md"),
        "default folder excluded under custom config"
    );
}

/// When the config can't be loaded at all, the task scan falls back to the
/// DEFAULT allowlist (01-projects/ + 02-areas/) instead of failing. Delete
/// onebrain.yml so the in-handler `load_vault_config_at` errors, then a dated
/// task under the default project folder must still be returned.
#[tokio::test]
async fn vault_tasks_without_config_falls_back_to_default_folders() {
    let (dir, router) = vault_router(None);
    fs::write(
        dir.path().join("01-projects/dated.md"),
        "- [ ] default-folder task 📅 2026-06-30\n",
    )
    .unwrap();
    // Remove the config so the load inside get_vault_tasks fails → default opts.
    fs::remove_file(dir.path().join("onebrain.yml")).unwrap();

    let (status, body) = get_authed(&router, "/api/vault/tasks").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let files: Vec<&str> = json["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["file"].as_str().unwrap())
        .collect();
    assert!(
        files.contains(&"01-projects/dated.md"),
        "default-folder task should scan even without a config: {body}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Symlink-escape on the WRITE path (resolve_in_vault_create) — security
// ─────────────────────────────────────────────────────────────────────────

/// A write whose parent component is a symlink pointing OUTSIDE the vault must
/// be refused, even though the path is lexically clean (no `..`). The create
/// path can't canonicalize the not-yet-existing target, so it canonicalizes the
/// deepest existing ancestor and checks it stays inside the vault.
#[cfg(unix)]
#[tokio::test]
async fn write_through_symlinked_dir_escaping_vault_is_rejected() {
    let (dir, router) = vault_router(None);
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();

    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/vault/file?path=escape/pwned.md",
        "text/markdown",
        "evil\n",
    )
    .await;
    assert!(
        status.is_client_error() && status != StatusCode::UNAUTHORIZED,
        "symlink-escape write must be rejected (4xx ≠ 401); got {status}; body: {body}"
    );
    assert!(
        !outside.path().join("pwned.md").exists(),
        "a write escaped the vault through a symlinked directory"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/vault/raw — raw-bytes serving (content-type guess, forced
// attachment, byte ranges, size cap). The pure helpers (content_type_for,
// forces_attachment, parse_byte_range, content_disposition_attachment) are
// unit-tested in api.rs; these drive the full handler wiring end-to-end.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn raw_serves_image_bytes_with_guessed_content_type() {
    let (dir, router) = vault_router(None);
    // A short PNG-signature-ish byte blob (not valid UTF-8 → proves the raw path
    // is byte-faithful, unlike the text /file endpoint which would 422 on this).
    let bytes = [
        0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF, 0x10,
    ];
    fs::write(dir.path().join("img.png"), bytes).unwrap();

    let req = Request::builder()
        .uri("/api/vault/raw?path=img.png")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("content-type").unwrap(), "image/png");
    // Range support is advertised so the browser knows it can seek media.
    assert_eq!(res.headers().get("accept-ranges").unwrap(), "bytes");
    // No attachment forced for a plain image.
    assert!(res.headers().get("content-disposition").is_none());
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], &bytes[..], "raw bytes must round-trip unchanged");
}

#[tokio::test]
async fn raw_forces_attachment_for_scriptable_svg() {
    // An SVG can carry script; the raw endpoint must serve it as an octet-stream
    // attachment so it can never render+run in the app origin.
    let (dir, router) = vault_router(None);
    fs::write(dir.path().join("pic.svg"), "<svg onload=\"x\"></svg>").unwrap();

    let req = Request::builder()
        .uri("/api/vault/raw?path=pic.svg")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/octet-stream",
        "scriptable types must NOT keep their real image/svg+xml type"
    );
    let cd = res
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(cd.contains("attachment"), "must force attachment: {cd}");
    assert!(cd.contains("pic.svg"), "must carry the real filename: {cd}");
}

#[tokio::test]
async fn raw_download_param_forces_attachment_keeping_type() {
    // `&download=1` on a normal image forces the attachment disposition but the
    // content-type stays the real image type (download, not a security force).
    let (dir, router) = vault_router(None);
    fs::write(dir.path().join("photo.png"), [1u8, 2, 3, 4]).unwrap();

    let req = Request::builder()
        .uri("/api/vault/raw?path=photo.png&download=1")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("content-type").unwrap(), "image/png");
    let cd = res
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        cd.contains("attachment"),
        "download=1 forces attachment: {cd}"
    );
    assert!(cd.contains("photo.png"), "keeps the real filename: {cd}");
}

#[tokio::test]
async fn raw_byte_range_returns_206_partial_slice() {
    let (dir, router) = vault_router(None);
    // 10 bytes "0123456789" in a non-forced type so the range path is honoured.
    fs::write(dir.path().join("media.png"), b"0123456789").unwrap();

    let req = Request::builder()
        .uri("/api/vault/raw?path=media.png")
        .header("X-OneBrain-Token", TOKEN)
        .header("range", "bytes=2-5")
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.headers().get("content-range").unwrap(), "bytes 2-5/10");
    assert_eq!(res.headers().get("content-type").unwrap(), "image/png");
    assert_eq!(res.headers().get("accept-ranges").unwrap(), "bytes");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"2345", "inclusive [2,5] slice");
}

#[tokio::test]
async fn raw_unsatisfiable_range_falls_back_to_full_200() {
    // A range whose start is past EOF is unsatisfiable → the handler ignores it
    // and serves the full 200 body (it never emits a 416).
    let (dir, router) = vault_router(None);
    fs::write(dir.path().join("media.png"), b"0123456789").unwrap();

    let req = Request::builder()
        .uri("/api/vault/raw?path=media.png")
        .header("X-OneBrain-Token", TOKEN)
        .header("range", "bytes=100-200")
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "unsatisfiable range → full 200"
    );
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 10, "full body served");
}

#[tokio::test]
async fn raw_missing_is_404() {
    let (_dir, router) = vault_router(None);
    let (status, _body) = get_authed(&router, "/api/vault/raw?path=nope.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn raw_directory_path_is_400() {
    // `01-projects` is a directory in the fixture vault → 400 "not a file".
    let (_dir, router) = vault_router(None);
    let (status, body) = get_authed(&router, "/api/vault/raw?path=01-projects").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(body.contains("not a file"), "body: {body}");
}

#[tokio::test]
async fn raw_over_cap_is_413() {
    // A file just past the 50 MB raw cap → 413, refused before the read (sparse).
    let (dir, router) = vault_router(None);
    let big = dir.path().join("big.bin");
    let f = fs::File::create(&big).unwrap();
    f.set_len(50 * 1024 * 1024 + 1).unwrap(); // sparse, cheap
    let (status, body) = get_authed(&router, "/api/vault/raw?path=big.bin").await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "body: {body}");
    assert!(body.contains("too large"), "body: {body}");
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/vault/upload — raw binary write (chat attachments)
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn upload_writes_raw_bytes_and_creates_parents() {
    let (dir, router) = vault_router(None);
    // Body need not be valid markdown; deep path proves parents are created.
    let payload = "attachment-bytes-\u{0}\u{1}\u{2}-end";
    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/vault/upload?path=attachments/img/a.bin",
        "application/octet-stream",
        payload,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["path"], "attachments/img/a.bin");
    let on_disk = fs::read(dir.path().join("attachments/img/a.bin")).unwrap();
    assert_eq!(on_disk, payload.as_bytes(), "uploaded bytes must match");
}

#[tokio::test]
async fn upload_rejects_tooling_and_traversal_paths() {
    let (_dir, router) = vault_router(None);
    for path in [".git/x.bin", "../escape.bin"] {
        let (status, body) = send_authed(
            &router,
            "POST",
            &format!("/api/vault/upload?path={path}"),
            "application/octet-stream",
            "x",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "upload to {path} must be 400; body: {body}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────
// PUT /api/vault/file — If-Match variants + tooling guard
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn put_with_star_if_match_overwrites_unconditionally() {
    let (dir, router) = vault_router(None);
    let req = Request::builder()
        .method("PUT")
        .uri("/api/vault/file?path=01-projects/alpha.md")
        .header("X-OneBrain-Token", TOKEN)
        .header("content-type", "text/markdown")
        .header("If-Match", "*") // wildcard = overwrite whatever's there
        .body(Body::from("# Star overwrite\n"))
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let on_disk = fs::read_to_string(dir.path().join("01-projects/alpha.md")).unwrap();
    assert!(on_disk.contains("Star overwrite"), "disk: {on_disk}");
}

#[tokio::test]
async fn put_without_if_match_is_precondition_required() {
    let (dir, router) = vault_router(None);
    // The revision contract is mandatory: omitting If-Match is a 428, never a
    // silent overwrite (explicit force = `If-Match: *`).
    let before = std::fs::read_to_string(dir.path().join("01-projects/alpha.md")).unwrap();
    let req = Request::builder()
        .method("PUT")
        .uri("/api/vault/file?path=01-projects/alpha.md")
        .header("X-OneBrain-Token", TOKEN)
        .header("content-type", "text/markdown")
        .body(Body::from("# Headerless overwrite\n"))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    // And the file is untouched.
    let after = std::fs::read_to_string(dir.path().join("01-projects/alpha.md")).unwrap();
    assert_eq!(before, after, "missing If-Match must not write");
}

#[tokio::test]
async fn delete_folder_empty_path_is_bad_request_not_vault_root_trash() {
    let (dir, router) = vault_router(None);
    // "" used to resolve to the vault root itself — the folder-delete endpoint
    // would then trash the whole vault. Now a 400 from resolve_in_vault.
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/vault/folder?path=")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        dir.path().join("01-projects").exists(),
        "vault must be untouched"
    );
}

#[tokio::test]
async fn put_to_tooling_dir_is_400() {
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .method("PUT")
        .uri("/api/vault/file?path=.git/config")
        .header("X-OneBrain-Token", TOKEN)
        .header("content-type", "text/markdown")
        .header("If-Match", "*")
        .body(Body::from("evil"))
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(body.contains("tooling directory"), "body: {body}");
}

// ─────────────────────────────────────────────────────────────────────────
// DELETE /api/vault/file — missing target
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_missing_file_is_404() {
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/vault/file?path=01-projects/ghost.md")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (status, _body) = send(&router, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/vault/folder — conflict; DELETE /api/vault/folder — error branches
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_folder_over_existing_is_409() {
    // `01-projects` already exists in the fixture vault.
    let (_dir, router) = vault_router(None);
    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/vault/folder?path=01-projects",
        "text/plain",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert!(body.contains("already exists"), "body: {body}");
}

#[tokio::test]
async fn delete_folder_missing_is_404() {
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/vault/folder?path=01-projects/ghostdir")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (status, _body) = send(&router, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_folder_on_a_file_is_400() {
    // README.md is a file → the FOLDER endpoint must refuse it (mirror guard).
    let (_dir, router) = vault_router(None);
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/vault/folder?path=README.md")
        .header("X-OneBrain-Token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(body.contains("not a folder"), "body: {body}");
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/vault/move — body rejection + tooling guard
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn move_missing_field_body_is_422() {
    // Valid JSON but missing the required `to` field → axum Json data error → 422.
    let (_dir, router) = vault_router(None);
    let (status, _body) = send_authed(
        &router,
        "POST",
        "/api/vault/move",
        "application/json",
        r#"{"from":"01-projects/alpha.md"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn move_wrong_content_type_is_415() {
    // Right shape, wrong content-type → axum Json missing-content-type → 415.
    let (_dir, router) = vault_router(None);
    let (status, _body) = send_authed(
        &router,
        "POST",
        "/api/vault/move",
        "text/plain",
        r#"{"from":"01-projects/alpha.md","to":"01-projects/x.md"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn move_from_tooling_dir_is_400() {
    let (_dir, router) = vault_router(None);
    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/vault/move",
        "application/json",
        r#"{"from":".git/config","to":"01-projects/x.md"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(body.contains("tooling directory"), "body: {body}");
}

// ─────────────────────────────────────────────────────────────────────────
// Method-not-allowed on an existing route → 405 (auth passes first).
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn wrong_method_on_config_is_405() {
    let (_dir, router) = vault_router(None);
    // /api/config is GET-only; an authed PATCH matches the path but not the verb.
    let (status, _body) = send_authed(&router, "PATCH", "/api/config", "text/plain", "").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

// ─────────────────────────────────────────────────────────────────────────
// No vault bound → 503 on EVERY remaining vault handler (tasks / raw / upload /
// post / put / delete file / move / folder create+delete). Complements the
// existing no_vault_{tree,file,config} tests so all require_vault_root guards
// are exercised. Each carries the curated "no vault bound" message.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn no_vault_remaining_endpoints_all_503() {
    let router = no_vault_router();
    // (method, uri, content-type, body) — every path is NON-tooling + well-formed
    // so the handler reaches `require_vault_root` (the 503 guard) rather than
    // tripping an earlier reject_tooling_path / Json rejection.
    let cases: &[(&str, &str, &str, &str)] = &[
        ("GET", "/api/vault/tasks", "text/plain", ""),
        ("GET", "/api/vault/raw?path=a.png", "text/plain", ""),
        (
            "POST",
            "/api/vault/upload?path=a.bin",
            "application/octet-stream",
            "x",
        ),
        ("POST", "/api/vault/file?path=n.md", "text/markdown", "x"),
        ("PUT", "/api/vault/file?path=n.md", "text/markdown", "x"),
        ("DELETE", "/api/vault/file?path=n.md", "text/plain", ""),
        (
            "POST",
            "/api/vault/move",
            "application/json",
            r#"{"from":"a.md","to":"b.md"}"#,
        ),
        ("POST", "/api/vault/folder?path=f", "text/plain", ""),
        ("DELETE", "/api/vault/folder?path=f", "text/plain", ""),
    ];
    for (method, uri, ctype, body) in cases {
        let (status, b) = send_authed(&router, method, uri, ctype, body).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {uri} should be 503 with no vault; body: {b}"
        );
        assert!(
            b.contains("no vault bound"),
            "{method} {uri} body should carry the no-vault message: {b}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/translate — Google gtx bridge for select-to-lookup
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn translate_requires_token() {
    let (_d, router) = vault_router(None);
    let req = Request::builder()
        .method("POST")
        .uri("/api/translate")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"text":"hi","to":"th"}"#))
        .unwrap();
    let (status, _) = send(&router, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn translate_rejects_empty_text_authed() {
    let (_d, router) = vault_router(None);
    let (status, body) = send_authed(
        &router,
        "POST",
        "/api/translate",
        "application/json",
        r#"{"text":"","to":"th"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("error"));
}
