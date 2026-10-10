//! A held `auth.lock` must never starve the gateway (v3.5.1 T1, #428).
//!
//! Before #428 the gateway held an in-process `Mutex<AuthStore>` across an
//! UNBOUNDED `auth.lock` wait, on the async workers. One stalled CLI holding
//! the lock (Ctrl-Z mid `tokens revoke`) parked `/token` forever, and with
//! it every runtime worker — `/mcp` and SSE keep-alives included.
//!
//! This file drives the real `onebrain gateway run` binary with only two
//! runtime workers (`TOKIO_WORKER_THREADS=2`), holds `auth.lock` from THIS
//! test process (a separate process from the gateway, so the lock conflicts
//! exactly as a CLI's would), and checks that:
//! - a fresh authenticated `/mcp tools/list` still answers in < 1 s;
//! - each of three concurrent `/token` calls gets a 503
//!   `temporarily_unavailable` + `Retry-After: 2` within 6 s;
//! - an already-open approval SSE stream gets a keep-alive while the lock
//!   is held (the hold runs past the 15 s keep-alive cadence).
//!
//! The sandbox helpers are copied from `gateway_keepalive_e2e.rs` (it owns
//! the originals; `tests/support` is deliberately not used here).

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tempfile::{tempdir, TempDir};

const PROTOCOL: &str = "2026-07-28";
const TOKEN: &str = "auth-lock-e2e-test-token-not-a-secret";

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
            "token": token, "kind": "access", "family": "auth-lock-e2e-family",
            "client_id": "auth-lock-e2e", "scope": "brain", "expires": now + 3600,
            "revoked": false, "rotated_to": null,
        }),
    );
    write(
        home,
        ".onebrain/gateway/tokens.json",
        &serde_json::to_string_pretty(&serde_json::Value::Object(tokens)).unwrap(),
    );
    // The token's client is registered too, as for every real token: a call
    // waiting for approval treats an unregistered client as removed (#427).
    let client = serde_json::json!({ "auth-lock-e2e": {
        "client_id": "auth-lock-e2e", "client_name": null, "redirect_uris": [],
        "application_type": "native", "created": now,
    }});
    write(
        home,
        ".onebrain/gateway/clients.json",
        &serde_json::to_string_pretty(&client).unwrap(),
    );
}

fn wait_for_gateway_url(child: &mut std::process::Child, stdout: &Path) -> String {
    const PREFIX: &str = "gateway listening on ";
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = std::fs::read_to_string(stdout).unwrap_or_default();
        if let Some(line) = out.lines().find(|l| l.starts_with(PREFIX)) {
            return line[PREFIX.len()..].trim().to_string();
        }
        if let Some(status) = child.try_wait().expect("poll gateway child") {
            // stderr is not echoed: it can hold sandbox credentials.
            panic!("gateway exited early ({status})");
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
    _vault: TempDir,
    _child: KillOnDrop,
    mcp_url: String,
    base_url: String,
    approvals_url: String,
}

/// Spawn `gateway run --port 0` on a 2-worker runtime with an ask_once
/// mutating policy (so `brain_capture` opens a held SSE stream).
fn start() -> Sandbox {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let cache = root.path().join("cache");
    let vault = tempdir().unwrap();
    write(vault.path(), "onebrain.yml", "folders: {}\n");
    write(
        &home,
        ".onebrain/gateway.yml",
        &format!(
            "default_vault: {v}\nvaults:\n  t1: {v}\npolicy:\n  mutating: ask_once\n",
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
        .env("TOKIO_WORKER_THREADS", "2")
        .env_remove("ONEBRAIN_VAULT")
        .current_dir(vault.path())
        .args(["gateway", "run", "--port", "0"])
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .expect("spawn onebrain gateway run");
    let mut child = KillOnDrop(child);
    let mcp_url = wait_for_gateway_url(&mut child.0, &stdout);
    let base_url = mcp_url.trim_end_matches("/mcp").to_string();
    let approvals_url = format!("{base_url}/approvals");
    Sandbox {
        _root: root,
        home,
        _vault: vault,
        _child: child,
        mcp_url,
        base_url,
        approvals_url,
    }
}

/// Take `auth.lock` the way another `onebrain` process would (own handle,
/// exclusive), and write the holder sidecar naming `pid`.
fn hold_auth_lock(home: &Path, pid: u32) -> std::fs::File {
    let dir = home.join(".onebrain/gateway");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join("auth.lock"))
        .unwrap();
    file.lock().unwrap();
    std::fs::write(
        dir.join("auth.lock.holder"),
        format!(r#"{{"pid":{pid},"version":"9.9.9"}}"#),
    )
    .unwrap();
    file
}

struct Timed {
    at: Duration,
    line: String,
}

/// POST an approval-gated `brain_capture` and stream its reply line by line
/// on a thread (see `gateway_keepalive_e2e.rs::open_capture_stream`).
/// Returns the instant the POST was sent: every [`Timed::at`] is measured
/// from it.
fn open_capture_stream(sb: &Sandbox) -> (String, Instant, JoinHandle<Vec<Timed>>) {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "brain_capture", "arguments": {"title": "auth lock", "text": "hello"}},
    });
    let url = sb.mcp_url.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let t0 = Instant::now();
    std::thread::spawn(move || {
        let sent = agent(Duration::from_secs(300))
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("MCP-Protocol-Version", PROTOCOL)
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", "brain_capture")
            .send(body.to_string())
            .map_err(|e| e.to_string());
        let _ = tx.send(sent);
    });
    let resp = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => panic!("POST /mcp failed: {e}"),
        Err(_) => panic!("first byte too late: no response headers within 5s"),
    };
    assert_eq!(resp.status().as_u16(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let reader = std::thread::spawn(move || {
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

/// Join the stream reader, failing fast if the gateway never closes it.
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

/// What one `/token` call saw.
struct TokenReply {
    elapsed: Duration,
    status: u16,
    retry_after: Option<String>,
    body: String,
}

fn post_token(base_url: &str) -> TokenReply {
    let t = Instant::now();
    let mut resp = agent(Duration::from_secs(20))
        .post(&format!("{base_url}/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .send("grant_type=refresh_token&refresh_token=auth-lock-e2e-not-a-real-refresh")
        .expect("POST /token");
    TokenReply {
        elapsed: t.elapsed(),
        status: resp.status().as_u16(),
        retry_after: resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body: resp.body_mut().read_to_string().unwrap(),
    }
}

/// A fresh connection's authenticated `tools/list`: (elapsed, status, body).
fn tools_list(mcp_url: &str) -> (Duration, u16, String) {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}});
    let t = Instant::now();
    let mut resp = agent(Duration::from_secs(20))
        .post(mcp_url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("MCP-Protocol-Version", PROTOCOL)
        .header("Mcp-Method", "tools/list")
        .send(body.to_string())
        .expect("POST /mcp tools/list");
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap();
    (t.elapsed(), status, text)
}

/// Fire three concurrent `/token` calls while `auth.lock` is held and check
/// each one gives up with the designed 503 within 6 s.
fn token_wave_answers_503(base_url: &str) {
    let tokens: Vec<JoinHandle<TokenReply>> = (0..3)
        .map(|_| {
            let base = base_url.to_string();
            std::thread::spawn(move || post_token(&base))
        })
        .collect();
    for handle in tokens {
        let reply = handle.join().unwrap();
        assert_eq!(reply.status, 503, "{}", reply.body);
        assert!(
            reply.elapsed <= Duration::from_secs(6),
            "/token took {:?} to give up",
            reply.elapsed
        );
        assert_eq!(reply.retry_after.as_deref(), Some("2"));
        let json: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"error": "temporarily_unavailable"})
        );
    }
}

/// How long (on the stream's clock) `auth.lock` stays held. rmcp sends a
/// `:` keep-alive every 15 s, so holding past 17 s guarantees one is due
/// INSIDE the hold.
const HOLD_UNTIL: Duration = Duration::from_secs(17);

/// #428 acceptance (design T1 "Starvation test").
#[test]
fn a_held_auth_lock_starves_nothing_and_token_answers_503() {
    let sb = start();

    // An approval-gated call holds an SSE stream open for the whole test.
    let (content_type, t0, reader) = open_capture_stream(&sb);
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    let code = read_pairing_code(&sb.home);
    let id = wait_for_one_pending(&sb, &code);

    let lock = hold_auth_lock(&sb.home, 424_242);
    let hold_start = t0.elapsed();

    // Waves of three concurrent /token calls keep blocking threads busy on
    // auth.lock for the whole hold. With only two runtime workers, a wait on
    // a worker (or under a shared mutex) would stall everything else: the
    // fresh tools/list below and the stream's keep-alives.
    let mut waves = 0;
    while t0.elapsed() < HOLD_UNTIL {
        let base = sb.base_url.clone();
        let wave = std::thread::spawn(move || token_wave_answers_503(&base));
        std::thread::sleep(Duration::from_millis(500));
        let (elapsed, status, body) = tools_list(&sb.mcp_url);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("brain_search"), "{body}");
        assert!(
            elapsed < Duration::from_secs(1),
            "tools/list took {elapsed:?} while auth.lock was held — the runtime is starved"
        );
        if let Err(panic) = wave.join() {
            std::panic::resume_unwind(panic);
        }
        waves += 1;
    }

    let hold_end = t0.elapsed();
    drop(lock);
    let after = post_token(&sb.base_url);
    assert_eq!(
        after.status, 400,
        "once the lock is free /token must work again (a bogus refresh is invalid_grant): {}",
        after.body
    );

    resolve(&sb, &code, &id, "approve");
    let lines = join_within(reader, Duration::from_secs(10));
    let dump: String = lines
        .iter()
        .map(|t| format!("{:>6.1}s {}\n", t.at.as_secs_f64(), t.line))
        .collect();
    let inside = lines
        .iter()
        .filter(|t| t.line.starts_with(':') && t.at > hold_start && t.at < hold_end)
        .count();
    assert!(
        inside >= 1,
        "no keep-alive inside the hold {hold_start:?}..{hold_end:?} ({waves} /token waves):\n{dump}"
    );
    let mut prev = Duration::ZERO;
    for t in &lines {
        assert!(
            t.at - prev <= Duration::from_secs(30),
            "gap > 30s before {:?}:\n{dump}",
            t.line
        );
        prev = t.at;
    }
    assert!(
        lines.iter().any(|t| t.line.contains("\"result\"")),
        "the approved call never returned its result:\n{dump}"
    );
}

/// #428 holder test: the CLI's busy message names the holder pid recorded in
/// the `auth.lock.holder` sidecar, and the command exits non-zero.
#[test]
fn a_busy_store_fails_clients_remove_naming_the_holder_pid() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".onebrain/gateway")).unwrap();
    let _lock = hold_auth_lock(&home, 424_242);

    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("ONEBRAIN_CACHE_DIR", root.path().join("cache"))
        .env_remove("ONEBRAIN_VAULT")
        .current_dir(root.path())
        .args(["gateway", "clients", "remove", "some-client"])
        .stdin(Stdio::null())
        .output()
        .expect("run onebrain gateway clients remove");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must exit non-zero: {stderr}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the CLI must give up on a held lock, not hang"
    );
    assert!(stderr.contains("pid 424242"), "{stderr}");
    assert!(stderr.contains("busy"), "{stderr}");
}
