//! Approval keep-alive over a real socket (v3.5.0 T3, #412, design D1).
//!
//! Cloudflare gives a response 125 s to START; Claude gives a tool call
//! 300 s to FINISH. An approval-gated call must therefore put bytes on the
//! wire immediately and keep them flowing while a human decides. This file
//! drives the real `onebrain gateway run` binary the way a connector does
//! and times every line of the reply.
//!
//! Sandboxing (HOME/USERPROFILE/ONEBRAIN_CACHE_DIR in a tempdir) and the two
//! BINDING env switches (`ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL`,
//! `ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX`) are copied from
//! `gateway_approval_e2e.rs` — read its module docs for why each exists.
//! The bearer token is planted straight into `tokens.json`, exactly as
//! `gateway_http.rs::plant_valid_access_token` does.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tempfile::{tempdir, TempDir};

mod support;

const PROTOCOL: &str = "2026-07-28";
const TOKEN: &str = "keepalive-e2e-test-token-not-a-secret";

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

fn plant_valid_access_token(home: &Path, token: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut tokens = serde_json::Map::new();
    tokens.insert(
        token.to_string(),
        serde_json::json!({
            "token": token, "kind": "access", "family": "keepalive-e2e-family",
            "client_id": "keepalive-e2e", "scope": "brain", "expires": now + 3600,
            "revoked": false, "rotated_to": null,
        }),
    );
    write(
        home,
        ".onebrain/gateway/tokens.json",
        &serde_json::to_string_pretty(&serde_json::Value::Object(tokens)).unwrap(),
    );
}

fn wait_for_gateway_url(child: &mut std::process::Child, stdout: &Path, stderr: &Path) -> String {
    const PREFIX: &str = "gateway listening on ";
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = std::fs::read_to_string(stdout).unwrap_or_default();
        if let Some(line) = out.lines().find(|l| l.starts_with(PREFIX)) {
            return line[PREFIX.len()..].trim().to_string();
        }
        if let Some(status) = child.try_wait().expect("poll gateway child") {
            let err = std::fs::read_to_string(stderr).unwrap_or_default();
            panic!(
                "gateway exited early ({status}); redacted stderr tail:\n{}",
                support::redacted_capture_tail(&err)
            );
        }
        assert!(
            Instant::now() < deadline,
            "gateway did not start within 30s"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Never put the pairing code (a credential) in a panic message.
fn read_pairing_code(home: &Path) -> String {
    let raw = std::fs::read_to_string(home.join(".onebrain/gateway/pairing.json"))
        .unwrap_or_else(|e| panic!("read sandbox pairing.json: {e}"));
    let json: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("pairing.json not JSON: {e}"));
    json["code"]
        .as_str()
        .expect("pairing.json has a code")
        .to_string()
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

struct Sandbox {
    _root: TempDir,
    home: PathBuf,
    vault: TempDir,
    child: KillOnDrop,
    mcp_url: String,
    approvals_url: String,
    stderr: PathBuf,
}

/// Spawn `gateway run --port 0` with `default_vault` + `vaults.t1` pointing at
/// a fresh vault and `policy_yaml` (an indented `policy:` body) appended.
fn start(policy_yaml: &str) -> Sandbox {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let cache = root.path().join("cache");
    let vault = tempdir().unwrap();
    write(vault.path(), "onebrain.yml", "folders: {}\n");
    write(
        &home,
        ".onebrain/gateway.yml",
        &format!(
            "default_vault: {v}\nvaults:\n  t1: {v}\npolicy:\n{policy_yaml}",
            v = vault.path().display()
        ),
    );
    plant_valid_access_token(&home, TOKEN);
    let stdout = root.path().join("gw.out");
    let stderr = root.path().join("gw.err");
    let child = Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .env("ONEBRAIN_CACHE_DIR", &cache)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL", "1")
        .env("ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX", "1")
        .env_remove("ONEBRAIN_VAULT")
        .current_dir(vault.path())
        .args(["gateway", "run", "--port", "0"])
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .expect("spawn onebrain gateway run");
    let mut child = KillOnDrop(child);
    let mcp_url = wait_for_gateway_url(&mut child.0, &stdout, &stderr);
    let approvals_url = mcp_url.trim_end_matches("/mcp").to_string() + "/approvals";
    Sandbox {
        _root: root,
        home,
        vault,
        child,
        mcp_url,
        approvals_url,
        stderr,
    }
}

/// One non-empty line of the streamed reply and when (since the POST was
/// sent) it arrived.
struct Timed {
    at: Duration,
    line: String,
}

/// POST an approval-gated `brain_capture` and stream its reply line by line
/// on a thread. Returns the reply's content-type, the instant the POST was
/// sent, and the reader. `send()` returns once response HEADERS arrive —
/// which, before T3, was only after the human decided — so it runs on the
/// reader thread and the caller waits at most 5 s (the first-byte bound) for
/// the headers. ureq's `timeout_recv_response` can't express this: it also
/// caps the body read at headers + limit, and the body streams for minutes.
fn open_capture_stream(sb: &Sandbox) -> (String, Instant, JoinHandle<Vec<Timed>>) {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "brain_capture", "arguments": {"title": "keepalive", "text": "hello"}},
    });
    let url = sb.mcp_url.clone();
    let (headers_tx, headers_rx) = std::sync::mpsc::channel::<Result<(u16, String), String>>();
    let t0 = Instant::now();
    let reader = std::thread::spawn(move || {
        let sent = agent(Duration::from_secs(300))
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("MCP-Protocol-Version", PROTOCOL)
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", "brain_capture")
            .send(body.to_string());
        let resp = match sent {
            Ok(resp) => resp,
            Err(e) => {
                let _ = headers_tx.send(Err(e.to_string()));
                return Vec::new();
            }
        };
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let _ = headers_tx.send(Ok((resp.status().as_u16(), content_type)));
        std::io::BufReader::new(resp.into_body().into_reader())
            .lines()
            .map(|l| l.expect("read streamed line"))
            .filter(|l| !l.is_empty())
            .map(|line| Timed {
                at: t0.elapsed(),
                line,
            })
            .collect()
    });
    let (status, content_type) = match headers_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(headers)) => headers,
        // Message wrapper only (no test forces a send error): reports a
        // refused connection etc. instead of a 5 s "first byte" timeout.
        Ok(Err(e)) => panic!("POST /mcp failed: {e}"),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("first byte too late: no response headers within 5s")
        }
        // Unreachable in practice: the thread sends on both send() outcomes
        // before it can return, so this needs a panic before send().
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("unreachable: stream thread ended without reporting send()")
        }
    };
    assert_eq!(status, 200);
    (content_type, t0, reader)
}

/// Bounded poll (10 s) for exactly one pending approval; returns its id.
fn wait_for_one_pending(sb: &Sandbox, code: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut resp = agent(Duration::from_secs(10))
            .get(&sb.approvals_url)
            .header("X-OneBrain-Pairing", code)
            .call()
            .expect("GET /approvals");
        let list: serde_json::Value =
            serde_json::from_str(&resp.body_mut().read_to_string().unwrap()).unwrap();
        if let Some(first) = list.as_array().and_then(|a| a.first()) {
            return first["id"].as_str().unwrap().to_string();
        }
        assert!(Instant::now() < deadline, "no pending approval within 10s");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn resolve(sb: &Sandbox, code: &str, id: &str, decision: &str) {
    let status = agent(Duration::from_secs(10))
        .post(&format!("{}/{id}", sb.approvals_url))
        .header("content-type", "application/json")
        .header("X-OneBrain-Pairing", code)
        .send(serde_json::json!({ "decision": decision }).to_string())
        .expect("POST /approvals/{id}")
        .status()
        .as_u16();
    assert_eq!(status, 200);
}

/// Join the stream reader, but fail fast (instead of blocking until ureq's
/// 300 s global timeout) if the gateway never closes the stream.
fn join_within(reader: JoinHandle<Vec<Timed>>, limit: Duration) -> Vec<Timed> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(reader.join());
    });
    match rx.recv_timeout(limit) {
        Ok(Ok(lines)) => lines,
        Ok(Err(_)) => panic!("stream reader panicked"),
        Err(_) => panic!("stream did not close within {limit:?} after the decision"),
    }
}

/// Wait up to `limit` for `child` to exit; panic otherwise. Used by the
/// shutdown tests (Tasks 3/4).
fn wait_exit(child: &mut std::process::Child, limit: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "process did not exit within {limit:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The stream contract every keep-alive test checks.
fn assert_keepalive_stream(lines: &[Timed], hold: Duration) {
    let rendered: Vec<String> = lines
        .iter()
        .map(|t| format!("{:>6.1}s {}", t.at.as_secs_f64(), t.line))
        .collect();
    let dump = rendered.join("\n");

    let first = lines.first().unwrap_or_else(|| panic!("empty stream"));
    assert!(
        first.at < Duration::from_secs(5),
        "first byte too late:\n{dump}"
    );
    let notice: serde_json::Value = serde_json::from_str(
        first
            .line
            .strip_prefix("data:")
            .unwrap_or_else(|| panic!("first line was not a data event:\n{dump}"))
            .trim_start(),
    )
    .unwrap();
    assert!(
        notice["params"]["data"]
            .as_str()
            .or(notice["params"]["message"].as_str())
            .unwrap_or("")
            .starts_with("waiting for human approval of brain_capture"),
        "{dump}"
    );

    let keepalives = lines.iter().filter(|t| t.line.starts_with(':')).count();
    let expected = (hold.as_secs() / 15) as usize;
    assert!(
        keepalives >= expected.saturating_sub(1).max(1),
        "only {keepalives} keep-alives:\n{dump}"
    );

    let mut prev = Duration::ZERO;
    for t in lines {
        // rmcp emits `:` every 15 s; ≤ 30 s (hub ruling, red-team item 10)
        // absorbs CI scheduling jitter and is still far inside Cloudflare's
        // 125 s response-start limit.
        assert!(
            t.at - prev <= Duration::from_secs(30),
            "gap > 30s before {:?}:\n{dump}",
            t.line
        );
        prev = t.at;
    }

    let body: String = lines.iter().map(|t| format!("{}\n\n", t.line)).collect();
    let reply = support::parse_mcp_reply(&body);
    assert!(
        reply["result"]["structuredContent"]["path"]
            .as_str()
            .unwrap_or("")
            .starts_with("00-inbox/"),
        "{dump}"
    );
}

fn approve_after(hold: Duration) {
    let sb = start("  mutating: ask_once\n");
    let (content_type, _t0, reader) = open_capture_stream(&sb);
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    let code = read_pairing_code(&sb.home);
    let id = wait_for_one_pending(&sb, &code);
    std::thread::sleep(hold);
    resolve(&sb, &code, &id, "approve");
    let lines = join_within(reader, Duration::from_secs(10));
    assert_keepalive_stream(&lines, hold);
    let notes = std::fs::read_dir(sb.vault.path().join("00-inbox"))
        .unwrap()
        .count();
    assert_eq!(notes, 1, "expected exactly one captured note in 00-inbox");
}

/// D1 acceptance (design §4): first byte < 5 s, `:` keep-alives while
/// pending (rmcp sends one every 15 s; the test allows gaps ≤ 30 s), final
/// result on the same stream. 32 s = two keep-alives;
/// the full ≥ 130 s hold is the `#[ignore]`d twin below.
#[test]
fn an_approval_gated_call_streams_at_once_and_keeps_alive_until_approved() {
    approve_after(Duration::from_secs(32));
}

/// Past Cloudflare's 125 s response-start limit. Manual (Task 5):
/// `cargo test -p onebrain-cli --test gateway_keepalive_e2e -- --ignored`.
#[test]
#[ignore = "holds an approval for 130 s — run in the Task 5 verification"]
fn an_approval_held_past_cloudflares_125s_limit_still_completes() {
    approve_after(Duration::from_secs(130));
}

/// The clamp reaches the running process: a configured 900 s wait is
/// announced as 270 s, and stderr names the key.
#[test]
fn an_oversized_approval_wait_is_clamped_to_270s_at_startup() {
    let sb = start("  mutating: ask_once\n  approval_wait_seconds: 900\n");
    let (_ct, _t0, reader) = open_capture_stream(&sb);
    let code = read_pairing_code(&sb.home);
    let id = wait_for_one_pending(&sb, &code);
    resolve(&sb, &code, &id, "deny");
    let lines = join_within(reader, Duration::from_secs(10));
    let body: String = lines.iter().map(|t| format!("{}\n\n", t.line)).collect();
    let notice = &support::sse_data_events(&body)[0];
    let text = notice["params"]["data"]
        .as_str()
        .or(notice["params"]["message"].as_str())
        .unwrap_or("");
    assert_eq!(
        text, "waiting for human approval of brain_capture (up to 270s)",
        "{body}"
    );
    let err = std::fs::read_to_string(&sb.stderr).unwrap();
    assert!(
        err.contains("policy.approval_wait_seconds is 900"),
        "{}",
        support::redacted_capture_tail(&err)
    );
}

/// Review Focus 1 + hub ruling: a restart while a call waits on a human
/// DENIES that approval — the client gets a "shutting down" error as the
/// final event of its own stream, nothing is written — and the process
/// exits 0 long before launchd's 20 s SIGKILL.
#[cfg(unix)]
#[test]
fn sigterm_with_an_approval_pending_denies_it_and_exits_cleanly() {
    let mut sb = start("  mutating: ask_once\n");
    let (_ct, _t0, reader) = open_capture_stream(&sb);
    let code = read_pairing_code(&sb.home);
    let _id = wait_for_one_pending(&sb, &code);

    // SAFETY: plain kill(2) on our own child's pid.
    let rc = unsafe { libc::kill(sb.child.0.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0);
    let started = Instant::now();
    let status = wait_exit(&mut sb.child.0, Duration::from_secs(15));
    assert_eq!(status.code(), Some(0), "{status}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );

    let lines = join_within(reader, Duration::from_secs(10));
    let body: String = lines.iter().map(|t| format!("{}\n\n", t.line)).collect();
    let reply = support::parse_mcp_reply(&body);
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("shutting down"),
        "the pending call must be DENIED on its own stream, not dropped: {body}"
    );
    assert!(
        !sb.vault.path().join("00-inbox").exists()
            || std::fs::read_dir(sb.vault.path().join("00-inbox"))
                .unwrap()
                .count()
                == 0,
        "a call denied at shutdown must write nothing"
    );
    // The audit trail names the shutdown as the denying channel.
    let audit_dir = sb.home.join(".onebrain/gateway/audit");
    let audit: String = std::fs::read_dir(&audit_dir)
        .expect("audit dir")
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    let entry: serde_json::Value = audit
        .lines()
        .map(|l| serde_json::from_str(l).expect("audit line is JSON"))
        .find(|e: &serde_json::Value| e["tool"] == "brain_capture")
        .unwrap_or_else(|| panic!("no brain_capture audit entry: {audit}"));
    assert_eq!(entry["decision"], "denied", "{entry}");
    assert_eq!(entry["channel"], "shutdown", "{entry}");
    let err = std::fs::read_to_string(&sb.stderr).unwrap();
    assert!(
        err.contains("denied pending approvals because the gateway is shutting down"),
        "{}",
        support::redacted_capture_tail(&err)
    );
}
