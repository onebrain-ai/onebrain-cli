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
fn onebrain_mcp(vault_root: &Path, cache_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_onebrain"));
    cmd.env("ONEBRAIN_CACHE_DIR", cache_dir)
        .arg("--vault")
        .arg(vault_root)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
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

#[test]
fn stdio_jsonrpc_handshake_tools_list_and_status_then_clean_exit() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-mcp-vault\n",
    );
    write(vault.path(), "note.md", "# Hello\nsome vault content\n");

    let mut child = KillOnDrop(
        onebrain_mcp(vault.path(), cache.path())
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

    // 5. Close stdin — the server must notice EOF and exit cleanly.
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
            panic!("onebrain mcp did not exit within 30s of stdin close");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
