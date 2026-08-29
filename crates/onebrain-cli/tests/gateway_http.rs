//! `onebrain gateway run` binary integration test — the real-TCP twin of the
//! oneshot coverage in `commands/gateway/server.rs` (which drives the router
//! directly via `tower::ServiceExt::oneshot`, no socket). This test spawns
//! the actual compiled binary, parses the bound URL from its stdout startup
//! line, and drives the MCP handshake + all four Brain-pack tools over real
//! HTTP via `ureq`.
//!
//! Model: `tests/mcp_stdio.rs`'s sandboxing (tempdir vault + cache + HOME so
//! `~/.onebrain` and daemon slots never touch the real machine; `KillOnDrop`)
//! and `tests/serve_bind_order.rs`'s stdout-to-file polling — the gateway
//! blocks on Ctrl-C after a successful bind (same as `serve`), so `.output()`
//! would hang forever; stdout is captured to a file this test polls instead.
//!
//! Header requirements (see task-2-report.md's discovered-headers section):
//! `Host` comes free over real TCP (`ureq` sets it from the URL's
//! authority) — no manual header needed, unlike the oneshot tests' `Request`
//! builder. SEP-2243's `Mcp-Method`/`Mcp-Name` headers ARE still required on
//! every non-`initialize` request once `MCP-Protocol-Version: 2026-07-28` is
//! set, exactly as in the oneshot tests.
//!
//! OAuth (Gateway PR 3, Task 2): `/mcp` now requires `Authorization: Bearer
//! <token>`. This crate has no library target (bin-only), so this external
//! test binary can't call `AuthStore::issue_token_pair` directly the way the
//! in-crate oneshot tests do — instead [`plant_valid_access_token`] writes a
//! `TokenRecord` straight into the sandboxed `$HOME/.onebrain/gateway/tokens.json`
//! file, in the exact shape `AuthStore` persists. `AuthStore` re-reads its
//! files fresh on every call (no in-memory cache — see `store.rs`'s module
//! docs), so a file planted after the gateway process is already running is
//! picked up on the next request with no restart needed. This is a
//! test-only shortcut standing in for the real `/authorize` + `/token`
//! exchange, which doesn't exist yet (Tasks 4-5).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::tempdir;

mod support;

/// Kills the spawned gateway on drop so a panicking assertion never leaks an
/// orphan `onebrain gateway run` process on CI. Copied (not shared) from
/// `tests/mcp_stdio.rs` per the brief — test files cannot pull in
/// non-`support` helpers from each other.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// Best-effort `onebrain daemon stop --all` in the sandbox HOME, so a daemon
/// `brain_search` auto-started doesn't linger past the test (it would
/// idle-exit on its own TTL, but stopping is tidy and frees the port
/// immediately). Mirrors `tests/mcp_stdio.rs::stop_daemon`.
fn stop_daemon(cache_dir: &Path, home: &Path) {
    let _ = Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .env("ONEBRAIN_CACHE_DIR", cache_dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .args(["daemon", "stop", "--all"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Spawn `onebrain gateway run --port 0`, scoped to a sandbox HOME/cache and
/// a given cwd, with stdout/stderr redirected to files this test polls — the
/// process blocks on Ctrl-C after a successful bind (same shape as `serve`),
/// so `.output()` would hang forever waiting for it to exit.
fn spawn_gateway(
    cache_dir: &Path,
    home: &Path,
    cwd: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
) -> std::process::Child {
    let stdout_file = std::fs::File::create(stdout_path).unwrap();
    let stderr_file = std::fs::File::create(stderr_path).unwrap();
    Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .env("ONEBRAIN_CACHE_DIR", cache_dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("ONEBRAIN_VAULT")
        .current_dir(cwd)
        .args(["gateway", "run", "--port", "0"])
        .stdin(Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file)
        .spawn()
        .expect("spawn onebrain gateway run")
}

/// Bounded poll (30s) for the stable startup line (`gateway listening on
/// http://<bound-addr>/mcp`), returning the parsed `/mcp` URL. Panics with a
/// REDACTED tail of the captured stderr if the process exits early or the
/// deadline passes first — this is the ONLY way the test learns the
/// OS-assigned port from `--port 0`.
///
/// **Why the panic has to carry the stderr, and why it has to redact it.**
/// The capture files live inside a `tempfile::TempDir` that is dropped —
/// and deleted — during this panic's own unwind, so whatever is not in the
/// message is gone by the time a human reads the failure; a byte count
/// leaves no way forward but to patch the test and re-run CI. But the
/// streams cannot be interpolated raw either (CodeQL
/// `rust/cleartext-logging`, and this branch's own "no host path or secret
/// in any test message" rule): `out` already contains the real pairing code
/// by this point — `gateway run` prints it before the "gateway listening"
/// line — and `err` carries the gateway's tracing output, which
/// legitimately names host paths.
///
/// So: stdout is never emitted in any form (it is the one place the pairing
/// code is ever shown), and stderr goes through
/// [`support::redacted_capture_tail`], which collapses every path-shaped and
/// pairing-code-shaped token to a placeholder and bounds the result. The
/// sibling harnesses in `gateway_oauth_e2e.rs` and `gateway_approval_e2e.rs`
/// do exactly the same, through the same helper.
/// `gateway_startup_failure_panic_carries_a_redacted_stderr_tail` below pins
/// the behavior against a REAL constructed startup failure, not a
/// hand-written string.
fn wait_for_gateway_url(
    child: &mut std::process::Child,
    stdout_path: &Path,
    stderr_path: &Path,
) -> String {
    const PREFIX: &str = "gateway listening on ";
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = std::fs::read_to_string(stdout_path).unwrap_or_default();
        if let Some(line) = out.lines().find(|l| l.starts_with(PREFIX)) {
            return line[PREFIX.len()..].trim().to_string();
        }
        if let Some(status) = child.try_wait().expect("poll gateway child") {
            let err = std::fs::read_to_string(stderr_path).unwrap_or_default();
            panic!(
                "onebrain gateway run exited early ({status}) before printing the \
                 listening line; redacted stderr tail:\n{}",
                support::redacted_capture_tail(&err)
            );
        }
        if Instant::now() >= deadline {
            let err = std::fs::read_to_string(stderr_path).unwrap_or_default();
            panic!(
                "onebrain gateway run did not print the listening line within 30s; \
                 redacted stderr tail:\n{}",
                support::redacted_capture_tail(&err)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Plants a live, unexpired, unrevoked ACCESS `TokenRecord` directly into the
/// sandboxed `$HOME/.onebrain/gateway/tokens.json` — see the module doc
/// comment for why this test can't just call `AuthStore::issue_token_pair`
/// (no library target) or drive the real OAuth flow (no `/authorize`/`/token`
/// routes yet). The directory always exists by the time this is called: the
/// gateway process's `AuthStore::open()` creates it at the very start of
/// `run()`, well before it prints the "gateway listening on" line this
/// test's caller already waited for.
///
/// The JSON shape mirrors `commands/gateway/auth/store.rs::TokenRecord`
/// field-for-field (`kind`/`client_id` etc. use the crate's own
/// `#[serde(rename_all = "lowercase")]` encoding) — if that shape ever
/// changes, this helper needs to change with it, same as any other
/// cross-boundary fixture.
fn plant_valid_access_token(home: &Path, token: &str) {
    let dir = home.join(".onebrain").join("gateway");
    std::fs::create_dir_all(&dir).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut tokens = serde_json::Map::new();
    tokens.insert(
        token.to_string(),
        serde_json::json!({
            "token": token,
            "kind": "access",
            "family": "gateway-http-test-family",
            "client_id": "gateway-http-test",
            "scope": "brain",
            "expires": now + 3600,
            "revoked": false,
            "rotated_to": null,
        }),
    );
    std::fs::write(
        dir.join("tokens.json"),
        serde_json::to_vec_pretty(&serde_json::Value::Object(tokens)).unwrap(),
    )
    .unwrap();
}

/// Bounded poll (10s) that the child has exited after being killed — proves
/// `kill()` actually reaped the process rather than leaving a zombie/hung
/// child, without relying on a platform-specific exit-status shape for a
/// SIGKILL'd process.
fn assert_exits_after_kill(child: &mut std::process::Child) {
    child.kill().expect("kill gateway child");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("poll gateway child").is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "onebrain gateway run did not exit within 10s of being killed"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A `ureq` agent that never turns a non-2xx HTTP status into an `Err`.
/// JSON-RPC business-logic errors (unknown vault, traversal rejection) are
/// carried in the response BODY per the JSON-RPC-over-HTTP convention this
/// server follows — the oneshot tests in `commands/gateway/server.rs` assert
/// on `resp["error"]` without caring about HTTP status. Turning
/// `http_status_as_error` off means this test's `post` helper has one path
/// for both success and JSON-RPC-error responses.
fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .into()
}

/// POST one JSON-RPC `body` to `url` with `token` as the `Authorization:
/// Bearer` credential, plus `extra` headers layered on the baseline
/// content-type/accept/authorization every request needs, and parse the
/// response as JSON. `Host` comes free over real TCP — see the module doc
/// comment.
fn post(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    body: &serde_json::Value,
    extra: &[(&str, &str)],
) -> serde_json::Value {
    let mut req = agent
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {token}"));
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    let mut resp = req
        .send(body.to_string())
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));
    let text = resp
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|e| panic!("read response body from {url}: {e}"));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("response from {url} was not JSON ({e}): {text}"))
}

const PROTOCOL: &str = "2026-07-28";

fn init_body(id: u32, protocol_version: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": protocol_version,
            "capabilities": {},
            "clientInfo": {"name": "gateway-http-test", "version": "0.0.0"},
        },
    })
}

fn call_body(id: u32, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments},
    })
}

/// SEP-2243 headers required on every non-`initialize` request once
/// `MCP-Protocol-Version: 2026-07-28` is set — real-TCP twin of
/// `commands/gateway/server.rs::standard_headers` (test files cannot share
/// non-`support` helpers, so this is intentionally duplicated, not called).
fn standard_headers<'a>(method: &'a str, name: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut headers = vec![("MCP-Protocol-Version", PROTOCOL), ("Mcp-Method", method)];
    if let Some(name) = name {
        headers.push(("Mcp-Name", name));
    }
    headers
}

/// `onebrain gateway run` outside any vault, with an EMPTY sandbox HOME (no
/// `~/.onebrain/gateway.yml`, so no `default_vault` either): unlike `onebrain
/// mcp`'s eager `open_engine` guard (which exits 64 before ever starting the
/// server — see `mcp_without_vault_exits_64_before_any_handshake`), the
/// gateway resolves a vault PER REQUEST, not at startup, so the server comes
/// up fine and STAYS UP. A `brain_tasks` call with no `vault` argument then
/// returns a vault-not-found JSON-RPC error rather than crashing the process
/// or hanging — asserting exactly that (not an exit code) is the actual
/// behavior this test protects, hence the name.
#[test]
fn gateway_run_outside_vault_brain_tasks_returns_vault_not_found_error() {
    let neutral = tempdir().unwrap(); // no onebrain.yml anywhere above
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap(); // empty — no gateway.yml, no run dir

    let stdout_path = neutral.path().join("gateway-stdout.log");
    let stderr_path = neutral.path().join("gateway-stderr.log");
    let mut child = KillOnDrop(spawn_gateway(
        cache.path(),
        home.path(),
        neutral.path(),
        &stdout_path,
        &stderr_path,
    ));
    let mcp_url = wait_for_gateway_url(&mut child.0, &stdout_path, &stderr_path);
    let token = "gateway-http-test-token-outside-vault";
    plant_valid_access_token(home.path(), token);
    let agent = http_agent();

    let init_resp = post(
        &agent,
        &mcp_url,
        token,
        &init_body(1, PROTOCOL),
        &[("MCP-Protocol-Version", PROTOCOL)],
    );
    assert_eq!(
        init_resp["result"]["protocolVersion"], PROTOCOL,
        "initialize pin: {init_resp}"
    );

    let resp = post(
        &agent,
        &mcp_url,
        token,
        &call_body(2, "brain_tasks", serde_json::json!({})),
        &standard_headers("tools/call", Some("brain_tasks")),
    );
    let message = resp["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a JSON-RPC error (no vault anywhere): {resp}"));
    assert!(
        message.contains("E_VAULT_NOT_FOUND"),
        "expected the vault-not-found E-code in the message: {message}"
    );

    // The server must still be alive — the error above is per-request
    // resolution, not a startup failure that would have taken it down.
    assert!(
        child.0.try_wait().expect("poll gateway child").is_none(),
        "server must stay up after a per-request vault-resolution error"
    );

    assert_exits_after_kill(&mut child.0);
}

/// Happy path: a fixture vault (note + dated task) named by a machine-level
/// `~/.onebrain/gateway.yml` (proving config loading end-to-end — the
/// spawn's cwd is a NEUTRAL tempdir outside the vault, so walk-up discovery
/// alone could never resolve it; only `default_vault` from the config can).
/// Drives the full MCP handshake + all four Brain-pack tools over real HTTP.
#[test]
fn gateway_run_happy_path_serves_fixture_vault_via_machine_config() {
    let vault = tempdir().unwrap();
    write(vault.path(), "onebrain.yml", "folders: {}\n");
    write(
        vault.path(),
        "01-projects/x.md",
        "- [ ] gateway fixture task 📅 2026-01-01\n",
    );
    write(vault.path(), "hello.md", "hello from the fixture vault\n");

    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    write(
        home.path(),
        ".onebrain/gateway.yml",
        &format!(
            "default_vault: {v}\nvaults:\n  t1: {v}\n",
            v = vault.path().display()
        ),
    );
    let cwd = tempdir().unwrap(); // neutral — deliberately NOT inside the vault

    let stdout_path = cwd.path().join("gateway-stdout.log");
    let stderr_path = cwd.path().join("gateway-stderr.log");
    let mut child = KillOnDrop(spawn_gateway(
        cache.path(),
        home.path(),
        cwd.path(),
        &stdout_path,
        &stderr_path,
    ));
    let mcp_url = wait_for_gateway_url(&mut child.0, &stdout_path, &stderr_path);
    let token = "gateway-http-test-token-happy-path";
    plant_valid_access_token(home.path(), token);
    let agent = http_agent();

    // 1. initialize pins 2026-07-28.
    let resp = post(
        &agent,
        &mcp_url,
        token,
        &init_body(1, PROTOCOL),
        &[("MCP-Protocol-Version", PROTOCOL)],
    );
    assert_eq!(resp["result"]["protocolVersion"], PROTOCOL, "pin: {resp}");
    assert_eq!(resp["result"]["serverInfo"]["name"], "onebrain-gateway");

    // 2. initialize echoes a dual-era client's own requested version.
    let resp = post(
        &agent,
        &mcp_url,
        token,
        &init_body(2, "2025-11-25"),
        &[("MCP-Protocol-Version", "2025-11-25")],
    );
    assert_eq!(
        resp["result"]["protocolVersion"], "2025-11-25",
        "echo: {resp}"
    );

    // 3. tools/list carries all four Brain-pack tools.
    let list_body = serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {},
    });
    let resp = post(
        &agent,
        &mcp_url,
        token,
        &list_body,
        &standard_headers("tools/list", None),
    );
    let names: Vec<&str> = resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools array: {resp}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for tool in ["capabilities", "brain_tasks", "brain_get", "brain_search"] {
        assert!(
            names.contains(&tool),
            "tools/list missing `{tool}`: {names:?}"
        );
    }

    // 4. brain_tasks surfaces the fixture task via the machine-config-named
    // default vault.
    let resp = post(
        &agent,
        &mcp_url,
        token,
        &call_body(
            4,
            "brain_tasks",
            serde_json::json!({"due_by": "2026-12-31"}),
        ),
        &standard_headers("tools/call", Some("brain_tasks")),
    );
    let sc = &resp["result"]["structuredContent"];
    assert_eq!(sc["total"], 1, "{resp}");
    assert!(
        sc["tasks"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("gateway fixture task"),
        "{resp}"
    );

    // 5. brain_get round-trips the fixture note.
    let resp = post(
        &agent,
        &mcp_url,
        token,
        &call_body(5, "brain_get", serde_json::json!({"file": "hello.md"})),
        &standard_headers("tools/call", Some("brain_get")),
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("hello from the fixture vault"), "{resp}");

    // 6. brain_get rejects a traversal attempt.
    let resp = post(
        &agent,
        &mcp_url,
        token,
        &call_body(6, "brain_get", serde_json::json!({"file": "../escape"})),
        &standard_headers("tools/call", Some("brain_get")),
    );
    assert!(
        resp.get("error").is_some(),
        "expected a traversal rejection: {resp}"
    );

    // 7. brain_search round-trips through the real warm daemon it auto-starts.
    // The fixture vault was never reindexed, so this asserts response SHAPE
    // and no transport error, not hit content (empty index).
    let resp = post(
        &agent,
        &mcp_url,
        token,
        &call_body(7, "brain_search", serde_json::json!({"query": "vault"})),
        &standard_headers("tools/call", Some("brain_search")),
    );
    assert_eq!(
        resp["result"]["isError"], false,
        "brain_search must not be a transport-level failure: {resp}"
    );
    let body_text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(body_text)
        .unwrap_or_else(|e| panic!("brain_search body was not JSON ({e}): {body_text}"));
    assert!(
        parsed.is_object(),
        "brain_search body should be a JSON object: {parsed}"
    );

    // The daemon `brain_search` just spawned must live under the SANDBOX
    // HOME, never a real one on the developer's/CI machine — proves the
    // gateway's `daemon_client::ensure_running(Some(&vault_path))` call
    // inherited this process's `$HOME` override rather than escaping it.
    let run_dir = home.path().join(".onebrain").join("run");
    let spawned_a_sandboxed_daemon = std::fs::read_dir(&run_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("daemon-"))
        })
        .unwrap_or(false);
    assert!(
        spawned_a_sandboxed_daemon,
        "brain_search must have spawned its daemon under the sandbox HOME ({}); \
         found nothing there",
        run_dir.display()
    );

    // 8. Kill the server and assert it exits.
    assert_exits_after_kill(&mut child.0);

    // Cleanup: stop the daemon `brain_search` auto-started so it doesn't
    // linger past the test (it would idle-exit on its own TTL, but this is
    // tidy and frees the port immediately).
    stop_daemon(cache.path(), home.path());
}

// ── The startup-failure diagnostic itself ────────────────────────────────

/// The panic `wait_for_gateway_url` raises when the gateway dies at startup
/// must be ACTIONABLE and REDACTED at the same time, and this proves both
/// against a REAL failure rather than a hand-written stderr string.
///
/// The failure is constructed the same shape CI hits: `gateway run`'s very
/// first store call (`AuditLog::open()` → `~/.onebrain/gateway/audit`) cannot
/// create its directory, so `run()` returns an `anyhow` error, the binary
/// prints it to stderr and exits 1 before ever reaching the "gateway
/// listening" line. Here that is forced by planting a regular FILE at
/// `$HOME/.onebrain/gateway`, which no `create_dir_all` can turn into a
/// directory. (CI's own version is a permissions failure on the same call —
/// the point is only that the diagnostic lives in stderr and the capture
/// files are deleted during the unwind.)
///
/// `catch_unwind` deliberately leaves the default panic hook installed, so
/// the redacted message is printed to the test log as well: setting a no-op
/// hook would be process-global and would swallow a genuine panic from a
/// concurrently-running test in this same binary. A "thread panicked" line
/// in this test's output is expected, and its content is the thing under
/// test.
#[test]
fn gateway_startup_failure_panic_carries_a_redacted_stderr_tail() {
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    // A regular file exactly where the audit/auth store's directory must go.
    write(home.path(), ".onebrain/gateway", "not a directory\n");
    let cwd = tempdir().unwrap();

    let stdout_path = cwd.path().join("gateway-stdout.log");
    let stderr_path = cwd.path().join("gateway-stderr.log");
    let mut child = KillOnDrop(spawn_gateway(
        cache.path(),
        home.path(),
        cwd.path(),
        &stdout_path,
        &stderr_path,
    ));

    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for_gateway_url(&mut child.0, &stdout_path, &stderr_path)
    }))
    .expect_err("gateway run must fail to start with a file in place of its store dir");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| panic!("panic payload was not a String"));

    // 1. Actionable: it says the process died, and it carries the gateway's
    //    OWN words about why — not a byte count.
    assert!(
        message.contains("exited early"),
        "expected the early-exit panic, got: {message}"
    );
    assert!(
        message.contains("gateway audit log") || message.contains("gateway auth store"),
        "the panic must carry the gateway's own failure text: {message}"
    );
    assert!(
        !message.contains("<empty>"),
        "the stderr tail must not be empty for a startup failure: {message}"
    );

    // 2. Redacted. Negative control FIRST: the raw stderr really did name
    //    the sandbox path, so what follows is proof the redactor removed
    //    something that was there — not proof that there was nothing to
    //    remove. (The capture file is still readable here only because
    //    `catch_unwind` stopped the unwind inside this test, leaving `cwd`
    //    alive; in the real harness this same `TempDir` is gone by the time
    //    anyone sees the panic, which is the whole reason the message has to
    //    carry the diagnostic itself.)
    let raw_stderr = std::fs::read_to_string(&stderr_path).expect("read captured stderr");
    assert!(
        raw_stderr.contains(&home.path().display().to_string()),
        "precondition: the gateway's own stderr must name the sandbox HOME, \
         otherwise this test proves nothing about redaction"
    );
    assert!(
        message.contains("<path>"),
        "the failure text names a path, so redaction must have fired: {message}"
    );
    for sandbox in [home.path(), cwd.path(), cache.path()] {
        let rendered = sandbox.display().to_string();
        assert!(
            !message.contains(&rendered),
            "sandbox path leaked into the panic message: {message}"
        );
        // The tempdir's own basename is distinctive (`.tmpXXXXXX`); catching
        // it catches a leak through a differently-canonicalized prefix
        // (`/var` vs `/private/var` on macOS) that the full-path check above
        // would miss.
        let base = sandbox
            .file_name()
            .and_then(|s| s.to_str())
            .expect("tempdir has a basename");
        assert!(
            !message.contains(base),
            "sandbox tempdir name leaked into the panic message: {message}"
        );
    }
    // Strongest form: after redaction nothing path-shaped is left at all.
    assert!(
        !message.contains('/') && !message.contains('\\'),
        "a path separator survived redaction: {message}"
    );
}

/// Supplementary unit pins on the shared redactor, kept in this binary (not
/// in `tests/support/mod.rs`) so they run once rather than once per test
/// binary that pulls that module in. The constructed-failure test above is
/// the primary guarantee; these fix the exact shapes.
#[test]
fn redacted_capture_tail_replaces_paths_and_pairing_codes_and_keeps_the_rest() {
    use support::redacted_capture_tail as tail;

    assert_eq!(
        tail("Error: create gateway auth store dir /var/folders/t/x: oops"),
        "Error: create gateway auth store dir <path> oops"
    );
    assert_eq!(tail("path=/home/runner/.onebrain"), "path=<path>");
    assert_eq!(tail("opening \"/tmp/a b\""), "opening \"<path> b\"");
    assert_eq!(tail("C:\\Users\\runner\\x"), "C:<path>");
    assert_eq!(tail("\\\\server\\share"), "<path>");
    // The shape the delimiter-preconditioned first draft silently missed:
    // a host path glued to a preceding word. Redacted here, at the cost of
    // over-redacting things like `and/or` — see `path_start`.
    assert_eq!(tail("under/Users/alice/x"), "under<path>");

    // A pairing code cannot reach stderr today, but the shape is redacted
    // anyway — see the helper's own doc comment.
    assert_eq!(tail("code ABCD-2345 here"), "code <code> here");
    assert_eq!(tail("abcd-2345"), "abcd-2345");
    assert_eq!(tail("ABCDE-234"), "ABCDE-234");

    // Ordinary diagnostic text — the part worth keeping — survives intact.
    assert_eq!(
        tail("2026-08-29T12:00:00.123456Z WARN gateway: not a directory (os error 20)"),
        "2026-08-29T12:00:00.123456Z WARN gateway: not a directory (os error 20)"
    );
}

#[test]
fn redacted_capture_tail_is_bounded_and_marks_an_empty_stream() {
    use support::{redacted_capture_tail as tail, CAPTURE_TAIL_BYTES, CAPTURE_TAIL_LINES};

    assert_eq!(tail(""), "<empty>");

    let many = (0..100)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let out = tail(&many);
    assert_eq!(out.lines().count(), CAPTURE_TAIL_LINES);
    assert!(out.starts_with("line88"), "{out}");

    let out = tail(&"x".repeat(10_000));
    assert!(out.starts_with("[…] "), "{out}");
    assert!(out.len() < CAPTURE_TAIL_BYTES + 16, "{}", out.len());
}
