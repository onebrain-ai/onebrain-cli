//! Token-optimization end-to-end seam test (Track 4 / plan 4.6).
//!
//! Gated behind `ONEBRAIN_E2E=1` (it spawns a real daemon + a real `onebrain
//! mcp` stdio server, and lex-indexes a scratch collection). Proves the whole
//! wired seam against live processes, not mocks:
//!
//!   1. lex-reindex a scratch vault (no model download)
//!   2. `onebrain mcp` (auto-starts a daemon over the vault)
//!   3. MCP `query`   — fused hits shaped through the funnel (records a gain event)
//!   4. MCP `get`     — first send: FULL body (daemon ledger records the send)
//!   5. MCP `get`     — repeat, unchanged: a REFERENCE envelope, not the body
//!   6. MCP `get --force` — re-materializes the FULL body (bypasses the ledger)
//!   7. `onebrain token gain --history --json` — the recorded events are visible,
//!      including the `mcp_get` surface and the `ledger_ref` cache hit.
//!
//! The vault pins `token_optimization.level: balanced` so the already-sent
//! ledger is active (design §3b, level 2↑); a stable `CLAUDE_CODE_SESSION_ID`
//! makes the session-token resolution deterministic across the two `get`s so
//! the ledger recognizes the repeat.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use tempfile::tempdir;

const SESSION: &str = "e2esessiontoken1";

/// Kills the spawned server on drop so panicking paths never leak an orphan.
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

/// A CLI `onebrain` command in the sandbox (isolated cache + HOME + a stable
/// session id), scoped to `vault_root`.
///
/// `ONEBRAIN_NO_DAEMON=1` forces the direct-engine path (design §5d: memo +
/// ledger open `token.redb` in-process): deterministic regardless of whether
/// the machine already has a daemon on the default port, and fast — no daemon
/// spawn / model-load / bind race. It exercises exactly the seam a lone MCP
/// session hits when no daemon is available.
fn onebrain(vault: &Path, cache: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_onebrain"));
    cmd.env("ONEBRAIN_CACHE_DIR", cache)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CLAUDE_CODE_SESSION_ID", SESSION)
        .env("ONEBRAIN_NO_DAEMON", "1")
        .arg("--vault")
        .arg(vault);
    cmd
}

/// The `onebrain mcp` stdio server (auto-starts a daemon in the sandbox HOME).
fn onebrain_mcp(vault: &Path, cache: &Path, home: &Path) -> Command {
    let mut cmd = onebrain(vault, cache, home);
    cmd.arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn stop_daemon(cache: &Path, home: &Path, vault: &Path) {
    let _ = onebrain(vault, cache, home)
        .args(["daemon", "stop"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn send(stdin: &mut std::process::ChildStdin, value: &serde_json::Value) {
    let mut line = serde_json::to_string(value).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
}

fn spawn_line_reader(stdout: std::process::ChildStdout) -> mpsc::Receiver<serde_json::Value> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                        if tx.send(v).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

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
            }
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for id={expected_id}"),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("onebrain mcp closed stdout before responding to id={expected_id}")
            }
        }
    }
}

/// Extract the concatenated text of a `tools/call` result's content blocks.
fn tool_text(resp: &serde_json::Value) -> String {
    resp["result"]["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn call_get(
    stdin: &mut std::process::ChildStdin,
    rx: &mpsc::Receiver<serde_json::Value>,
    id: i64,
    args: serde_json::Value,
    deadline: std::time::Instant,
) -> String {
    send(
        stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "get", "arguments": args }
        }),
    );
    tool_text(&recv_response(rx, id, deadline))
}

#[test]
fn token_flow_query_get_reference_force_and_gain() {
    if std::env::var("ONEBRAIN_E2E").as_deref() != Ok("1") {
        eprintln!("skipping token_e2e: set ONEBRAIN_E2E=1 to run (spawns daemon + mcp)");
        return;
    }

    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();

    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-token-e2e\ntoken_optimization:\n  level: balanced\n",
    );
    write(
        vault.path(),
        "note.md",
        "# Hello\nsome vault content about rust errors and retries\n",
    );
    // A doc large enough to exceed the balanced get_cap (4000 tokens) so a get
    // must truncate it — for the honesty-marker assertion (B2). ~800 lines of
    // ASCII prose ≈ well over 4000 tokens.
    let big_body: String = (0..800)
        .map(|i| format!("line {i}: rust errors retries and more filler prose here\n"))
        .collect();
    write(vault.path(), "bignote.md", &format!("# Big\n{big_body}"));
    // A doc only the CLI `search get` touches (GAP 3 ledger test).
    write(
        vault.path(),
        "clinote.md",
        "# Cli\ncontent read through the engine by the CLI get surface\n",
    );

    // 1. Full reindex so the daemon's `doc_hash` resolves for the ledger check.
    // (A `--lex-only` pass is incremental — it SKIPS a vault with no index yet,
    // so it can't bootstrap one; the full pass builds the initial index. This
    // loads the embedding model, so the gated e2e needs it cached locally —
    // the same dependency `search_integration.rs`'s embed tests carry.) Done
    // BEFORE mcp starts its daemon, which then opens this index.
    let reindex = onebrain(vault.path(), cache.path(), home.path())
        .args(["search", "reindex"])
        .output()
        .expect("run reindex");
    assert!(
        reindex.status.success(),
        "reindex failed (embedding model must be available for the e2e): {}",
        String::from_utf8_lossy(&reindex.stderr)
    );

    // 2. Spawn `onebrain mcp` (auto-starts a daemon over the vault).
    let mut child = KillOnDrop(
        onebrain_mcp(vault.path(), cache.path(), home.path())
            .spawn()
            .expect("spawn onebrain mcp"),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    let rx = spawn_line_reader(child.0.stdout.take().unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(60);

    // handshake
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26", "capabilities": {},
                "clientInfo": { "name": "token-e2e", "version": "0.0.0" }
            }
        }),
    );
    recv_response(&rx, 1, deadline);
    send(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    );

    // 3. query — exercises the McpQuery shaping seam (records a gain event).
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "query", "arguments": { "searches": [{ "type": "lex", "query": "rust errors" }] } }
        }),
    );
    let query_resp = recv_response(&rx, 2, deadline);
    assert!(
        query_resp["result"].is_object() || query_resp.get("result").is_some(),
        "query returned no result: {query_resp}"
    );

    // 4. get #1 — first send: FULL body.
    let full1 = call_get(
        &mut stdin,
        &rx,
        10,
        serde_json::json!({ "file": "note.md" }),
        deadline,
    );
    assert!(
        full1.contains("some vault content"),
        "first get should return the full body, got: {full1}"
    );
    assert!(
        !full1.contains("sent_earlier"),
        "first get must NOT be a reference"
    );

    // 5. get #2 — repeat, unchanged: a REFERENCE envelope, not the body.
    let ref2 = call_get(
        &mut stdin,
        &rx,
        11,
        serde_json::json!({ "file": "note.md" }),
        deadline,
    );
    assert!(
        ref2.contains("sent_earlier") && ref2.contains("rematerialize"),
        "second get should return a reference envelope, got: {ref2}"
    );
    assert!(
        ref2.contains("--force"),
        "reference must embed the --force re-materialize instruction: {ref2}"
    );

    // 6. get --force — re-materialize the FULL body again.
    let full3 = call_get(
        &mut stdin,
        &rx,
        12,
        serde_json::json!({ "file": "note.md", "force": true }),
        deadline,
    );
    assert!(
        full3.contains("some vault content"),
        "forced get should re-materialize the full body, got: {full3}"
    );
    assert!(
        !full3.contains("sent_earlier"),
        "forced get must bypass the ledger (full body, not a reference)"
    );

    // 6b. #255 index-identity keying: the already-sent ledger now keys on the
    // doc's INDEX identity (`Engine::doc_hash`, the raw file bytes captured at
    // index time), UNIFIED with the read-hook — not a hash of the delivered disk
    // bytes. So an out-of-band DISK edit that is NOT reindexed leaves `doc_hash`
    // unchanged, and a repeat whole-doc get still returns an already-sent
    // REFERENCE; the edit is reflected only after a reindex flips the index hash.
    // (Agent-driven edits go through the PostToolUse search-reindex hook, which
    // DOES reindex, so this stale window only applies to truly out-of-band edits;
    // the reference's `--force` re-materializes the live disk body regardless.)
    // The prior get(id 12) was forced, so it did NOT re-record — the last
    // recorded hash is still H0 from id 10, which equals the (unchanged) index
    // hash, so the ledger reads UNCHANGED. Pre-#255 this asserted the NEW body;
    // that split the get key from the read-hook, the bug #255 fixes.
    write(
        vault.path(),
        "note.md",
        "# Hello\nEDITED OUT OF BAND totally different bytes now\n",
    );
    let after_edit = call_get(
        &mut stdin,
        &rx,
        13,
        serde_json::json!({ "file": "note.md" }),
        deadline,
    );
    assert!(
        after_edit.contains("sent_earlier"),
        "an out-of-band edit without reindex leaves the index hash unchanged, so a \
         repeat get is still an already-sent reference (index-identity keying, #255): {after_edit}"
    );
    assert!(
        after_edit.contains("--force"),
        "the reference must embed the --force re-materialize path to recover the live body: {after_edit}"
    );

    // 6c. BLOCKING-2: a get on a doc bigger than the balanced get_cap truncates
    // the body — and the truncation must be DISCLOSED (design §4), not silent.
    let big = call_get(
        &mut stdin,
        &rx,
        14,
        serde_json::json!({ "file": "bignote.md" }),
        deadline,
    );
    assert!(
        big.contains("[truncated at line"),
        "an over-cap get must append a truncation marker with a cursor: {}",
        &big[big.len().saturating_sub(400)..]
    );
    assert!(
        big.contains("--force"),
        "the truncation marker must tell the agent how to get the full body: {}",
        &big[big.len().saturating_sub(400)..]
    );

    // Close stdin; let the server exit before the CLI ledger steps + gain read.
    drop(stdin);
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if child.0.try_wait().expect("poll child").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // 7. GAP-3: the CLI `search get` structured path also wires the ledger.
    // Two `search get --output json` on the same doc in one session → the
    // second is a reference; `--force` → the full body again.
    let cli_get = |args: &[&str]| -> String {
        let out = onebrain(vault.path(), cache.path(), home.path())
            .args(["search", "get"])
            .args(args)
            .args(["--output", "json"])
            .output()
            .expect("run search get");
        assert!(
            out.status.success(),
            "search get failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let cli1 = cli_get(&["clinote.md"]);
    assert!(
        cli1.contains("read through the engine") && !cli1.contains("sent_earlier"),
        "first CLI search get returns the full body: {cli1}"
    );
    let cli2 = cli_get(&["clinote.md"]);
    assert!(
        cli2.contains("sent_earlier") && cli2.contains("rematerialize"),
        "second CLI search get in the same session returns a reference: {cli2}"
    );
    let cli_forced = cli_get(&["clinote.md", "--force"]);
    assert!(
        cli_forced.contains("read through the engine") && !cli_forced.contains("sent_earlier"),
        "CLI search get --force bypasses the ledger and returns the full body: {cli_forced}"
    );

    // 8. token gain --history --json — the raw per-call events the surfaces
    // recorded (JSONL source of truth). Must include the get surface and the
    // ledger reference hit.
    let gain = onebrain(vault.path(), cache.path(), home.path())
        .args(["token", "gain", "--history", "--json"])
        .output()
        .expect("run token gain");
    assert!(
        gain.status.success(),
        "token gain failed: {}",
        String::from_utf8_lossy(&gain.stderr)
    );
    let out = String::from_utf8_lossy(&gain.stdout);
    let v: serde_json::Value = serde_json::from_str(&out).expect("token gain --json is JSON");
    let history = serde_json::to_string(&v).unwrap();
    assert!(
        history.contains("mcp_get"),
        "gain history should record the mcp_get surface: {history}"
    );
    assert!(
        history.contains("mcp_query"),
        "gain history should record the mcp_query surface: {history}"
    );
    assert!(
        history.contains("ledger_ref"),
        "gain history should record the ledger reference hit: {history}"
    );
    assert!(
        history.contains("cli_search"),
        "gain history should record the CLI search get surface: {history}"
    );

    stop_daemon(cache.path(), home.path(), vault.path());
}
