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
//! Two tests pin the ordering from both sides:
//! - The NEGATIVE half occupies a port itself (a real ephemeral `TcpListener`
//!   held for the duration of the assertion), runs `onebrain serve --port
//!   <that port>` against it, and asserts NO banner/URL/token appears —
//!   only the bind error.
//! - The POSITIVE half runs `onebrain serve --port <free port>` for real and
//!   asserts the banner DOES appear after a successful bind — without it, the
//!   negative half would still pass if the `on_bind` banner print were
//!   deleted entirely.
//!
//! In both, `--port` forces the standalone bind path (skips daemon routing
//! entirely — see `wants_daemon_routing` in `serve.rs`).

mod support;

use assert_cmd::Command;
use std::fs;
use std::net::TcpListener;
use std::time::{Duration, Instant};
use tempfile::tempdir;

/// Create a minimal vault — `serve` is vault-required and resolves it before
/// anything else, so this just needs to exist.
fn minimal_vault() -> tempfile::TempDir {
    let vault = tempdir().unwrap();
    fs::write(vault.path().join("onebrain.yml"), "method: onebrain\n").unwrap();
    vault
}

#[test]
fn serve_bind_failure_prints_no_success_banner_before_the_error() {
    // Occupy a port ourselves so `onebrain serve --port <port>` fails to bind.
    // Held open for the entire subprocess run.
    let occupied = TcpListener::bind("127.0.0.1:0").expect("bind a probe port");
    let port = occupied.local_addr().unwrap().port();

    let vault = minimal_vault();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .current_dir(vault.path())
        .env_remove("ONEBRAIN_VAULT")
        // `serve --port` always means standalone (never a daemon route), so
        // this doesn't need `$ONEBRAIN_NO_DAEMON` — but set it anyway to keep
        // the test robust against a future change to that gating.
        .env("ONEBRAIN_NO_DAEMON", "1")
        .args(["serve", "--port", &port.to_string()])
        .output()
        .expect("spawn onebrain binary");
    // `occupied` stays bound until end of scope — the port must be held for
    // the entire subprocess run so the child's bind reliably fails.

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

/// RAII kill for the long-running `serve` subprocess: a failed assertion
/// between spawn and the test's own kill must never leak a listener that
/// blocks on Ctrl-C forever (mirrors `StopDaemonOnDrop` in the daemon tests).
/// A double kill (Drop after the happy path already killed) is a no-op error.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The POSITIVE half of the #278 ordering contract: after a SUCCESSFUL bind,
/// the banner (with the stop hint and the token-bearing URL) must appear on
/// stdout. Without this test, the negative assertions above would still pass
/// if the `on_bind` banner print were deleted entirely.
#[test]
fn serve_successful_bind_prints_the_banner() {
    // Use `--port 0` so the KERNEL assigns a free port at bind time — no
    // probe/drop/rebind TOCTOU race with other processes. The banner must
    // then carry the ACTUAL bound port (not the literal `0`), which this
    // test parses back out of the printed URL.
    let vault = minimal_vault();

    // `serve` blocks on Ctrl-C after a successful bind, so `.output()` would
    // hang — spawn instead, with stdout captured to a file we can poll while
    // the child runs.
    let stdout_path = vault.path().join("serve-stdout.log");
    let stdout_file = fs::File::create(&stdout_path).unwrap();
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("onebrain"))
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .current_dir(vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .env("ONEBRAIN_NO_DAEMON", "1")
        .args(["serve", "--port", "0"])
        .stdout(stdout_file)
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn onebrain binary");
    let mut child = KillOnDrop(child);

    // Bounded poll (≤5s, well under the 10s budget): the banner prints from
    // `on_bind`, i.e. as soon as the listener is up.
    let deadline = Instant::now() + Duration::from_secs(5);
    let stdout = loop {
        let s = fs::read_to_string(&stdout_path).unwrap_or_default();
        if s.contains("Ctrl-C to stop") {
            break s;
        }
        if let Some(status) = child.0.try_wait().expect("probe serve child") {
            panic!("serve exited early ({status}) instead of serving: stdout={s}");
        }
        assert!(
            Instant::now() < deadline,
            "banner never appeared within 5s of a successful bind: stdout={s}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    // The full banner: section header, token-bearing URL carrying the REAL
    // kernel-assigned port, and the stop hint (already matched by the loop).
    assert!(
        stdout.contains("Serving"),
        "banner header must appear after a successful bind: {stdout}"
    );
    let url_line = stdout
        .lines()
        .find(|l| l.contains("http://127.0.0.1:"))
        .unwrap_or_else(|| panic!("token-bearing URL must appear: {stdout}"));
    let bound_port: u16 = url_line
        .split("http://127.0.0.1:")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("URL must carry a parseable port: {url_line}"));
    assert_ne!(
        bound_port, 0,
        "banner must print the ACTUAL bound port, not the requested `0`: {url_line}"
    );
    assert!(
        url_line.contains("/?token="),
        "URL must carry the auth token: {url_line}"
    );

    // Terminate the still-serving child (also covered by KillOnDrop on the
    // panic paths above).
    child.0.kill().expect("kill serve child");
    child.0.wait().expect("reap serve child");
}
