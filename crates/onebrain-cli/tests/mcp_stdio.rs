//! `onebrain mcp` stdio JSON-RPC integration smoke test.
//!
//! Proves the full stdio loop works end-to-end through the real compiled
//! binary: spawn `onebrain --vault <fixture> mcp`, drive the MCP handshake
//! (`initialize` -> `notifications/initialized` -> `tools/list`), call the
//! `status` tool, then close stdin and verify the server exits 0.
//!
//! Deliberately lex-only: `status` and `tools/list` never construct the
//! embedder (see `Engine::open`'s doc comment — the real embedder is lazy),
//! so this test never downloads a model and runs unconditionally in CI
//! (no `ONEBRAIN_TEST_EMBED` gate, unlike `search_integration.rs`).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use tempfile::tempdir;

/// Kills the spawned server on drop so panicking assertion/timeout paths
/// never leak an orphan `onebrain mcp` process on CI.
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

/// Build a `Command` for the `onebrain` binary, scoped to `vault_root` and a
/// tempdir-redirected cache (`ONEBRAIN_CACHE_DIR`) so tests never touch the
/// real `~/.cache/onebrain/` or `~/Library/Caches/onebrain/`. Unlike the
/// other integration tests' `onebrain()` helper, this one deliberately omits
/// `--json` (irrelevant to `mcp`, which speaks JSON-RPC over stdio
/// regardless) and pipes all three stdio streams for the caller to drive.
///
/// `home` redirects `$HOME` / `%USERPROFILE%` to a tempdir so the warm-daemon
/// discovery + run dir (`~/.onebrain/run/`) is fully isolated: the `mcp`
/// server's `ensure_running()` spawns ITS OWN daemon over `vault_root` in the
/// sandbox instead of discovering (and colliding with) a real daemon on the
/// developer's machine. The spawned daemon inherits this env, so it publishes
/// `daemon.json` under the sandbox HOME and holds the fixture vault's engine.
fn onebrain_mcp(vault_root: &Path, cache_dir: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_onebrain"));
    cmd.env("ONEBRAIN_CACHE_DIR", cache_dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .arg("--vault")
        .arg(vault_root)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Best-effort `onebrain daemon stop` in the sandbox HOME, so a daemon the
/// `mcp` server auto-started doesn't linger past the test (it would idle-exit
/// on its own TTL, but stopping is tidy and frees the port immediately).
fn stop_daemon(cache_dir: &Path, home: &Path) {
    let _ = Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .env("ONEBRAIN_CACHE_DIR", cache_dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        // `--all`: the mcp server auto-started a PER-VAULT slot daemon (#230);
        // a plain `stop` would target only the cwd's slot and miss it.
        .args(["daemon", "stop", "--all"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Sends one JSON-RPC line (`value` + trailing `\n`) to the child's stdin.
fn send(stdin: &mut std::process::ChildStdin, value: &serde_json::Value) {
    let mut line = serde_json::to_string(value).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
}

/// Spawns a background thread that reads `stdout` line by line and forwards
/// each successfully-parsed JSON value over the returned channel. Non-JSON
/// or blank lines are skipped defensively (the server only ever emits single
/// JSON-RPC lines on stdout, but this keeps the reader robust). The thread
/// exits (dropping the sender) once it hits EOF, which happens only after
/// the child process itself exits — `read_line` blocks until then, which is
/// exactly why this runs off the main thread: it lets the test bound total
/// wait time with `recv_timeout` instead of blocking indefinitely.
fn spawn_line_reader(stdout: std::process::ChildStdout) -> mpsc::Receiver<serde_json::Value> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF: child closed stdout (process exiting)
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                        if tx.send(v).is_err() {
                            break; // receiver dropped (test ended)
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// Waits (bounded by `deadline`) on `rx` for a JSON-RPC message whose `"id"`
/// matches `expected_id`, skipping any other message (e.g. notifications
/// without an `"id"`).
fn recv_response(
    rx: &mpsc::Receiver<serde_json::Value>,
    expected_id: i64,
    deadline: std::time::Instant,
) -> serde_json::Value {
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for response id={expected_id}"
        );
        match rx.recv_timeout(remaining) {
            Ok(v) => {
                if v.get("id").and_then(|id| id.as_i64()) == Some(expected_id) {
                    return v;
                }
                // Some other message (e.g. a notification) — keep waiting.
            }
            Err(RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for response id={expected_id}")
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("onebrain mcp closed stdout before responding to id={expected_id}")
            }
        }
    }
}

/// `onebrain mcp` run with NO vault anywhere above `cwd` must fail fast with
/// `E_VAULT_NOT_FOUND` (exit 64) — the SAME vault-resolution guard `open_engine`
/// runs before doing anything else (see `commands/mcp.rs::run`, which calls
/// `open_engine(vault_flag)` as its first statement). This must happen
/// *before* any MCP handshake: the server never even reaches `serve(stdio())`,
/// so no JSON-RPC frame is ever written to stdout. Mirrors the
/// `serve_without_vault_exits_64` pattern in `dispatch_coverage.rs` (another
/// long-running-server command with the identical early vault-resolve guard).
#[test]
fn mcp_without_vault_exits_64_before_any_handshake() {
    let neutral = tempdir().unwrap(); // no onebrain.yml anywhere above
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap(); // isolate the daemon run dir

    let mut child = KillOnDrop(
        onebrain_mcp(neutral.path(), cache.path(), home.path())
            .spawn()
            .expect("spawn onebrain mcp"),
    );

    // Close stdin immediately — if the process somehow did reach the MCP
    // stdio loop, EOF would make it exit cleanly (0) rather than hang; this
    // keeps the test bounded even if the vault guard were ever removed.
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    {
        use std::io::Read;
        drop(child.0.stdin.take());
        child
            .0
            .stdout
            .take()
            .expect("child stdout")
            .read_to_end(&mut stdout_buf)
            .expect("read stdout");
        child
            .0
            .stderr
            .take()
            .expect("child stderr")
            .read_to_end(&mut stderr_buf)
            .expect("read stderr");
    }
    let status = child.0.wait().expect("wait for onebrain mcp to exit");

    assert_eq!(
        status.code(),
        Some(64),
        "onebrain mcp outside a vault should exit 64 (E_VAULT_NOT_FOUND) before any MCP handshake, got status {:?}, stderr: {}",
        status,
        String::from_utf8_lossy(&stderr_buf)
    );

    // No MCP handshake ever started: stdout must not contain a JSON-RPC frame
    // (e.g. an `initialize` response's `serverInfo`) — the vault check must
    // fire before `serve(stdio())` is ever reached.
    let stdout = String::from_utf8_lossy(&stdout_buf);
    assert!(
        !stdout.contains("jsonrpc") && !stdout.contains("serverInfo"),
        "no JSON-RPC frame should ever be written outside a vault: stdout: {stdout}"
    );
}

#[test]
fn stdio_jsonrpc_handshake_tools_list_and_status_then_clean_exit() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap(); // isolate the daemon run dir (~/.onebrain/run/)
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-mcp-vault\n",
    );
    write(vault.path(), "note.md", "# Hello\nsome vault content\n");

    let mut child = KillOnDrop(
        onebrain_mcp(vault.path(), cache.path(), home.path())
            .spawn()
            .expect("spawn onebrain mcp"),
    );

    let mut stdin = child.0.stdin.take().expect("child stdin");
    let stdout = child.0.stdout.take().expect("child stdout");
    let rx = spawn_line_reader(stdout);

    let deadline = std::time::Instant::now() + Duration::from_secs(30);

    // 1. initialize
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "onebrain-cli-test", "version": "0.0.0" }
            }
        }),
    );
    let init_resp = recv_response(&rx, 1, deadline);
    let init_str = init_resp.to_string();
    assert!(
        init_str.contains("\"serverInfo\""),
        "initialize response missing serverInfo: {init_str}"
    );
    assert_eq!(
        init_resp["result"]["serverInfo"]["name"], "onebrain",
        "unexpected serverInfo: {init_resp}"
    );

    // 2. notifications/initialized (no response expected — it's a notification)
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }),
    );

    // 3. tools/list
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    let tools_resp = recv_response(&rx, 2, deadline);
    let tools_str = tools_resp.to_string();
    for name in ["query", "get", "multi_get", "status"] {
        assert!(
            tools_str.contains(&format!("\"{name}\"")),
            "tools/list missing tool `{name}`: {tools_str}"
        );
    }

    // 4. tools/call status (lex-only path: never constructs the embedder,
    // so this never triggers a model download).
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "status", "arguments": {} }
        }),
    );
    let status_resp = recv_response(&rx, 3, deadline);
    let status_str = status_resp.to_string();
    assert!(
        status_str.contains("\"doc_count\""),
        "status tool response missing doc_count: {status_str}"
    );

    // 5. tools/call get — fs-only (no engine/embedder), reads `note.md` written
    // to the fixture vault above. Asserts the returned content block carries the
    // known file content.
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "get", "arguments": { "file": "note.md" } }
        }),
    );
    let get_resp = recv_response(&rx, 4, deadline);
    let get_str = get_resp.to_string();
    assert!(
        get_str.contains("some vault content"),
        "get tool response missing fixture content: {get_str}"
    );

    // 6. tools/call multi_get — glob matching the fixture file; fs-only. Asserts
    // a `--- <path>` section header appears for the matched file.
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "multi_get", "arguments": { "pattern": "*.md" } }
        }),
    );
    let multi_resp = recv_response(&rx, 5, deadline);
    let multi_str = multi_resp.to_string();
    assert!(
        multi_str.contains("--- note.md"),
        "multi_get tool response missing `--- note.md` section: {multi_str}"
    );

    // 7. tools/call query — a lex sub-query. The fixture index is EMPTY (no
    // reindex ran), so the `query` tool short-circuits to an empty `results`
    // array plus a no-index `note` (rather than erroring on the absent tantivy
    // dir). This still exercises the tool wiring + `Parameters<QueryParams>`
    // camelCase deserialization end-to-end without any model download (lex never
    // constructs the embedder). We only assert the response has a `results`
    // array (shape correct) — not that it has hits.
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": { "searches": [{ "type": "lex", "query": "vault" }] }
            }
        }),
    );
    let query_resp = recv_response(&rx, 6, deadline);
    let query_str = query_resp.to_string();
    assert!(
        query_str.contains("\"results\""),
        "query tool response missing `results` array: {query_str}"
    );

    // 8. Close stdin — the server must notice EOF and exit cleanly.
    drop(stdin);

    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.0.try_wait().expect("polling child status") {
            assert!(
                status.success(),
                "onebrain mcp did not exit 0 after stdin close: {status:?}"
            );
            break;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.0.kill();
            let _ = child.0.wait();
            stop_daemon(cache.path(), home.path());
            panic!("onebrain mcp did not exit within 30s of stdin close");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // The `mcp` server auto-started a daemon in the sandbox HOME; stop it so it
    // doesn't linger past the test (it would idle-exit anyway, but this is tidy).
    stop_daemon(cache.path(), home.path());
}

/// Dual-era guard (v3.5 Gateway epic, PR 1): rmcp 3.x targets MCP
/// `2026-07-28`, but every LOCAL client (Claude Code, Codex, the plugin)
/// still opens the stdio session with a legacy `initialize` carrying its own
/// protocol version. The server must negotiate down to the requested version
/// — echoing it back in the initialize result — or every legacy client
/// breaks at handshake. One spawned server per version keeps failure
/// attribution obvious.
#[test]
fn initialize_negotiates_the_client_requested_protocol_version() {
    for requested in ["2024-11-05", "2025-03-26", "2025-11-25"] {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let home = tempdir().unwrap(); // isolate the daemon run dir
        write(
            vault.path(),
            "onebrain.yml",
            "search:\n  collection: t-mcp-negotiate\n",
        );

        let mut child = KillOnDrop(
            onebrain_mcp(vault.path(), cache.path(), home.path())
                .spawn()
                .expect("spawn onebrain mcp"),
        );
        let mut stdin = child.0.stdin.take().expect("child stdin");
        let stdout = child.0.stdout.take().expect("child stdout");
        let rx = spawn_line_reader(stdout);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);

        send(
            &mut stdin,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": requested,
                    "capabilities": {},
                    "clientInfo": { "name": "onebrain-cli-test", "version": "0.0.0" }
                }
            }),
        );
        let init_resp = recv_response(&rx, 1, deadline);
        assert_eq!(
            init_resp["result"]["protocolVersion"], requested,
            "server must negotiate down to the client's requested protocol \
             version ({requested}); got: {init_resp}"
        );

        // EOF must still produce a clean exit, same bound as the main test.
        drop(stdin);
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = child.0.try_wait().expect("polling child status") {
                assert!(
                    status.success(),
                    "onebrain mcp did not exit 0 after stdin close: {status:?}"
                );
                break;
            }
            if start.elapsed() > Duration::from_secs(30) {
                let _ = child.0.kill();
                let _ = child.0.wait();
                stop_daemon(cache.path(), home.path());
                panic!("onebrain mcp did not exit within 30s of stdin close");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        stop_daemon(cache.path(), home.path());
    }
}

/// Companion guard: a protocol version the SDK does not know must fall back
/// to the SDK default (2025-11-25 in rmcp 3.0.1) rather than erroring or
/// echoing the unknown string. Later Gateway PRs inherit this fallback
/// contract; if an rmcp upgrade flips the default, this test flags it.
#[test]
fn initialize_with_unknown_protocol_version_falls_back_to_sdk_default() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap(); // isolate the daemon run dir
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-mcp-negotiate-unknown\n",
    );

    let mut child = KillOnDrop(
        onebrain_mcp(vault.path(), cache.path(), home.path())
            .spawn()
            .expect("spawn onebrain mcp"),
    );
    let mut stdin = child.0.stdin.take().expect("child stdin");
    let stdout = child.0.stdout.take().expect("child stdout");
    let rx = spawn_line_reader(stdout);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);

    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "1990-01-01",
                "capabilities": {},
                "clientInfo": { "name": "onebrain-cli-test", "version": "0.0.0" }
            }
        }),
    );
    let init_resp = recv_response(&rx, 1, deadline);
    assert_eq!(
        init_resp["result"]["protocolVersion"], "2025-11-25",
        "an unknown requested protocol version must fall back to the SDK \
         default (2025-11-25), not error or echo the unknown string; got: {init_resp}"
    );

    // EOF must still produce a clean exit, same bound as the main test.
    drop(stdin);
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.0.try_wait().expect("polling child status") {
            assert!(
                status.success(),
                "onebrain mcp did not exit 0 after stdin close: {status:?}"
            );
            break;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.0.kill();
            let _ = child.0.wait();
            stop_daemon(cache.path(), home.path());
            panic!("onebrain mcp did not exit within 30s of stdin close");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    stop_daemon(cache.path(), home.path());
}
