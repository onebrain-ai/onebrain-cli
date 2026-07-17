//! Regression coverage for #278: `onebrain serve --port <N>` used to print the
//! full "🌐 Serving" banner — URL, vault, web UI, AND the auth token — BEFORE
//! attempting to bind the HTTP listener. On a bind failure (e.g. the port is
//! already in use) the user saw a plausible-looking banner immediately
//! followed by a bind error and exit 1: it read as "started, then crashed"
//! instead of "never started".
//!
//! The fix reorders `serve`'s standalone path so the listener binds FIRST —
//! the banner now prints only from inside `server::run_server_with`'s
//! `on_bind` callback, which fires exclusively after a successful bind (see
//! `crates/onebrain-cli/src/commands/serve.rs` and
//! `crates/onebrain-cli/src/server/mod.rs`).
//!
//! This test occupies a port itself (a real ephemeral `TcpListener` we bind
//! and hold for the duration of the assertion), then runs `onebrain serve
//! --port <that port>` against it. `--port` forces the standalone bind path
//! (skips daemon routing entirely — see `wants_daemon_routing` in
//! `serve.rs`), so the subprocess returns promptly with the bind error
//! instead of blocking on Ctrl-C.

use assert_cmd::Command;
use std::fs;
use std::net::TcpListener;
use tempfile::tempdir;

#[test]
fn serve_bind_failure_prints_no_success_banner_before_the_error() {
    // Occupy a port ourselves so `onebrain serve --port <port>` fails to bind.
    // Held open for the entire subprocess run.
    let occupied = TcpListener::bind("127.0.0.1:0").expect("bind a probe port");
    let port = occupied.local_addr().unwrap().port();

    // Minimal vault — `serve` is vault-required and resolves it before
    // anything else, so this just needs to exist.
    let vault = tempdir().unwrap();
    fs::write(vault.path().join("onebrain.yml"), "method: onebrain\n").unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env_remove("ONEBRAIN_VAULT")
        // `serve --port` always means standalone (never a daemon route), so
        // this doesn't need `$ONEBRAIN_NO_DAEMON` — but set it anyway to keep
        // the test robust against a future change to that gating.
        .env("ONEBRAIN_NO_DAEMON", "1")
        .args(["serve", "--port", &port.to_string()])
        .output()
        .expect("spawn onebrain binary");

    drop(occupied);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        !out.status.success(),
        "serve against an already-occupied port must fail; stdout={stdout} stderr={stderr}"
    );

    // No success-implying output of any kind: neither the section header nor
    // the URL/token/Ctrl-C lines the banner would have printed.
    assert!(
        !combined.contains("Serving"),
        "must not print the success banner header on bind failure: {combined}"
    );
    assert!(
        !combined.contains("http://127.0.0.1"),
        "must not print a URL (which would carry the auth token) on bind failure: {combined}"
    );
    assert!(
        !combined.contains("token="),
        "must never leak the auth token when the server never came up: {combined}"
    );
    assert!(
        !combined.contains("Ctrl-C to stop"),
        "must not print the stop hint for a server that never started: {combined}"
    );

    // The actual bind failure IS surfaced, clearly.
    assert!(
        stderr.contains("bind HTTP listener"),
        "must surface the bind error: stderr={stderr}"
    );
}
