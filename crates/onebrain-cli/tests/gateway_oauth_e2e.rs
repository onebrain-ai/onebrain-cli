//! Gateway OAuth capstone — the full end-to-end proof that a real client
//! (no prior knowledge, no shortcuts) can go from "no token" to "authorized
//! MCP calls" purely by following RFC 9728/8414 discovery, RFC 7591 dynamic
//! client registration, the `/authorize` consent flow (gated by the
//! device-pairing code), and `/token` code exchange + refresh rotation —
//! then verifies the two hardening properties (reuse detection burns the
//! whole token family; the pairing code is never echoed back over HTTP).
//!
//! Model: `tests/gateway_http.rs`'s sandboxing (`KillOnDrop`, tempdir
//! HOME/cache so `~/.onebrain` never touches the real machine, stdout
//! captured to a file this test polls since the gateway blocks on Ctrl-C
//! after a successful bind) and its SEP-2243 MCP header set
//! (`MCP-Protocol-Version`/`Mcp-Method`/`Mcp-Name`). Every helper below is
//! COPIED, not imported, from `gateway_http.rs` — test files in this crate
//! cannot share non-`support` helpers (see that file's own module doc
//! comment), and this crate ships no library target for either file to
//! depend on.
//!
//! Unlike `gateway_http.rs` (which plants a `TokenRecord` directly into
//! `tokens.json` as a stand-in for the OAuth flow that didn't exist yet),
//! this test drives the REAL flow over `ureq` end to end: `/register` →
//! `/authorize` (GET renders the consent form, POST gated by the pairing
//! code) → `/token`. The one deliberate shortcut is reading the CURRENT
//! pairing code back out of the sandboxed `$HOME/.onebrain/gateway/pairing.json`
//! file rather than scraping it from a terminal a human would be looking
//! at — this test IS the machine's owner in the scenario it simulates (the
//! same person who ran `gateway run` and is now pairing a client to it), so
//! reading the code the gateway itself just minted/persisted is the sandboxed
//! equivalent of "the user types the code they see on their own screen."
//!
//! The client under test registers itself as `application_type: "native"`
//! with a bare loopback `redirect_uri` (`http://127.0.0.1/callback`, no
//! port) — the only redirect shape this test CAN exercise without standing
//! up a real HTTPS listener: a `"web"` client's `redirect_uri` must be
//! `https://`, which this test has no way to serve, and the redirect target
//! is never actually fetched either way (the flow only needs the `code`/
//! `state`/`iss` query parameters off the `302`'s `Location` header, which
//! this test parses directly rather than following the redirect).

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tempfile::tempdir;

mod support;

// ── Copied from `gateway_http.rs` (see module docs: not shared) ────────────

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
/// http://<bound-addr>/mcp`), returning the parsed `/mcp` URL.
///
/// On a startup failure this panics with a REDACTED tail of the gateway's
/// stderr, never with stdout in any form and never with either stream raw —
/// see `gateway_http.rs::wait_for_gateway_url` for the full reasoning (the
/// capture files are deleted during this panic's own unwind, so a byte count
/// would leave no diagnostic anywhere; stdout carries the pairing code;
/// stderr carries host paths). All three gateway harnesses share one
/// redactor, [`support::redacted_capture_tail`], and
/// `gateway_http.rs::gateway_startup_failure_panic_carries_a_redacted_stderr_tail`
/// pins it against a real constructed failure.
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

/// A `ureq` agent that never turns a non-2xx status into an `Err` (this test
/// inspects `4xx`/`302` responses directly) AND never follows a redirect
/// (`max_redirects(0)`) — the `POST /authorize` success response is a `302`
/// this test parses `code`/`state`/`iss` off directly; the `redirect_uri` it
/// points at (`http://127.0.0.1/callback`) is never actually hosted, so
/// letting `ureq` follow it would just fail the request instead of handing
/// back the response this test needs.
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
            "clientInfo": {"name": "gateway-oauth-e2e-test", "version": "0.0.0"},
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
/// `commands/gateway/server.rs::standard_headers`.
fn standard_headers<'a>(method: &'a str, name: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut headers = vec![("MCP-Protocol-Version", PROTOCOL), ("Mcp-Method", method)];
    if let Some(name) = name {
        headers.push(("Mcp-Name", name));
    }
    headers
}

/// POST one JSON-RPC `body` to `/mcp` with `token` as the `Authorization:
/// Bearer` credential, plus `extra` headers, returning the raw response text
/// (both for JSON parsing and for the pairing-code-leak sweep below) and the
/// HTTP status.
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

/// `/mcp` with NO `Authorization` header at all — step 1 of the flow.
/// Returns `(status, body_text, www_authenticate_header)`.
fn post_mcp_unauthenticated(
    agent: &ureq::Agent,
    url: &str,
    body: &serde_json::Value,
) -> (u16, String, Option<String>) {
    let mut resp = agent
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", PROTOCOL)
        .send(body.to_string())
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));
    let status = resp.status().as_u16();
    let www = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let text = resp
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|e| panic!("read response body from {url}: {e}"));
    (status, text, www)
}

// ── New for this test: PKCE, form/query encoding, discovery parsing ───────

/// Base64url (RFC 4648 §5), unpadded — same algorithm as
/// `commands/gateway/auth/core.rs::base64url_nopad`, copied (not shared: no
/// library target — see module docs) so this test can build its own PKCE
/// pair and decode the auth-code redirect without touching crate internals.
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

/// A real RFC 7636 S256 PKCE pair: 32 random bytes (43-char base64url
/// verifier, well within the 43-128 range) and its SHA-256 challenge.
fn pkce_pair() -> (String, String) {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS CSPRNG unavailable for test PKCE verifier");
    let verifier = base64url_nopad(&buf);
    let challenge = base64url_nopad(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// RFC 3986 §2.3 unreserved-only percent-encoder — used for BOTH the
/// `application/x-www-form-urlencoded` POST bodies this test sends and (via
/// [`percent_decode`]) for reading the query string back off the
/// `/authorize` redirect's `Location` header. Every value this test encodes
/// is either base64url (already unreserved-only) or a fixed opaque test
/// string, so a byte-simple implementation — no `+`-for-space handling — is
/// sufficient.
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

/// Parse `?a=b&c=d` (or a bare `a=b&c=d`, no leading `?`) into a map,
/// percent-decoding every value — used to read `code`/`state`/`iss` off the
/// `/authorize` redirect's `Location` header query string.
fn parse_query(qs: &str) -> HashMap<String, String> {
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

/// GET `/authorize?<pairs>` (no auth — the consent page itself IS the
/// bootstrap surface), returning `(status, body_text)`.
fn get_authorize(agent: &ureq::Agent, base: &str, pairs: &[(&str, &str)]) -> (u16, String) {
    let url = format!("{base}?{}", form_encode(pairs));
    get(agent, &url)
}

/// POST `/authorize` form-urlencoded, returning `(status, body_text,
/// location_header)` — `location_header` is `Some` only on the `302` success
/// path (this agent never follows it, per [`http_agent`]'s doc comment).
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

/// Extract the `resource_metadata="..."` URL out of an RFC 9728
/// `WWW-Authenticate` challenge header value.
fn extract_resource_metadata_url(www_authenticate: &str) -> String {
    const MARKER: &str = "resource_metadata=\"";
    let start = www_authenticate
        .find(MARKER)
        .unwrap_or_else(|| panic!("no resource_metadata in WWW-Authenticate: {www_authenticate}"))
        + MARKER.len();
    let rest = &www_authenticate[start..];
    let end = rest
        .find('"')
        .unwrap_or_else(|| panic!("unterminated resource_metadata quote: {www_authenticate}"));
    rest[..end].to_string()
}

/// Read the CURRENT device-pairing code straight out of the sandboxed
/// `$HOME/.onebrain/gateway/pairing.json` — mirrors `AuthStore::pairing_code`'s
/// on-disk shape (`{"code": "...", "created": ...}`, see
/// `commands/gateway/auth/store.rs::PairingState`). See the module doc
/// comment for why this is a legitimate shortcut, not a real one: this test
/// IS the machine's owner in the scenario it simulates.
///
/// **Nothing derived from the file's CONTENTS reaches a panic message, and
/// neither does its path.** `raw` here IS the pairing code: a partially
/// written `pairing.json` leaves `{"code":"ABCD-EFGH"` on disk, `serde_json`
/// rejects it, and interpolating `raw` would put a live credential in the CI
/// log. What is reported instead is the `io::Error`/`serde_json::Error`
/// itself (kind, and line/column for the parse) plus, for a shape mismatch,
/// the top-level KEY NAMES — enough to tell "no file" from "truncated file"
/// from "the on-disk shape changed", none of it secret.
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

/// Set up a minimal fixture vault (one dated task) and a machine-level
/// `~/.onebrain/gateway.yml` naming it as the default vault — same shape as
/// `gateway_http.rs`'s own happy-path fixture, trimmed to what step 6 needs
/// (`brain_tasks` only; this test never calls `brain_search`, so there is no
/// warm-daemon spawn to plan around).
fn write_fixture_vault_and_config(home: &Path) -> tempfile::TempDir {
    let vault = tempdir().unwrap();
    write(vault.path(), "onebrain.yml", "folders: {}\n");
    write(
        vault.path(),
        "01-projects/x.md",
        "- [ ] gateway oauth e2e fixture task 📅 2026-01-01\n",
    );
    write(
        home,
        ".onebrain/gateway.yml",
        &format!(
            "default_vault: {v}\nvaults:\n  t1: {v}\n",
            v = vault.path().display()
        ),
    );
    vault
}

/// The full OAuth 2.1 authorization-code + refresh-rotation capstone,
/// exercised entirely over real HTTP against a spawned `onebrain gateway
/// run` process, playing the role of a from-scratch MCP client ("Claude")
/// that starts with nothing but the `/mcp` URL.
#[test]
fn gateway_oauth_full_authorization_code_and_refresh_rotation_flow() {
    let home = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let cwd = tempdir().unwrap(); // neutral — outside any vault
    let _vault = write_fixture_vault_and_config(home.path());

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
    let agent = http_agent();

    // Every response body captured along the way — swept for the pairing
    // code at the very end (see the final assertion).
    let mut captured_bodies: Vec<(&'static str, String)> = Vec::new();

    // `gateway run` must have printed the pairing-code startup line — the
    // ONLY channel a pairing code is ever shown on (see `commands/gateway/
    // mod.rs`'s module docs). Checked now: `wait_for_gateway_url` already
    // proved the stdout file is being flushed promptly (`println!` uses a
    // `LineWriter` regardless of TTY status, so this line — printed BEFORE
    // the "gateway listening" line — is already on disk too).
    let stdout_so_far = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    // Security: `stdout_so_far` already contains the real, live pairing
    // code by design at this point (the very thing this assertion checks
    // for) — never interpolate it whole into the message (CodeQL
    // `rust/cleartext-logging`).
    assert!(
        stdout_so_far.contains("pairing code: "),
        "gateway run must print the pairing-code startup line ({} bytes of stdout captured)",
        stdout_so_far.len()
    );

    // ── Step 1: `/mcp` with no token → 401, parse `resource_metadata` ──────
    let (status, body, www) = post_mcp_unauthenticated(&agent, &mcp_url, &init_body(1));
    assert_eq!(status, 401, "unauthenticated /mcp must 401: {body}");
    assert!(body.is_empty(), "401 body must be empty: {body:?}");
    let www = www.unwrap_or_else(|| panic!("no WWW-Authenticate header on the 401"));
    let prm_url = extract_resource_metadata_url(&www);

    // ── Step 2: GET PRM → follow authorization_servers[0] → GET AS metadata ─
    let (status, prm_body) = get(&agent, &prm_url);
    assert_eq!(status, 200, "{prm_body}");
    captured_bodies.push(("protected resource metadata", prm_body.clone()));
    let prm: serde_json::Value = serde_json::from_str(&prm_body)
        .unwrap_or_else(|e| panic!("PRM was not JSON ({e}): {prm_body}"));
    assert_eq!(prm["resource"], serde_json::json!(mcp_url));
    let as_issuer = prm["authorization_servers"][0]
        .as_str()
        .unwrap_or_else(|| panic!("PRM had no authorization_servers[0]: {prm_body}"))
        .to_string();

    let as_metadata_url = format!("{as_issuer}/.well-known/oauth-authorization-server");
    let (status, as_body) = get(&agent, &as_metadata_url);
    assert_eq!(status, 200, "{as_body}");
    captured_bodies.push(("authorization server metadata", as_body.clone()));
    let as_meta: serde_json::Value = serde_json::from_str(&as_body)
        .unwrap_or_else(|e| panic!("AS metadata was not JSON ({e}): {as_body}"));
    assert_eq!(
        as_meta["code_challenge_methods_supported"],
        serde_json::json!(["S256"]),
        "AS metadata must advertise S256-only PKCE: {as_body}"
    );
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

    // ── Step 3: POST /register — a NATIVE client, loopback redirect_uri ────
    // (Web clients need `https://`, which this test has no way to host; a
    // native, loopback-registered client is the fully-testable path — the
    // redirect target is never actually fetched either way, see module docs.)
    const REDIRECT_URI: &str = "http://127.0.0.1/callback";
    let register_body = serde_json::json!({
        "client_name": "gateway-oauth-e2e-test-client",
        "redirect_uris": [REDIRECT_URI],
        "application_type": "native",
    });
    let (status, register_resp_body) = post_json(&agent, &register_url, &register_body);
    assert_eq!(status, 201, "{register_resp_body}");
    captured_bodies.push(("register response", register_resp_body.clone()));
    let registered: serde_json::Value = serde_json::from_str(&register_resp_body)
        .unwrap_or_else(|e| panic!("register response was not JSON ({e}): {register_resp_body}"));
    assert!(
        registered.get("client_secret").is_none(),
        "public-client-only AS must never emit a client_secret: {register_resp_body}"
    );
    let client_id = registered["client_id"]
        .as_str()
        .unwrap_or_else(|| panic!("register response had no client_id: {register_resp_body}"))
        .to_string();

    // ── Step 4: GET /authorize (real PKCE pair) → extract the form; POST a
    // WRONG pairing code → no code minted; POST the RIGHT code (read from
    // the sandbox store — see module docs) → capture code+iss ────────────
    let (verifier, challenge) = pkce_pair();
    const STATE: &str = "gateway-oauth-e2e-state";
    let authorize_params = [
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", REDIRECT_URI),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", STATE),
    ];

    let (status, form_body) = get_authorize(&agent, &authorize_url, &authorize_params);
    assert_eq!(status, 200, "{form_body}");
    captured_bodies.push(("consent form", form_body.clone()));
    // "Extract the form": the fake client confirms the rendered page really
    // does echo back what it sent (this is what a real client would scrape
    // hidden fields from before submitting the pairing code) rather than
    // blindly trusting its own request params.
    assert!(
        form_body.contains(r#"name="pairing_code""#),
        "consent form missing the pairing_code input: {form_body}"
    );
    assert!(
        form_body.contains(&format!(r#"value="{client_id}""#)),
        "consent form did not echo client_id: {form_body}"
    );
    assert!(
        form_body.contains(&format!(r#"value="{challenge}""#)),
        "consent form did not echo code_challenge: {form_body}"
    );
    assert!(
        form_body.contains(&format!(r#"value="{STATE}""#)),
        "consent form did not echo state: {form_body}"
    );

    let mut wrong_pairing_params = authorize_params.to_vec();
    wrong_pairing_params.push(("pairing_code", "WRONG-CODE"));
    let (status, wrong_body, wrong_location) =
        post_authorize(&agent, &authorize_url, &wrong_pairing_params);
    assert_eq!(
        status, 200,
        "a wrong pairing code must re-render the form, not redirect: {wrong_body}"
    );
    // Security: if this ever fails, `wrong_location` holds a Location
    // header carrying a live, illegitimately-minted authorization code —
    // never interpolate it into the message (CodeQL `rust/cleartext-logging`).
    assert!(
        wrong_location.is_none(),
        "a wrong pairing code must mint no code, but a Location header was present"
    );
    captured_bodies.push(("wrong-pairing-code re-render", wrong_body));

    let real_pairing_code = read_pairing_code(home.path());
    let mut right_pairing_params = authorize_params.to_vec();
    right_pairing_params.push(("pairing_code", real_pairing_code.as_str()));
    let (status, right_body, right_location) =
        post_authorize(&agent, &authorize_url, &right_pairing_params);
    assert_eq!(
        status, 302,
        "the right pairing code must redirect: {right_body}"
    );
    captured_bodies.push(("right-pairing-code redirect body", right_body));
    let location = right_location.unwrap_or_else(|| panic!("302 with no Location header"));
    captured_bodies.push(("right-pairing-code Location header", location.clone()));
    // Security: `location` embeds the just-minted, live authorization code
    // as its `code=` query parameter — none of the checks below may
    // interpolate it whole into a message (CodeQL `rust/cleartext-logging`).
    let query_start = location
        .find('?')
        .unwrap_or_else(|| panic!("redirect Location had no query string"));
    assert!(
        location.starts_with(REDIRECT_URI),
        "must redirect back to the registered redirect_uri"
    );
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
    assert_eq!(
        params.get("iss").map(String::as_str),
        Some(as_issuer.as_str()),
        "RFC 9207 iss must name this AS"
    );

    // ── Step 5: POST /token with the real PKCE verifier → tokens ───────────
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
    captured_bodies.push(("token exchange response", token_body.clone()));
    let tokens: serde_json::Value = serde_json::from_str(&token_body)
        .unwrap_or_else(|e| panic!("token response was not JSON ({e}): {token_body}"));
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["scope"], "brain");
    // Security: never interpolate `token_body` (it carries the live
    // access/refresh token pair) into a panic message once it's known to be
    // well-formed JSON — report the top-level keys that WERE present
    // instead, which is exactly what's useful to localise a missing-field
    // bug without also leaking a credential (CodeQL `rust/cleartext-logging`).
    let response_keys = |v: &serde_json::Value| -> Vec<String> {
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    let access_token_1 = tokens["access_token"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "no access_token in token response (keys: {:?})",
                response_keys(&tokens)
            )
        })
        .to_string();
    let refresh_token_1 = tokens["refresh_token"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "no refresh_token in token response (keys: {:?})",
                response_keys(&tokens)
            )
        })
        .to_string();

    // ── Step 6: `/mcp` initialize + brain_tasks WITH the Bearer token ──────
    let (status, init_resp_body) = post_mcp(
        &agent,
        &mcp_url,
        &access_token_1,
        &init_body(1),
        &[("MCP-Protocol-Version", PROTOCOL)],
    );
    assert_eq!(status, 200, "{init_resp_body}");
    captured_bodies.push(("mcp initialize response", init_resp_body.clone()));
    let init_resp: serde_json::Value = serde_json::from_str(&init_resp_body)
        .unwrap_or_else(|e| panic!("initialize response was not JSON ({e}): {init_resp_body}"));
    assert_eq!(init_resp["result"]["protocolVersion"], PROTOCOL);

    let (status, tasks_resp_body) = post_mcp(
        &agent,
        &mcp_url,
        &access_token_1,
        &call_body(
            2,
            "brain_tasks",
            serde_json::json!({"due_by": "2026-12-31"}),
        ),
        &standard_headers("tools/call", Some("brain_tasks")),
    );
    assert_eq!(status, 200, "{tasks_resp_body}");
    captured_bodies.push(("mcp brain_tasks response", tasks_resp_body.clone()));
    let tasks_resp: serde_json::Value = serde_json::from_str(&tasks_resp_body)
        .unwrap_or_else(|e| panic!("brain_tasks response was not JSON ({e}): {tasks_resp_body}"));
    let sc = &tasks_resp["result"]["structuredContent"];
    assert_eq!(sc["total"], 1, "{tasks_resp_body}");
    assert!(
        sc["tasks"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("gateway oauth e2e fixture task"),
        "{tasks_resp_body}"
    );

    // ── Step 7: refresh rotation → old refresh dead ─────────────────────────
    let (status, rotate_body) = post_token(
        &agent,
        &token_url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token_1.as_str()),
        ],
    );
    assert_eq!(status, 200, "{rotate_body}");
    captured_bodies.push(("refresh rotation response", rotate_body.clone()));
    let rotated: serde_json::Value = serde_json::from_str(&rotate_body)
        .unwrap_or_else(|e| panic!("refresh rotation response was not JSON ({e}): {rotate_body}"));
    // Security: same rule as the first token exchange above — `rotate_body`
    // carries a live token pair, never interpolate it whole once it's known
    // to be well-formed JSON.
    let access_token_2 = rotated["access_token"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "no access_token in rotation response (keys: {:?})",
                response_keys(&rotated)
            )
        })
        .to_string();
    let refresh_token_2 = rotated["refresh_token"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "no refresh_token in rotation response (keys: {:?})",
                response_keys(&rotated)
            )
        })
        .to_string();
    assert_ne!(
        access_token_2, access_token_1,
        "rotation must mint a NEW access token"
    );
    assert_ne!(
        refresh_token_2, refresh_token_1,
        "rotation must mint a NEW refresh token"
    );

    // The OLD refresh token is now dead: presenting it again is a REPLAY of
    // an already-rotated token, which RFC 6749 §4.1.2/OAuth 2.1's reuse
    // detection treats as evidence of a leak.
    let (status, replay_body) = post_token(
        &agent,
        &token_url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token_1.as_str()),
        ],
    );
    assert_eq!(
        status, 400,
        "replaying the already-rotated refresh token must fail: {replay_body}"
    );
    captured_bodies.push(("refresh replay (reuse) response", replay_body.clone()));
    let replay_err: serde_json::Value = serde_json::from_str(&replay_body)
        .unwrap_or_else(|e| panic!("replay error response was not JSON ({e}): {replay_body}"));
    assert_eq!(replay_err["error"], "invalid_grant", "{replay_body}");

    // ── Step 8: the reuse just detected must have revoked the WHOLE token
    // family — even the second-generation access token, which had done
    // nothing wrong on its own, must now 401 on /mcp ───────────────────────
    let (status, after_revoke_body) = post_mcp(
        &agent,
        &mcp_url,
        &access_token_2,
        &init_body(3),
        &[("MCP-Protocol-Version", PROTOCOL)],
    );
    assert_eq!(
        status, 401,
        "refresh-token reuse must revoke the whole family, including the newest access \
         token: {after_revoke_body}"
    );
    captured_bodies.push(("post-family-revocation /mcp response", after_revoke_body));

    // ── Pairing code must NEVER appear in any captured HTTP response body ──
    // Security: this is the exact check whose failure means the pairing
    // code leaked — printing `text` on that failure would defeat the point
    // of the check (CodeQL `rust/cleartext-logging`). Report the body length
    // instead; the label already names which response leaked it.
    for (label, text) in &captured_bodies {
        assert!(
            !text.contains(&real_pairing_code),
            "pairing code leaked into the \"{label}\" response body ({} bytes)",
            text.len()
        );
    }

    // The server must still be alive — every failure above was a per-request
    // 4xx/401, never a crash.
    assert!(
        child.0.try_wait().expect("poll gateway child").is_none(),
        "server must stay up through the entire flow"
    );
    assert_exits_after_kill(&mut child.0);
}

/// `onebrain gateway pair` — reads the CLI verb wired in Task 6, independent
/// of the capstone flow above (this spawns its own short-lived `gateway
/// pair`/`gateway pair --rotate` processes rather than `gateway run`, since
/// the verb only touches the on-disk store — no running server needed).
#[test]
fn gateway_pair_verb_prints_and_rotates_the_pairing_code() {
    let home = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let cwd = tempdir().unwrap();

    let run_pair = |rotate: bool| -> String {
        let mut args = vec!["gateway", "pair"];
        if rotate {
            args.push("--rotate");
        }
        let output = Command::new(env!("CARGO_BIN_EXE_onebrain"))
            .env("ONEBRAIN_CACHE_DIR", cache.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env_remove("ONEBRAIN_VAULT")
            .current_dir(cwd.path())
            .args(&args)
            .output()
            .unwrap_or_else(|e| panic!("spawn onebrain {args:?}: {e}"));
        assert!(
            output.status.success(),
            "onebrain {args:?} did not exit 0: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // Security: `first`/`second`/`rotated` (this command's stdout) and
    // `on_disk_after_first`/`on_disk_after_rotate` (the persisted code read
    // straight off disk) are all live pairing-code material by design —
    // never interpolate any of them whole into an assertion message
    // (CodeQL `rust/cleartext-logging`). Every check below keeps its exact
    // boolean condition; only the diagnostic text changes.

    // First call (no `--rotate`) mints the code on first use and prints it.
    let first = run_pair(false);
    assert!(
        first.contains("pairing code: "),
        "stdout missing the \"pairing code: \" prefix"
    );
    let on_disk_after_first = read_pairing_code(home.path());
    assert!(
        first.contains(&on_disk_after_first),
        "printed code did not match the persisted code (printed {} bytes, disk code is {} bytes)",
        first.trim().len(),
        on_disk_after_first.len()
    );

    // A second call with no `--rotate` is idempotent — same code, both in
    // stdout and on disk.
    let second = run_pair(false);
    assert!(
        second.contains(&on_disk_after_first),
        "a non-rotating call's stdout did not contain the persisted code"
    );
    assert_eq!(
        read_pairing_code(home.path()),
        on_disk_after_first,
        "a non-rotating call must not change the persisted code"
    );

    // `--rotate` mints a NEW code, printed and persisted, that differs from
    // the original.
    let rotated = run_pair(true);
    assert!(
        rotated.contains("pairing code rotated: "),
        "stdout missing the \"pairing code rotated: \" prefix"
    );
    let on_disk_after_rotate = read_pairing_code(home.path());
    assert_ne!(
        on_disk_after_rotate, on_disk_after_first,
        "--rotate must mint a different code"
    );
    assert!(
        rotated.contains(&on_disk_after_rotate),
        "printed rotated code did not match the persisted code"
    );
}
