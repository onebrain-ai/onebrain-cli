//! Gateway Telegram approval capstone (Gateway PR 5, Task 7) — the
//! end-to-end proof that the Telegram channel Tasks 1-6 built in isolation
//! (`telegram_api.rs`'s `BotApi`, `telegram.rs`'s `TelegramChannel::{fire,
//! note_outcome, ensure_polling}` and its demand-driven `getUpdates` poller,
//! `approval.rs`'s `ResolvedVia::Telegram`, and `server.rs`'s
//! `await_approval` wiring) actually composes correctly against a REAL
//! spawned `onebrain gateway run` process, driven entirely over real HTTP —
//! exactly like a real MCP client and a real Telegram user tapping a real
//! inline button would, except the "real Telegram" on the other end is a
//! mock Bot API server this test itself hosts on loopback.
//!
//! Model: `tests/gateway_approval_e2e.rs`'s sandboxing (`KillOnDrop`,
//! tempdir `HOME`/cache so `~/.onebrain` never touches the real machine,
//! stdout/stderr captured to files this test polls, since the gateway
//! blocks on Ctrl-C after a successful bind) and its SEP-2243 MCP header set
//! (`MCP-Protocol-Version`/`Mcp-Method`/`Mcp-Name`). Every OAuth/MCP helper
//! below is COPIED, not imported, from `gateway_approval_e2e.rs` — test
//! files in this crate cannot share non-`support` helpers (see that file's
//! own module doc comment) and this crate ships no library target for any
//! of them to depend on. The OAuth leg is trimmed the SAME way
//! `gateway_approval_e2e.rs` trims it relative to `gateway_oauth_e2e.rs`'s
//! own capstone: this test discovers the protected-resource-metadata
//! document directly off the gateway's own origin (RFC 9728 guarantees it
//! lives at that fixed path either way) and skips refresh rotation
//! entirely — that is proven elsewhere, and this file's own complexity
//! budget goes to the Telegram wire format instead.
//!
//! **This file does NOT drive the operator `/approvals` HTTP surface at
//! all** — that is deliberate, not an oversight. Gateway PR 4's own
//! capstone (`gateway_approval_e2e.rs`) already proves `/approvals`
//! end-to-end; this file's whole point is the OTHER resolution path: a
//! human answering entirely inside Telegram, with the pending approval's id
//! recovered from the wire (the `callback_data` embedded in the inline
//! keyboard `sendMessage` sent) rather than from `GET /approvals`'s JSON.
//!
//! ## The mock Bot API server
//!
//! [`MockServer`]/[`MockState`] mirror `telegram.rs`'s and
//! `telegram_api.rs`'s own `#[cfg(test)]` mock-server fixtures (same
//! rationale: `BotApi` is a real blocking `ureq` client, so its tests need
//! an actual bound loopback socket, not `tower::ServiceExt::oneshot`'s
//! in-process router driving) — duplicated a third time rather than shared,
//! per this crate's established "one private copy per module/file that
//! needs one" convention. **A REAL, though entirely local, HTTP server**:
//! this satisfies the "no test may reach the real network" constraint
//! because the spawned gateway's `ONEBRAIN_TELEGRAM_API_BASE` points at
//! `http://127.0.0.1:<this test's own bound port>`, never
//! `https://api.telegram.org` — no packet leaves the loopback interface at
//! any point in either test below.
//!
//! ## BINDING REQUIREMENTS carried from `gateway_approval_e2e.rs`
//!
//! Both apply here for the identical reasons that file's own module doc
//! comment gives in full (not restated here): [`spawn_gateway`] sets
//! `ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL=1` so a macOS CI runner never
//! pops a real, unattended `osascript` dialog racing the Telegram channel
//! to resolve first, and `ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX=1` so a
//! successful capture's best-effort reindex never spawns (and leaks) a real
//! `onebrain daemon __run` process. The literals MUST stay in sync with
//! `approval_native::DISABLE_NATIVE_APPROVAL_ENV` and
//! `server::DISABLE_DAEMON_REINDEX_ENV` — this test binary has no library
//! target to import either constant from. A third literal is new here:
//! `ONEBRAIN_TELEGRAM_API_BASE`, which MUST stay in sync with
//! `telegram::TELEGRAM_API_BASE_ENV`.

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
            "clientInfo": {"name": "gateway-telegram-e2e-test", "version": "0.0.0"},
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

// ── New for this test: a mock Telegram Bot API server ─────────────────────

use axum::extract::{Path as AxumPath, State};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

/// Shared state for the mock Bot API server: a static per-method response
/// (set once, before the gateway is spawned — e.g. `sendMessage`'s
/// `message_id`), a FIFO of one-shot per-method responses this test queues
/// mid-flow (the poller's `getUpdates` needs to answer EMPTY for a while,
/// then answer with a scripted callback once this test knows the pending
/// approval's id), and every `(method, body)` request actually received —
/// same three-field shape as `telegram.rs`'s own `MockState` fixture, minus
/// the delay/in-flight bookkeeping that file's concurrency-specific tests
/// need and this one doesn't.
#[derive(Clone, Default)]
struct MockState {
    responses: Arc<Mutex<HashMap<String, Value>>>,
    queued: Arc<Mutex<HashMap<String, VecDeque<Value>>>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

impl MockState {
    fn set_response(&self, method: &str, body: Value) {
        self.responses
            .lock()
            .unwrap()
            .insert(method.to_string(), body);
    }

    fn queue_response(&self, method: &str, body: Value) {
        self.queued
            .lock()
            .unwrap()
            .entry(method.to_string())
            .or_default()
            .push_back(body);
    }

    fn requests(&self) -> Vec<(String, Value)> {
        self.requests.lock().unwrap().clone()
    }
}

/// Handles every `POST /{bot_and_token}/{method}` the spawned gateway's
/// `BotApi` sends: records `(method, body)`, then answers with the next
/// queued one-shot response for that method if any, else the static
/// scripted response, else a bare `{"ok":true,"result":null}` — matches
/// `BotApi::get_updates`'s own documented fallback (a `null`/missing
/// `result` collapses to an empty update batch), so an unscripted
/// `getUpdates` call is always a harmless, ordinary "nothing happened this
/// cycle" rather than an error.
async fn mock_handler(
    AxumPath(params): AxumPath<HashMap<String, String>>,
    State(state): State<MockState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let method = params.get("method").cloned().unwrap_or_default();
    state.requests.lock().unwrap().push((method.clone(), body));

    let queued = state
        .queued
        .lock()
        .unwrap()
        .get_mut(&method)
        .and_then(|q| q.pop_front());
    let resp = match queued {
        Some(v) => v,
        None => state
            .responses
            .lock()
            .unwrap()
            .get(&method)
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "ok": true, "result": null })),
    };
    Json(resp)
}

/// A mock Telegram Bot API server bound to an ephemeral `127.0.0.1` port —
/// see this file's module docs, "The mock Bot API server", for why this has
/// to be a real bound socket rather than an in-process router. Same
/// thread-plus-current-thread-runtime shape as `telegram.rs`'s and
/// `telegram_api.rs`'s own copies (themselves mirroring
/// `daemon_client.rs`'s live-server harness).
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
            let rt = tokio::runtime::Builder::new_current_thread()
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
        // Bounded, same reasoning as `telegram_api.rs`'s own copy: an
        // unbounded wait here means a sandbox that refuses loopback binds
        // hangs the whole test binary instead of failing one test.
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

/// Bounded poll for the FIRST `(method, body)` request the mock recorded —
/// `sendMessage` fires exactly once per approval (`TelegramChannel::fire`)
/// and `editMessageText` at most once (`TelegramChannel::note_outcome`
/// removes its `sent` entry before editing), so "first" is unambiguous for
/// both call sites this test uses it from.
fn wait_for_request(state: &MockState, method: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some((_, body)) = state.requests().into_iter().find(|(m, _)| m == method) {
            return body;
        }
        assert!(
            Instant::now() < deadline,
            "no {method} request arrived at the mock Bot API server within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ── Fixture + shared setup ─────────────────────────────────────────────────

/// The Telegram chat id this fixture configures — a private (positive, per
/// Telegram's own id convention — see `telegram::is_available`'s own doc
/// comment) chat id, arbitrary and fixed across both tests below since each
/// test gets its OWN sandboxed `$HOME`/mock server pair.
const CHAT_ID: i64 = 918_273_645;

/// Marker string planted in `brain_capture`'s `text` argument — the raw
/// note BODY. `args_summary` (and so the Telegram prompt, which embeds it
/// verbatim) only ever carries `text_chars=<count>`, never the body itself
/// (see `server.rs`'s own `brain_capture` handler doc comment) — this
/// constant is what both tests below assert is ABSENT from the message the
/// mock Bot API server received.
const BODY_MARKER: &str = "raw-telegram-e2e-body-must-never-reach-telegram";

/// Set up a minimal fixture vault and a machine-level
/// `~/.onebrain/gateway.yml` naming it as the default vault, with
/// `policy.mutating: ask_once` (the one config knob this test needs to
/// drive `brain_capture` through approval) and a `telegram:` block wired to
/// THIS test's own mock server via `bot_token`/`chat_id` — the same shape
/// `telegram_setup`'s wizard would have written, but constructed directly
/// here since driving the interactive wizard itself is out of this test's
/// scope (Task 6 already covers it).
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

/// Every live handle a test needs, held for its whole body so nothing is
/// dropped (and so its tempdir deleted, or its process killed) out from
/// under the still-running gateway. `_cache`/`_cwd` are read only by name —
/// never their contents — so the leading underscore documents "kept alive
/// on purpose, not otherwise used" rather than signalling dead code; see
/// the standard Rust dead-code convention this crate already relies on
/// elsewhere for exactly this shape.
struct Harness {
    child: KillOnDrop,
    mcp_url: String,
    access_token: String,
    home: tempfile::TempDir,
    vault: tempfile::TempDir,
    _cache: tempfile::TempDir,
    _cwd: tempfile::TempDir,
}

/// Spawns the sandboxed gateway (Telegram wired to `mock_base`), completes
/// the full OAuth discovery → register → authorize → token dance using the
/// sandbox's own pairing code, and sends `initialize` — everything a real
/// MCP client needs before its first `tools/call`. Identical in substance
/// to `gateway_approval_e2e.rs`'s own capstone test body up through that
/// point; factored into one function here (rather than inlined twice, that
/// file's own shape) purely because THIS file has two near-identical tests
/// — the approve and deny paths — that otherwise would have duplicated the
/// entire OAuth leg a second time for no benefit, since neither test varies
/// it.
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
        .unwrap_or_else(|| panic!("mcp url had no /mcp suffix: {mcp_url}"))
        .to_string();
    let prm_url = format!("{origin}/.well-known/oauth-protected-resource");
    let (status, prm_body) = get(&agent, &prm_url);
    assert_eq!(status, 200, "{prm_body}");
    let prm: serde_json::Value = serde_json::from_str(&prm_body)
        .unwrap_or_else(|e| panic!("PRM was not JSON ({e}): {prm_body}"));
    let as_issuer = prm["authorization_servers"][0]
        .as_str()
        .unwrap_or_else(|| panic!("PRM had no authorization_servers[0]: {prm_body}"))
        .to_string();

    let as_metadata_url = format!("{as_issuer}/.well-known/oauth-authorization-server");
    let (status, as_body) = get(&agent, &as_metadata_url);
    assert_eq!(status, 200, "{as_body}");
    let as_meta: serde_json::Value = serde_json::from_str(&as_body)
        .unwrap_or_else(|e| panic!("AS metadata was not JSON ({e}): {as_body}"));
    let authorize_url = as_meta["authorization_endpoint"]
        .as_str()
        .unwrap_or_else(|| panic!("AS metadata had no authorization_endpoint: {as_body}"))
        .to_string();
    let token_url = as_meta["token_endpoint"]
        .as_str()
        .unwrap_or_else(|| panic!("AS metadata had no token_endpoint: {as_body}"))
        .to_string();
    let register_url = as_meta["registration_endpoint"]
        .as_str()
        .unwrap_or_else(|| panic!("AS metadata had no registration_endpoint: {as_body}"))
        .to_string();

    const REDIRECT_URI: &str = "http://127.0.0.1/callback";
    let register_body = serde_json::json!({
        "client_name": "gateway-telegram-e2e-test-client",
        "redirect_uris": [REDIRECT_URI],
        "application_type": "native",
    });
    let (status, register_resp_body) = post_json(&agent, &register_url, &register_body);
    assert_eq!(status, 201, "{register_resp_body}");
    let registered: serde_json::Value = serde_json::from_str(&register_resp_body)
        .unwrap_or_else(|e| panic!("register response was not JSON ({e}): {register_resp_body}"));
    let client_id = registered["client_id"]
        .as_str()
        .unwrap_or_else(|| panic!("register response had no client_id: {register_resp_body}"))
        .to_string();

    let (verifier, challenge) = pkce_pair();
    const STATE: &str = "gateway-telegram-e2e-state";
    let authorize_params = [
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", REDIRECT_URI),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", STATE),
    ];
    let real_pairing_code = read_pairing_code(home.path());
    let mut right_pairing_params = authorize_params.to_vec();
    right_pairing_params.push(("pairing_code", real_pairing_code.as_str()));
    let (status, right_body, right_location) =
        post_authorize(&agent, &authorize_url, &right_pairing_params);
    assert_eq!(
        status, 302,
        "the right pairing code must redirect: {right_body}"
    );
    let location = right_location.unwrap_or_else(|| panic!("302 with no Location header"));
    let query_start = location
        .find('?')
        .unwrap_or_else(|| panic!("redirect Location had no query string"));
    let params = parse_query(&location[query_start..]);
    let auth_code = params
        .get("code")
        .unwrap_or_else(|| {
            panic!(
                "redirect Location had no code param (query keys present: {:?})",
                params.keys().collect::<Vec<_>>()
            )
        })
        .clone();
    assert_eq!(params.get("state").map(String::as_str), Some(STATE));

    let (status, token_body) = post_token(
        &agent,
        &token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", auth_code.as_str()),
            ("client_id", client_id.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", verifier.as_str()),
        ],
    );
    assert_eq!(status, 200, "{token_body}");
    let tokens: serde_json::Value = serde_json::from_str(&token_body)
        .unwrap_or_else(|e| panic!("token response was not JSON ({e}): {token_body}"));
    let response_keys = |v: &serde_json::Value| -> Vec<String> {
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    // Security: `access_token` is live credential material — never
    // interpolate it whole into a panic/assert message from here on.
    let access_token = tokens["access_token"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "no access_token in token response (keys: {:?})",
                response_keys(&tokens)
            )
        })
        .to_string();

    let (status, init_resp_body) = post_mcp(
        &agent,
        &mcp_url,
        &access_token,
        &init_body(1),
        &[("MCP-Protocol-Version", PROTOCOL)],
    );
    assert_eq!(status, 200, "{init_resp_body}");
    let init_resp: serde_json::Value = serde_json::from_str(&init_resp_body)
        .unwrap_or_else(|e| panic!("initialize response was not JSON ({e}): {init_resp_body}"));
    assert_eq!(
        init_resp["result"]["protocolVersion"], PROTOCOL,
        "{init_resp}"
    );

    Harness {
        child,
        mcp_url,
        access_token,
        home,
        vault,
        _cache: cache,
        _cwd: cwd,
    }
}

/// The `sendMessage` request's inline-keyboard `callback_data` for both
/// buttons, and the approval id embedded in each (`"a:<id>"`/`"d:<id>"`,
/// `TelegramChannel::fire`'s own wire shape, `commands/gateway/telegram.rs`
/// — restated here since this test binary has no library target to import
/// it from, and left as plain text rather than an intra-doc link: this
/// integration-test binary has no `super::telegram` module for
/// `[...]`-style linking to ever resolve against).
struct Buttons {
    approve_data: String,
    deny_data: String,
}

/// Asserts the `sendMessage` body's shape (names the tool and carries the
/// bounded `args_summary`, exactly two buttons, callback_data carrying a
/// shared approval id) and that the raw capture BODY never reached it, then
/// returns the parsed buttons. Does NOT check the `Client: <id>` line —
/// that framing is pinned by `telegram.rs`'s own
/// `fire_sends_one_bounded_message_with_approve_and_deny_buttons` unit
/// test, not by this e2e.
fn assert_prompt_and_extract_buttons(send_body: &Value) -> Buttons {
    assert_eq!(send_body["chat_id"], CHAT_ID, "{send_body}");
    let text = send_body["text"]
        .as_str()
        .unwrap_or_else(|| panic!("sendMessage had no text field: {send_body}"));
    assert!(
        text.contains("Tool: brain_capture"),
        "the prompt must name the tool being approved: {text}"
    );
    assert!(
        text.contains("capture: title="),
        "the prompt must carry the bounded args_summary, not something else: {text}"
    );
    assert!(
        !text.contains(BODY_MARKER),
        "the raw capture body must never reach telegram: {text}"
    );

    let row = send_body["reply_markup"]["inline_keyboard"][0]
        .as_array()
        .unwrap_or_else(|| panic!("sendMessage had no inline_keyboard row: {send_body}"));
    assert_eq!(row.len(), 2, "{send_body}");
    assert_eq!(row[0]["text"], "✅ Approve", "{send_body}");
    assert_eq!(row[1]["text"], "⛔ Deny", "{send_body}");
    let approve_data = row[0]["callback_data"]
        .as_str()
        .unwrap_or_else(|| panic!("Approve button had no callback_data: {send_body}"))
        .to_string();
    let deny_data = row[1]["callback_data"]
        .as_str()
        .unwrap_or_else(|| panic!("Deny button had no callback_data: {send_body}"))
        .to_string();
    let id = approve_data
        .strip_prefix("a:")
        .unwrap_or_else(|| panic!("Approve callback_data had no \"a:\" prefix: {approve_data}"))
        .to_string();
    assert_eq!(
        deny_data,
        format!("d:{id}"),
        "both buttons must carry the SAME approval id: {send_body}"
    );

    Buttons {
        approve_data,
        deny_data,
    }
}

/// A scripted `getUpdates` response carrying exactly one `callback_query`
/// update — `data` is `"a:<id>"` or `"d:<id>"` per [`Buttons`], `from`/
/// `message.chat` both report [`CHAT_ID`] (the same private chat this
/// fixture configured — `handle_update`'s whole authorization check is
/// `from_id == chat_id`).
fn callback_update(update_id: i64, callback_id: &str, data: &str) -> Value {
    serde_json::json!({
        "ok": true,
        "result": [{
            "update_id": update_id,
            "callback_query": {
                "id": callback_id,
                "from": { "id": CHAT_ID },
                "message": { "chat": { "id": CHAT_ID } },
                "data": data,
            }
        }]
    })
}

// ── The two capstone tests ─────────────────────────────────────────────────

/// A `brain_capture` call under `ask_once` blocks, `TelegramChannel::fire`
/// sends a real (mock) Telegram prompt with Approve/Deny buttons carrying
/// no raw note body, this test taps "Approve" by scripting the poller's
/// NEXT `getUpdates` response with the matching `"a:<id>"` callback, the
/// call completes and the note lands on disk, the audit line names
/// `"channel":"telegram"`, and the mock server saw the outcome edit
/// (`editMessageText`, keyboard cleared) closing the loop.
#[test]
fn a_capture_is_approved_from_telegram_end_to_end() {
    let mock_state = MockState::default();
    mock_state.set_response(
        "sendMessage",
        serde_json::json!({ "ok": true, "result": { "message_id": 9001 } }),
    );
    let mock = MockServer::start(mock_state.clone());

    let mut harness = spawn_and_authenticate(&mock.base, "e2e-approve-bot-token");

    let mcp_url_bg = harness.mcp_url.clone();
    let token_bg = harness.access_token.clone();
    let call_handle = std::thread::spawn(move || {
        let agent = http_agent();
        post_mcp(
            &agent,
            &mcp_url_bg,
            &token_bg,
            &call_body(
                2,
                "brain_capture",
                serde_json::json!({
                    "title": "Telegram E2E Approve Note",
                    "text": format!("{BODY_MARKER} — approved path"),
                }),
            ),
            &standard_headers("tools/call", Some("brain_capture")),
        )
    });

    // ── The prompt reaches the mock Bot API server, with no raw body ────
    let send_body = wait_for_request(&mock_state, "sendMessage", Duration::from_secs(10));
    assert!(
        !call_handle.is_finished(),
        "brain_capture must still be blocked on the pending approval, not yet returned"
    );
    assert_eq!(
        inbox_note_count(harness.vault.path()),
        0,
        "no note may exist before the approval is resolved"
    );
    let buttons = assert_prompt_and_extract_buttons(&send_body);

    // ── Tap "Approve": script the poller's next getUpdates response ─────
    mock_state.queue_response(
        "getUpdates",
        callback_update(9001, "cb-approve", &buttons.approve_data),
    );

    let (status1, call1_body) = call_handle.join().expect("brain_capture thread panicked");
    assert_eq!(status1, 200, "{call1_body}");
    let call1: serde_json::Value = serde_json::from_str(&call1_body)
        .unwrap_or_else(|e| panic!("brain_capture response was not JSON ({e}): {call1_body}"));
    assert!(call1.get("error").is_none(), "{call1}");
    let path1 = call1["result"]["structuredContent"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("no structuredContent.path: {call1}"))
        .to_string();
    assert!(path1.starts_with("00-inbox/"), "{path1}");

    // ── The note really exists on disk, with the right content ──────────
    let content1 = std::fs::read_to_string(harness.vault.path().join(&path1))
        .unwrap_or_else(|e| panic!("read captured note {path1}: {e}"));
    assert!(content1.contains("tags: [capture]"), "{content1}");
    assert!(content1.contains(BODY_MARKER), "{content1}");

    // ── The mock server saw the outcome edit, keyboard cleared ──────────
    let edit_body = wait_for_request(&mock_state, "editMessageText", Duration::from_secs(20));
    assert_eq!(edit_body["chat_id"], CHAT_ID, "{edit_body}");
    assert_eq!(edit_body["message_id"], 9001, "{edit_body}");
    let edit_text = edit_body["text"]
        .as_str()
        .unwrap_or_else(|| panic!("editMessageText had no text field: {edit_body}"));
    assert!(
        edit_text.starts_with("✅ Approved via telegram"),
        "{edit_text}"
    );
    assert_eq!(
        edit_body["reply_markup"]["inline_keyboard"],
        serde_json::json!([]),
        "the outcome edit must clear the inline keyboard, not merely omit it: {edit_body}"
    );

    // ── No stray daemon: `ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX` took
    // effect — same assertion `gateway_approval_e2e.rs` makes, see that
    // file's own module docs for the full rationale.
    assert!(
        !harness.home.path().join(".onebrain").join("run").exists(),
        "the gateway must not have spawned or probed a daemon: \
         ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX did not take effect"
    );

    // ── The audit log names the Telegram channel ─────────────────────────
    let entries = read_audit_entries(harness.home.path());
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["tool"], "brain_capture", "{entries:?}");
    assert_eq!(entries[0]["decision"], "approved", "{entries:?}");
    assert_eq!(entries[0]["channel"], "telegram", "{entries:?}");
    assert_eq!(entries[0]["outcome"], "ok", "{entries:?}");
    let summary = entries[0]["args_summary"].as_str().unwrap_or_default();
    assert!(
        !summary.contains(BODY_MARKER),
        "the audit trail must never carry the raw note body: {entries:?}"
    );

    assert!(
        harness
            .child
            .0
            .try_wait()
            .expect("poll gateway child")
            .is_none(),
        "server must stay up through the entire flow"
    );
    assert_exits_after_kill(&mut harness.child.0);
}

/// The denial mirror of the test above: tapping "Deny" (a `"d:<id>"`
/// callback) makes `brain_capture` fail with a policy error, writes no
/// note, and the audit line records `"decision":"denied"`,
/// `"channel":"telegram"`, `"outcome":"error"` — same channel-naming
/// property, opposite outcome.
#[test]
fn d_capture_is_denied_from_telegram_end_to_end() {
    let mock_state = MockState::default();
    mock_state.set_response(
        "sendMessage",
        serde_json::json!({ "ok": true, "result": { "message_id": 9002 } }),
    );
    let mock = MockServer::start(mock_state.clone());

    let mut harness = spawn_and_authenticate(&mock.base, "e2e-deny-bot-token");

    let mcp_url_bg = harness.mcp_url.clone();
    let token_bg = harness.access_token.clone();
    let call_handle = std::thread::spawn(move || {
        let agent = http_agent();
        post_mcp(
            &agent,
            &mcp_url_bg,
            &token_bg,
            &call_body(
                2,
                "brain_capture",
                serde_json::json!({
                    "title": "Telegram E2E Deny Note",
                    "text": format!("{BODY_MARKER} — denied path"),
                }),
            ),
            &standard_headers("tools/call", Some("brain_capture")),
        )
    });

    let send_body = wait_for_request(&mock_state, "sendMessage", Duration::from_secs(10));
    assert!(
        !call_handle.is_finished(),
        "brain_capture must still be blocked on the pending approval, not yet returned"
    );
    let buttons = assert_prompt_and_extract_buttons(&send_body);

    // ── Tap "Deny" ────────────────────────────────────────────────────
    mock_state.queue_response(
        "getUpdates",
        callback_update(9101, "cb-deny", &buttons.deny_data),
    );

    let (status1, call1_body) = call_handle.join().expect("brain_capture thread panicked");
    assert_eq!(status1, 200, "{call1_body}");
    let call1: serde_json::Value = serde_json::from_str(&call1_body)
        .unwrap_or_else(|e| panic!("brain_capture response was not JSON ({e}): {call1_body}"));
    let message = call1["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a JSON-RPC error for a denied call: {call1}"));
    assert!(message.contains("denied"), "{message}");

    assert_eq!(
        inbox_note_count(harness.vault.path()),
        0,
        "a denied capture must never create a file"
    );

    let edit_body = wait_for_request(&mock_state, "editMessageText", Duration::from_secs(20));
    assert_eq!(edit_body["chat_id"], CHAT_ID, "{edit_body}");
    assert_eq!(edit_body["message_id"], 9002, "{edit_body}");
    let edit_text = edit_body["text"]
        .as_str()
        .unwrap_or_else(|| panic!("editMessageText had no text field: {edit_body}"));
    assert!(
        edit_text.starts_with("⛔ Denied via telegram"),
        "{edit_text}"
    );
    assert_eq!(
        edit_body["reply_markup"]["inline_keyboard"],
        serde_json::json!([]),
        "{edit_body}"
    );

    let entries = read_audit_entries(harness.home.path());
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["tool"], "brain_capture", "{entries:?}");
    assert_eq!(entries[0]["decision"], "denied", "{entries:?}");
    assert_eq!(entries[0]["channel"], "telegram", "{entries:?}");
    assert_eq!(entries[0]["outcome"], "error", "{entries:?}");

    assert!(
        harness
            .child
            .0
            .try_wait()
            .expect("poll gateway child")
            .is_none(),
        "server must stay up through the entire flow"
    );
    assert_exits_after_kill(&mut harness.child.0);
}
