//! #427 + #430 end to end, against a REAL spawned `onebrain gateway run`
//! (sandboxed `HOME`/cache, temp vault) driven over real HTTP, with the
//! real `onebrain gateway tokens|clients` CLI doing the revoking — the CLI
//! and the gateway share nothing but the on-disk auth store, which is the
//! whole "revocation needs no IPC" design (`commands/gateway/access.rs`).
//!
//! - **#427:** a call waiting for a human is denied (audit channel
//!   `"revoked"`, nothing written, Telegram edited to "Access was revoked")
//!   within one periodic check after any of the three revoke verbs; an
//!   Allow that lands after a revoke writes nothing; a token that merely
//!   EXPIRES mid-wait is not revoked; an `ask_once` grant does not survive
//!   `tokens revoke --client` + a fresh consent.
//! - **#430:** SIGTERM with a Telegram prompt pending: the "Gateway
//!   stopped" edit completes before the process exits, even when the Bot
//!   API takes ~1.5 s to answer it — 20 runs out of 20.
//!
//! Helpers are COPIED from `gateway_telegram_e2e.rs` /
//! `gateway_approval_e2e.rs` (this crate has no library target, and
//! `tests/support/mod.rs` is not extended here). The Telegram side is a
//! mock Bot API on loopback (`ONEBRAIN_TELEGRAM_API_BASE`); nothing leaves
//! the machine. `ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL=1` and
//! `ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX=1` are set for the same reasons
//! those files give.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::tempdir;

mod support;

// ── Copied from `gateway_approval_e2e.rs` (see module docs: not shared) ────

/// Kills the spawned gateway on drop so a panicking assertion never leaks an
/// orphan `onebrain gateway run` process on CI.
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

/// Spawn `onebrain gateway run --port 0`, scoped to a sandbox HOME/cache and
/// a given cwd, with stdout/stderr redirected to files this test polls.
/// `telegram_api_base` is THIS test's own mock Bot API server's base URL
/// (`http://127.0.0.1:<port>`) — see the module docs for why that alone is
/// what keeps this whole file off the real network.
fn spawn_gateway(
    cache_dir: &Path,
    home: &Path,
    cwd: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    telegram_api_base: &str,
) -> std::process::Child {
    let stdout_file = std::fs::File::create(stdout_path).unwrap();
    let stderr_file = std::fs::File::create(stderr_path).unwrap();
    Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .env("ONEBRAIN_CACHE_DIR", cache_dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL", "1")
        .env("ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX", "1")
        .env("ONEBRAIN_TELEGRAM_API_BASE", telegram_api_base)
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
/// http://<bound-addr>/mcp`), returning the parsed `/mcp` URL. See
/// `gateway_approval_e2e.rs`'s own copy for the full redaction rationale —
/// identical here, just duplicated.
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

/// Bounded poll (10s) that the child has exited after being killed.
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

/// A `ureq` agent that never turns a non-2xx status into an `Err` and never
/// follows a redirect — see `gateway_approval_e2e.rs`'s own copy.
fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

const PROTOCOL: &str = "2026-07-28";

fn init_body(id: u32) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "gateway-revoke-e2e-test", "version": "0.0.0"},
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
/// `MCP-Protocol-Version: 2026-07-28` is set.
fn standard_headers<'a>(method: &'a str, name: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut headers = vec![("MCP-Protocol-Version", PROTOCOL), ("Mcp-Method", method)];
    if let Some(name) = name {
        headers.push(("Mcp-Name", name));
    }
    headers
}

/// POST one JSON-RPC `body` to `/mcp` with `token` as the `Authorization:
/// Bearer` credential, plus `extra` headers, returning `(status,
/// response_text)`.
fn post_mcp(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    body: &serde_json::Value,
    extra: &[(&str, &str)],
) -> (u16, String) {
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
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|e| panic!("read response body from {url}: {e}"));
    (status, text)
}

/// GET `url` with no auth, returning `(status, body_text)`.
fn get(agent: &ureq::Agent, url: &str) -> (u16, String) {
    let mut resp = agent
        .get(url)
        .call()
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|e| panic!("read response body from {url}: {e}"));
    (status, text)
}

/// POST a JSON `body` to `url` (no auth — `/register` is public), returning
/// `(status, body_text)`.
fn post_json(agent: &ureq::Agent, url: &str, body: &serde_json::Value) -> (u16, String) {
    let mut resp = agent
        .post(url)
        .header("content-type", "application/json")
        .send(body.to_string())
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|e| panic!("read response body from {url}: {e}"));
    (status, text)
}

/// Base64url (RFC 4648 §5), unpadded — copied (not shared: no library
/// target) so this test can build its own PKCE pair.
const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_nopad(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let c0 = b0 >> 2;
        let c1 = ((b0 & 0x03) << 4) | (b1 >> 4);
        let c2 = ((b1 & 0x0f) << 2) | (b2 >> 6);
        let c3 = b2 & 0x3f;
        out.push(B64URL_ALPHABET[c0 as usize] as char);
        out.push(B64URL_ALPHABET[c1 as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL_ALPHABET[c2 as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_ALPHABET[c3 as usize] as char);
        }
    }
    out
}

/// A real RFC 7636 S256 PKCE pair.
fn pkce_pair() -> (String, String) {
    use sha2::{Digest, Sha256};
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS CSPRNG unavailable for test PKCE verifier");
    let verifier = base64url_nopad(&buf);
    let challenge = base64url_nopad(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// RFC 3986 §2.3 unreserved-only percent-encoder for
/// `application/x-www-form-urlencoded` POST bodies.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Parse `?a=b&c=d` (or a bare `a=b&c=d`) into a map, percent-decoding every
/// value.
fn parse_query(qs: &str) -> std::collections::HashMap<String, String> {
    qs.trim_start_matches('?')
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let mut it = pair.splitn(2, '=');
            let k = it.next().unwrap_or("").to_string();
            let v = percent_decode(it.next().unwrap_or(""));
            (k, v)
        })
        .collect()
}

/// POST `/authorize` form-urlencoded, returning `(status, body_text,
/// location_header)`.
fn post_authorize(
    agent: &ureq::Agent,
    url: &str,
    pairs: &[(&str, &str)],
) -> (u16, String, Option<String>) {
    let mut resp = agent
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .send(form_encode(pairs))
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));
    let status = resp.status().as_u16();
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let text = resp
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|e| panic!("read response body from {url}: {e}"));
    (status, text, location)
}

/// POST `/token` form-urlencoded, returning `(status, body_text)`.
fn post_token(agent: &ureq::Agent, url: &str, pairs: &[(&str, &str)]) -> (u16, String) {
    let mut resp = agent
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .send(form_encode(pairs))
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|e| panic!("read response body from {url}: {e}"));
    (status, text)
}

/// Read the current device-pairing code straight out of the sandboxed
/// `$HOME/.onebrain/gateway/pairing.json` — see `gateway_oauth_e2e.rs`'s own
/// doc comment for why this is a legitimate shortcut, copied via
/// `gateway_approval_e2e.rs`. Nothing derived from the file's CONTENTS or
/// PATH reaches a panic message — see that file's identical helper for the
/// full rationale.
fn read_pairing_code(home: &Path) -> String {
    let path = home.join(".onebrain").join("gateway").join("pairing.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read the sandbox gateway pairing.json: {e}"));
    let json: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("the sandbox gateway pairing.json was not JSON: {e}"));
    json["code"].as_str().map(str::to_string).unwrap_or_else(|| {
        let keys: Vec<&str> = json
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        panic!("the sandbox gateway pairing.json had no string \"code\" field; top-level keys: {keys:?}")
    })
}

/// Reads every JSONL line back out of `{home}/.onebrain/gateway/audit/`,
/// across every month file present (this test is short-lived, so in
/// practice always exactly one file) — copied verbatim from
/// `gateway_approval_e2e.rs`.
fn read_audit_entries(home: &Path) -> Vec<serde_json::Value> {
    let dir = home.join(".onebrain").join("gateway").join("audit");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    files
        .into_iter()
        .flat_map(|f| {
            std::fs::read_to_string(&f)
                .unwrap_or_default()
                .lines()
                .map(|l| {
                    serde_json::from_str(l)
                        .unwrap_or_else(|e| panic!("bad audit-log line ({e}): {l}"))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Count of `.md` files directly under `<vault_root>/00-inbox` — `0` (not a
/// panic) when the folder doesn't exist yet.
fn inbox_note_count(vault_root: &Path) -> usize {
    let inbox = vault_root.join("00-inbox");
    if !inbox.is_dir() {
        return 0;
    }
    std::fs::read_dir(&inbox)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
        .count()
}

// ── A mock Telegram Bot API server (with per-method delay) ────────────────

use axum::extract::{Path as AxumPath, State};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

/// One request the mock answered: method, body, and when the mock FINISHED
/// handling it (after any scripted delay) — "the edit landed" for #430.
#[derive(Clone)]
struct Recorded {
    method: String,
    body: Value,
    /// Read only by the unix-only shutdown test.
    #[cfg(unix)]
    done_at: Instant,
}

#[derive(Clone, Default)]
struct MockState {
    responses: Arc<Mutex<HashMap<String, Value>>>,
    queued: Arc<Mutex<HashMap<String, std::collections::VecDeque<Value>>>>,
    delays: Arc<Mutex<HashMap<String, Duration>>>,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockState {
    fn set_response(&self, method: &str, body: Value) {
        self.responses
            .lock()
            .unwrap()
            .insert(method.to_string(), body);
    }

    /// A one-shot response for `method`, served before the scripted one.
    fn queue_response(&self, method: &str, body: Value) {
        self.queued
            .lock()
            .unwrap()
            .entry(method.to_string())
            .or_default()
            .push_back(body);
    }

    /// Used only by the unix-only shutdown test.
    #[cfg(unix)]
    fn set_delay(&self, method: &str, d: Duration) {
        self.delays.lock().unwrap().insert(method.to_string(), d);
    }

    fn find(&self, method: &str) -> Option<Recorded> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.method == method)
            .cloned()
    }
}

/// Records `(method, body)` once any delay for `method` has elapsed, then
/// answers with the scripted response, else `{"ok":true,"result":null}`
/// (an empty `getUpdates` batch).
async fn mock_handler(
    AxumPath(params): AxumPath<HashMap<String, String>>,
    State(state): State<MockState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let method = params.get("method").cloned().unwrap_or_default();
    let delay = state.delays.lock().unwrap().get(&method).copied();
    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    } else if method == "getUpdates" {
        // Keep the poller's unscripted long-poll from spinning hot.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    state.requests.lock().unwrap().push(Recorded {
        method: method.clone(),
        body,
        #[cfg(unix)]
        done_at: Instant::now(),
    });
    let queued = state
        .queued
        .lock()
        .unwrap()
        .get_mut(&method)
        .and_then(|q| q.pop_front());
    let scripted = queued.or_else(|| state.responses.lock().unwrap().get(&method).cloned());
    Json(scripted.unwrap_or_else(|| serde_json::json!({ "ok": true, "result": null })))
}

struct MockServer {
    base: String,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(state: MockState) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let port = Arc::new(AtomicU16::new(0));
        let stop_thread = stop.clone();
        let port_thread = port.clone();
        let join = std::thread::spawn(move || {
            let router = Router::new()
                .route("/{bot_and_token}/{method}", post(mock_handler))
                .with_state(state);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                port_thread.store(listener.local_addr().unwrap().port(), Ordering::SeqCst);
                let server = axum::serve(listener, router);
                let graceful = server.with_graceful_shutdown(async move {
                    while !stop_thread.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                });
                let _ = graceful.await;
            });
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let bound = loop {
            let p = port.load(Ordering::SeqCst);
            if p != 0 {
                break p;
            }
            assert!(Instant::now() < deadline, "mock bot api server never bound");
            std::thread::sleep(Duration::from_millis(10));
        };
        Self {
            base: format!("http://127.0.0.1:{bound}"),
            stop,
            join: Some(join),
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn wait_for_request(state: &MockState, method: &str, timeout: Duration) -> Recorded {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(r) = state.find(method) {
            return r;
        }
        assert!(
            Instant::now() < deadline,
            "no {method} request reached the mock Bot API within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ── Fixture, OAuth, CLI ───────────────────────────────────────────────────

const CHAT_ID: i64 = 918_273_646;
const REDIRECT_URI: &str = "http://127.0.0.1/callback";

fn write_fixture_vault_and_config(home: &Path, bot_token: &str) -> tempfile::TempDir {
    let vault = tempdir().unwrap();
    write(vault.path(), "onebrain.yml", "folders: {}\n");
    write(
        home,
        ".onebrain/gateway.yml",
        &format!(
            "default_vault: {v}\nvaults:\n  t1: {v}\npolicy:\n  mutating: ask_once\ntelegram:\n  bot_token: \"{bot_token}\"\n  chat_id: {CHAT_ID}\n",
            v = vault.path().display(),
        ),
    );
    vault
}

struct Harness {
    child: KillOnDrop,
    mcp_url: String,
    approvals_url: String,
    authorize_url: String,
    token_url: String,
    client_id: String,
    access_token: String,
    refresh_token: String,
    home: tempfile::TempDir,
    vault: tempfile::TempDir,
    _cache: tempfile::TempDir,
    _cwd: tempfile::TempDir,
}

/// Authorize `client_id` with the sandbox's pairing code and redeem the
/// code: one fresh consent, hence one fresh token family. Returns the
/// `(access, refresh)` tokens (never printed).
fn consent(
    agent: &ureq::Agent,
    home: &Path,
    authorize_url: &str,
    token_url: &str,
    client_id: &str,
) -> (String, String) {
    let (verifier, challenge) = pkce_pair();
    let pairing = read_pairing_code(home);
    let params = [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", REDIRECT_URI),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", "revoke-e2e"),
        ("pairing_code", pairing.as_str()),
    ];
    let (status, body, location) = post_authorize(agent, authorize_url, &params);
    assert_eq!(status, 302, "{body}");
    let location = location.expect("302 with a Location header");
    let query = &location[location.find('?').expect("query string")..];
    let code = parse_query(query)
        .remove("code")
        .expect("authorization code in the redirect");
    let (status, token_body) = post_token(
        agent,
        token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", client_id),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", verifier.as_str()),
        ],
    );
    assert_eq!(status, 200, "token exchange failed");
    let tokens: Value = serde_json::from_str(&token_body).expect("token response JSON");
    let field = |k: &str| {
        tokens[k]
            .as_str()
            .unwrap_or_else(|| panic!("no {k} in the token response"))
            .to_string()
    };
    (field("access_token"), field("refresh_token"))
}

/// Rotate `refresh` (same token family); returns the new access token.
fn refresh_access(token_url: &str, refresh: &str) -> String {
    let (status, body) = post_token(
        &http_agent(),
        token_url,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh)],
    );
    assert_eq!(status, 200, "refresh rotation failed");
    let tokens: Value = serde_json::from_str(&body).expect("refresh response JSON");
    tokens["access_token"]
        .as_str()
        .expect("an access_token in the refresh response")
        .to_string()
}

/// Spawn the sandboxed gateway (Telegram wired to `mock_base`), register a
/// client, consent once, and `initialize`.
fn spawn_and_authenticate(mock_base: &str, bot_token: &str) -> Harness {
    let home = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let vault = write_fixture_vault_and_config(home.path(), bot_token);
    let stdout_path = cwd.path().join("gateway-stdout.log");
    let stderr_path = cwd.path().join("gateway-stderr.log");
    let mut child = KillOnDrop(spawn_gateway(
        cache.path(),
        home.path(),
        cwd.path(),
        &stdout_path,
        &stderr_path,
        mock_base,
    ));
    let mcp_url = wait_for_gateway_url(&mut child.0, &stdout_path, &stderr_path);
    let agent = http_agent();
    let origin = mcp_url
        .strip_suffix("/mcp")
        .expect("mcp url ends in /mcp")
        .to_string();

    let (status, meta_body) = get(
        &agent,
        &format!("{origin}/.well-known/oauth-authorization-server"),
    );
    assert_eq!(status, 200, "{meta_body}");
    let meta: Value = serde_json::from_str(&meta_body).unwrap();
    let field = |k: &str| {
        meta[k]
            .as_str()
            .unwrap_or_else(|| panic!("no {k}"))
            .to_string()
    };
    let (authorize_url, token_url, register_url) = (
        field("authorization_endpoint"),
        field("token_endpoint"),
        field("registration_endpoint"),
    );

    let (status, reg_body) = post_json(
        &agent,
        &register_url,
        &serde_json::json!({
            "client_name": "gateway-revoke-e2e-client",
            "redirect_uris": [REDIRECT_URI],
            "application_type": "native",
        }),
    );
    assert_eq!(status, 201, "{reg_body}");
    let client_id = serde_json::from_str::<Value>(&reg_body).unwrap()["client_id"]
        .as_str()
        .expect("client_id")
        .to_string();

    let (access_token, refresh_token) =
        consent(&agent, home.path(), &authorize_url, &token_url, &client_id);
    let (status, init) = post_mcp(
        &agent,
        &mcp_url,
        &access_token,
        &init_body(1),
        &[("MCP-Protocol-Version", PROTOCOL)],
    );
    assert_eq!(status, 200, "{init}");

    Harness {
        child,
        approvals_url: format!("{origin}/approvals"),
        mcp_url,
        authorize_url,
        token_url,
        client_id,
        access_token,
        refresh_token,
        home,
        vault,
        _cache: cache,
        _cwd: cwd,
    }
}

/// Run the real `onebrain` CLI against the sandbox HOME (no gateway IPC —
/// it only writes the store), asserting success. Callers pass a client id
/// as `--client=<id>` or after `--`: ids are random base64url and may
/// start with `-`, which clap would otherwise read as a flag.
fn onebrain_cli(home: &Path, args: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("ONEBRAIN_VAULT")
        .current_dir(home)
        .args(args)
        .output()
        .expect("spawn the onebrain CLI");
    assert!(
        out.status.success(),
        "onebrain {} failed: {}",
        args.get(2).copied().unwrap_or(""),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Same algorithm as `store.rs::display_id` (copied: no library target).
fn display_id(s: &str) -> String {
    use sha2::{Digest, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(12);
    for &b in &digest[..6] {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// `GET /approvals` with the operator pairing code: the pending ids.
fn pending_ids(h: &Harness) -> Vec<String> {
    let mut resp = http_agent()
        .get(&h.approvals_url)
        .header("X-OneBrain-Pairing", read_pairing_code(h.home.path()))
        .call()
        .expect("GET /approvals");
    assert_eq!(resp.status().as_u16(), 200);
    let list: Value = serde_json::from_str(&resp.body_mut().read_to_string().unwrap()).unwrap();
    list.as_array()
        .expect("a JSON array")
        .iter()
        .map(|e| e["id"].as_str().expect("an id").to_string())
        .collect()
}

fn wait_for_one_pending(h: &Harness) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ids = pending_ids(h);
        assert!(ids.len() <= 1, "{ids:?}");
        if let Some(id) = ids.into_iter().next() {
            return id;
        }
        assert!(
            Instant::now() < deadline,
            "no pending approval appeared within 10s"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `POST /approvals/{id}` as the operator; `(status, body)`.
fn resolve(h: &Harness, id: &str, decision: &str) -> (u16, String) {
    let url = format!("{}/{id}", h.approvals_url);
    let mut resp = http_agent()
        .post(&url)
        .header("content-type", "application/json")
        .header("X-OneBrain-Pairing", read_pairing_code(h.home.path()))
        .send(serde_json::json!({ "decision": decision }).to_string())
        .expect("POST /approvals/{id}");
    let status = resp.status().as_u16();
    (status, resp.body_mut().read_to_string().unwrap())
}

/// Start one gated `brain_capture` with `token` on a background thread.
fn start_capture(h: &Harness, token: &str, title: &str) -> std::thread::JoinHandle<(u16, String)> {
    let (url, token, title) = (h.mcp_url.clone(), token.to_string(), title.to_string());
    std::thread::spawn(move || {
        post_mcp(
            &http_agent(),
            &url,
            &token,
            &call_body(
                2,
                "brain_capture",
                serde_json::json!({ "title": title, "text": "revoke e2e body" }),
            ),
            &standard_headers("tools/call", Some("brain_capture")),
        )
    })
}

/// The JSON-RPC error message of a finished call (panics on success).
fn error_message(call: (u16, String)) -> String {
    let (status, body) = call;
    assert_eq!(status, 200, "{body}");
    let reply = support::parse_mcp_reply(&body);
    reply["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a JSON-RPC error: {reply}"))
        .to_string()
}

fn mock_with_send(message_id: i64) -> (MockState, MockServer) {
    let state = MockState::default();
    state.set_response(
        "sendMessage",
        serde_json::json!({ "ok": true, "result": { "message_id": message_id } }),
    );
    let server = MockServer::start(state.clone());
    (state, server)
}

/// The shared body of the three revoke-verb tests: a pending capture is
/// denied within one periodic check (≤ 7 s) after `revoke` runs, nothing
/// is written, the audit names `revoked`, and Telegram is told.
fn assert_revoke_verb_denies_a_pending_capture(revoke: impl FnOnce(&Harness)) {
    let (mock_state, mock) = mock_with_send(4270);
    let mut h = spawn_and_authenticate(&mock.base, "revoke-verb-bot-token");
    let call = start_capture(&h, &h.access_token, "Revoke Verb");
    wait_for_one_pending(&h);
    wait_for_request(&mock_state, "sendMessage", Duration::from_secs(10));

    revoke(&h);
    let revoked_at = Instant::now();
    let deadline = revoked_at + Duration::from_secs(7);
    while !call.is_finished() {
        assert!(
            Instant::now() < deadline,
            "the pending call was not denied within 7 s of the revoke"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let message = error_message(call.join().unwrap());
    assert!(
        message.contains("access was revoked while this call waited for approval"),
        "{message}"
    );
    assert_eq!(
        inbox_note_count(h.vault.path()),
        0,
        "nothing may be written"
    );
    assert!(pending_ids(&h).is_empty());

    let entries = read_audit_entries(h.home.path());
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["decision"], "denied", "{entries:?}");
    assert_eq!(entries[0]["channel"], "revoked", "{entries:?}");

    let edit = wait_for_request(&mock_state, "editMessageText", Duration::from_secs(10));
    let text = edit.body["text"].as_str().unwrap_or_default();
    assert!(
        text.starts_with("\u{26d4} Access was revoked \u{b7} nothing was written"),
        "{text}"
    );
    assert_eq!(
        edit.body["reply_markup"]["inline_keyboard"],
        serde_json::json!([])
    );
    assert_exits_after_kill(&mut h.child.0);
}

// ── #427 ──────────────────────────────────────────────────────────────────

#[test]
fn tokens_revoke_by_access_id_denies_a_pending_capture() {
    assert_revoke_verb_denies_a_pending_capture(|h| {
        let id = display_id(&h.access_token);
        onebrain_cli(h.home.path(), &["gateway", "tokens", "revoke", &id]);
    });
}

#[test]
fn tokens_revoke_client_denies_a_pending_capture() {
    assert_revoke_verb_denies_a_pending_capture(|h| {
        onebrain_cli(
            h.home.path(),
            &[
                "gateway",
                "tokens",
                "revoke",
                &format!("--client={}", h.client_id),
            ],
        );
    });
}

#[test]
fn clients_remove_denies_a_pending_capture() {
    assert_revoke_verb_denies_a_pending_capture(|h| {
        onebrain_cli(
            h.home.path(),
            &["gateway", "clients", "remove", "--", &h.client_id],
        );
    });
}

/// The Allow-time check: the operator approves right after the revoke —
/// well before the first periodic check (5 s after the call started) — and
/// still nothing is written.
#[test]
fn an_allow_after_a_revoke_writes_nothing() {
    let (_mock_state, mock) = mock_with_send(4271);
    let mut h = spawn_and_authenticate(&mock.base, "allow-after-revoke-bot-token");
    let started = Instant::now();
    let call = start_capture(&h, &h.access_token, "Allow After Revoke");
    let id = wait_for_one_pending(&h);

    onebrain_cli(
        h.home.path(),
        &[
            "gateway",
            "tokens",
            "revoke",
            &format!("--client={}", h.client_id),
        ],
    );
    let (status, body) = resolve(&h, &id, "approve");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the Allow must land before the first periodic check for this test to mean anything ({:?})",
        started.elapsed()
    );
    assert_eq!(status, 200, "the approval was still pending: {body}");

    let message = error_message(call.join().unwrap());
    assert!(message.contains("access was revoked"), "{message}");
    assert_eq!(
        inbox_note_count(h.vault.path()),
        0,
        "nothing may be written"
    );
    let entries = read_audit_entries(h.home.path());
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["channel"], "revoked", "{entries:?}");
    assert_exits_after_kill(&mut h.child.0);
}

/// "Revoked" never means merely "expired": the access token expires while
/// the call waits (past a periodic check), and the human's Allow still
/// writes.
#[test]
fn an_access_token_expiring_mid_wait_is_not_revoked() {
    let (_mock_state, mock) = mock_with_send(4272);
    let mut h = spawn_and_authenticate(&mock.base, "expiry-bot-token");
    let call = start_capture(&h, &h.access_token, "Expires Mid Wait");
    let id = wait_for_one_pending(&h);

    // Age the access token on disk (the store has no "expire" API).
    let tokens_path = h.home.path().join(".onebrain/gateway/tokens.json");
    let mut tokens: Value =
        serde_json::from_str(&std::fs::read_to_string(&tokens_path).unwrap()).unwrap();
    tokens[h.access_token.as_str()]["expires"] = serde_json::json!(1);
    std::fs::write(&tokens_path, serde_json::to_vec_pretty(&tokens).unwrap()).unwrap();

    // Past the first periodic check (5 s).
    std::thread::sleep(Duration::from_millis(6500));
    assert!(
        !call.is_finished(),
        "an expired token must not deny the call"
    );
    assert_eq!(pending_ids(&h), vec![id.clone()]);

    let (status, body) = resolve(&h, &id, "approve");
    assert_eq!(status, 200, "{body}");
    let (status, body) = call.join().unwrap();
    assert_eq!(status, 200, "{body}");
    let reply = support::parse_mcp_reply(&body);
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(inbox_note_count(h.vault.path()), 1);
    let entries = read_audit_entries(h.home.path());
    assert_eq!(entries[0]["channel"], "http", "{entries:?}");
    assert_exits_after_kill(&mut h.child.0);
}

/// An `ask_once` grant dies with the consent it came from: after
/// `tokens revoke --client` and a fresh consent (new token family) the same
/// client is asked again.
#[test]
fn an_ask_once_grant_is_not_reused_after_revoke_client_and_reconsent() {
    let (_mock_state, mock) = mock_with_send(4273);
    let mut h = spawn_and_authenticate(&mock.base, "grant-bot-token");

    let call = start_capture(&h, &h.access_token, "Granted Once");
    let id = wait_for_one_pending(&h);
    assert_eq!(resolve(&h, &id, "approve").0, 200);
    let (status, body) = call.join().unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(inbox_note_count(h.vault.path()), 1);

    onebrain_cli(
        h.home.path(),
        &[
            "gateway",
            "tokens",
            "revoke",
            &format!("--client={}", h.client_id),
        ],
    );
    let (fresh, _) = consent(
        &http_agent(),
        h.home.path(),
        &h.authorize_url,
        &h.token_url,
        &h.client_id,
    );

    let call = start_capture(&h, &fresh, "Must Ask Again");
    let id = wait_for_one_pending(&h);
    assert_eq!(
        inbox_note_count(h.vault.path()),
        1,
        "the old grant must not have let the second capture through"
    );
    assert_eq!(resolve(&h, &id, "deny").0, 200);
    let message = error_message(call.join().unwrap());
    assert!(message.contains("denied"), "{message}");
    assert_exits_after_kill(&mut h.child.0);
}

/// Grant/revocation parity across channels: after a single-token
/// `tokens revoke <access-id>` (the family survives — the client can still
/// refresh), an Allow that arrives anyway must leave NO grant behind,
/// whichever channel delivered it. Otherwise the next call — made with a
/// refreshed token of the same family — would be auto-allowed by that grant
/// and the revoke defeated. `allow` delivers the Allow for pending `id`.
fn assert_an_allow_after_a_single_token_revoke_leaves_no_grant(
    message_id: i64,
    allow: impl FnOnce(&Harness, &MockState, &str),
) {
    let (mock_state, mock) = mock_with_send(message_id);
    let mut h = spawn_and_authenticate(&mock.base, "grant-parity-bot-token");
    let call = start_capture(&h, &h.access_token, "Revoked Then Allowed");
    let id = wait_for_one_pending(&h);
    wait_for_request(&mock_state, "sendMessage", Duration::from_secs(10));

    let access_id = display_id(&h.access_token);
    onebrain_cli(h.home.path(), &["gateway", "tokens", "revoke", &access_id]);
    allow(&h, &mock_state, &id);
    let message = error_message(call.join().unwrap());
    assert!(message.contains("access was revoked"), "{message}");
    assert_eq!(
        inbox_note_count(h.vault.path()),
        0,
        "nothing may be written"
    );

    // Same family, fresh access token: the client is asked again.
    let refreshed = refresh_access(&h.token_url, &h.refresh_token);
    let call = start_capture(&h, &refreshed, "After The Revoke");
    let id = wait_for_one_pending(&h);
    assert_eq!(
        inbox_note_count(h.vault.path()),
        0,
        "a grant recorded by the revoked Allow let the next call through"
    );
    assert_eq!(resolve(&h, &id, "deny").0, 200);
    assert!(error_message(call.join().unwrap()).contains("denied"));
    assert_exits_after_kill(&mut h.child.0);
}

#[test]
fn an_http_allow_after_a_single_token_revoke_leaves_no_grant() {
    assert_an_allow_after_a_single_token_revoke_leaves_no_grant(4274, |h, _, id| {
        let (status, body) = resolve(h, id, "approve");
        assert_eq!(status, 200, "the approval was still pending: {body}");
    });
}

#[test]
fn a_telegram_allow_after_a_single_token_revoke_leaves_no_grant() {
    assert_an_allow_after_a_single_token_revoke_leaves_no_grant(4275, |h, mock, id| {
        mock.queue_response(
            "getUpdates",
            serde_json::json!({
                "ok": true,
                "result": [{
                    "update_id": 7001,
                    "callback_query": {
                        "id": "cb-allow-after-revoke",
                        "from": { "id": CHAT_ID },
                        "message": { "chat": { "id": CHAT_ID } },
                        "data": format!("a:{id}"),
                    }
                }]
            }),
        );
        // The tap reached the gateway AND resolved the approval as an
        // Allow (a bare "✅" answer; a call already denied by the periodic
        // check would get "too late"), so the Allow-time path is what
        // denied it.
        let ack = wait_for_request(mock, "answerCallbackQuery", Duration::from_secs(10));
        assert_eq!(ack.body["text"], "\u{2705}", "{}", ack.body);
        let _ = h;
    });
}

// ── #430 ──────────────────────────────────────────────────────────────────

/// SIGTERM with a Telegram prompt pending: the "Gateway stopped" edit — on
/// a Bot API that takes 1.5 s to answer it — completes BEFORE the process
/// exits (the runtime teardown alone allows only 1 s), every time.
#[cfg(unix)]
#[test]
fn shutdown_lets_a_slow_telegram_edit_land_before_exit() {
    const RUNS: usize = 20;
    for run in 0..RUNS {
        let (mock_state, mock) = mock_with_send(4300 + run as i64);
        mock_state.set_delay("editMessageText", Duration::from_millis(1500));
        let mut h = spawn_and_authenticate(&mock.base, "shutdown-bot-token");
        let call = start_capture(&h, &h.access_token, "Shutdown Edit");
        wait_for_request(&mock_state, "sendMessage", Duration::from_secs(10));

        // SAFETY: plain kill(2) on our own child's pid.
        let rc = unsafe { libc::kill(h.child.0.id() as libc::pid_t, libc::SIGTERM) };
        assert_eq!(rc, 0);
        let deadline = Instant::now() + Duration::from_secs(15);
        let exited_at = loop {
            if let Some(status) = h.child.0.try_wait().expect("poll gateway child") {
                assert_eq!(status.code(), Some(0), "run {run}: {status}");
                break Instant::now();
            }
            assert!(Instant::now() < deadline, "run {run}: no exit within 15 s");
            std::thread::sleep(Duration::from_millis(5));
        };

        let edit = mock_state.find("editMessageText").unwrap_or_else(|| {
            panic!("run {run}/{RUNS}: the outcome edit never completed before exit")
        });
        assert!(
            edit.done_at <= exited_at,
            "run {run}/{RUNS}: the gateway exited {:?} before the edit completed",
            edit.done_at - exited_at
        );
        let text = edit.body["text"].as_str().unwrap_or_default();
        assert!(
            text.starts_with("\u{23f9} Gateway stopped"),
            "run {run}: {text}"
        );
        assert!(error_message(call.join().unwrap()).contains("shutting down"));
        assert_eq!(inbox_note_count(h.vault.path()), 0);
    }
}
