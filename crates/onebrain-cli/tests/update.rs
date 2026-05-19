//! Layer 2 integration tests for `onebrain update`.
//!
//! Strategy:
//!
//! - The CLI binary's default fetch path reads `ONEBRAIN_GITHUB_RELEASES_URL`
//!   when set, so a local `mockito::Server` can stand in for GitHub.
//! - Install + validate paths still call real `bun` / `onebrain` binaries.
//!   Tests that would otherwise mutate the user's global install always run
//!   `--check`, which fetches but skips install/validate entirely.
//! - For the install/validate paths we rely on the library-level unit tests
//!   in `onebrain_fs::update` (injectable closures, exhaustive coverage).
//!   This file exercises the wiring shell only.
//!
//! The PATH override trick (point PATH at a tempdir with no `onebrain`
//! binary) lets us verify the validate-failure branch even when the test
//! host has a real `onebrain` on PATH — but only when we also bypass the
//! install step. We expose that through the same env-var: setting
//! `ONEBRAIN_INSTALL_SKIP=1` in test mode (which we do NOT honor in the
//! library) is too invasive · instead each test uses `--check` to short
//! circuit before install.

use assert_cmd::Command;
use predicates::prelude::*;

const TAG_URL_ENV: &str = "ONEBRAIN_GITHUB_RELEASES_URL";

fn mocked_releases_url(server: &mockito::Server) -> String {
    format!(
        "{}/repos/onebrain-ai/onebrain/releases/latest",
        server.url()
    )
}

/// Helper · build a mockito server that serves a `releases/latest` payload
/// at the standard GitHub path. Returns the server (must outlive the test
/// command) and the URL to feed into `ONEBRAIN_GITHUB_RELEASES_URL`.
fn mock_releases(body: serde_json::Value) -> (mockito::ServerGuard, String) {
    let mut server = mockito::Server::new();
    let url = mocked_releases_url(&server);
    server
        .mock("GET", "/repos/onebrain-ai/onebrain/releases/latest")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body.to_string())
        .create();
    (server, url)
}

#[test]
fn update_check_prints_latest_version_from_mocked_github() {
    let (_server, url) = mock_releases(serde_json::json!({
        "tag_name": "v9.9.9",
        "published_at": "2026-04-24T00:00:00Z"
    }));

    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["update", "--check"])
        .env(TAG_URL_ENV, &url)
        .assert()
        .success()
        .stdout(predicate::str::contains("OneBrain Update"))
        .stdout(predicate::str::contains("latest: v9.9.9"))
        .stdout(predicate::str::contains("dry run complete"));
}

#[test]
fn update_check_exits_1_when_github_returns_503() {
    let mut server = mockito::Server::new();
    let url = mocked_releases_url(&server);
    server
        .mock("GET", "/repos/onebrain-ai/onebrain/releases/latest")
        .with_status(503)
        .create();

    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["update", "--check"])
        .env(TAG_URL_ENV, &url)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("Fetch failed"));
}

#[test]
fn update_check_exits_1_when_payload_missing_tag_name() {
    let (_server, url) = mock_releases(serde_json::json!({ "name": "no tag here" }));

    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["update", "--check"])
        .env(TAG_URL_ENV, &url)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("missing tag_name"));
}

#[test]
fn update_check_is_idempotent_no_side_effects() {
    let (_server, url) = mock_releases(serde_json::json!({
        "tag_name": "v9.9.9"
    }));

    // Run twice; both must exit 0 with the same stdout pattern. No global
    // state should accumulate between runs (the CLI process is fresh each
    // time).
    for _ in 0..2 {
        Command::cargo_bin("onebrain")
            .unwrap()
            .args(["update", "--check"])
            .env(TAG_URL_ENV, &url)
            .assert()
            .success()
            .stdout(predicate::str::contains("latest: v9.9.9"));
    }
}

#[test]
fn update_help_includes_check_flag() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["update", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--check"));
}

#[test]
fn update_network_unreachable_exits_1() {
    // 127.0.0.1:1 is reserved · TCP connect will fail fast.
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["update", "--check"])
        .env(
            TAG_URL_ENV,
            "http://127.0.0.1:1/repos/onebrain-ai/onebrain/releases/latest",
        )
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("Fetch failed"));
}
