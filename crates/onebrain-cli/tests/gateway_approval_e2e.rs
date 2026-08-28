//! Gateway approval capstone (Gateway PR 4, Task 6) — the end-to-end proof
//! that the policy/approval/audit machinery Tasks 1-5 built in isolation
//! (`policy.rs`'s `decide`, `approval.rs`'s `Approvals` registry,
//! `approval_routes.rs`'s operator `/approvals` surface, and `server.rs`'s
//! `brain_capture`) actually composes correctly against a REAL spawned
//! `onebrain gateway run` process, driven entirely over real HTTP, exactly
//! like a real MCP client + a real human operator would: complete OAuth to
//! get a token, call `brain_capture` under a `policy.mutating: ask_once`
//! config, watch it block, resolve it through the operator `/approvals`
//! surface, confirm the note landed on disk with the right content, confirm
//! a second call within the resulting grant's TTL needs no approval at all,
//! and confirm both calls are in the audit log with the right decisions.
//! Also proves the privilege-separation property end-to-end: a connector's
//! own live OAuth bearer token — the credential that legitimately calls
//! `/mcp` — is flatly rejected on `/approvals`.
//!
//! Model: `tests/gateway_oauth_e2e.rs`'s sandboxing (`KillOnDrop`, tempdir
//! HOME/cache so `~/.onebrain` never touches the real machine, stdout
//! captured to a file this test polls since the gateway blocks on Ctrl-C
//! after a successful bind) and its SEP-2243 MCP header set
//! (`MCP-Protocol-Version`/`Mcp-Method`/`Mcp-Name`). Every helper below is
//! COPIED, not imported, from `gateway_oauth_e2e.rs`/`gateway_http.rs` (test
//! files in this crate cannot share non-`support` helpers — see
//! `gateway_oauth_e2e.rs`'s own module doc comment — and this crate ships no
//! library target for any of them to depend on). The OAuth leg is trimmed
//! relative to `gateway_oauth_e2e.rs`'s own capstone: it already proves the
//! unauthenticated-401/PRM-challenge-parsing/refresh-rotation properties in
//! full, so this file discovers the protected-resource-metadata document
//! directly off the gateway's own origin (a legitimate shortcut — RFC 9728
//! guarantees it lives at the fixed `{issuer}/.well-known/
//! oauth-protected-resource` path either way) and skips refresh rotation
//! entirely, spending its own complexity budget on the approval flow that is
//! actually new here.
//!
//! ## BINDING REQUIREMENT: no real dialog on CI
//!
//! This test drives `brain_capture` under `ask_once`, so — absent a
//! guard — the spawned gateway subprocess would reach `server::
//! await_approval`, see `approval_native::is_available()` return `true` on
//! any macOS runner (every macOS box ships `/usr/bin/osascript`), and fire a
//! real, blocking, unattended `osascript display dialog` GUI popup with
//! nobody there to click it — hanging this test until the approval TTL
//! expires, or worse, blocking the CI runner itself. `server.rs`'s own
//! `cfg!(test)` guard does NOT cover this: it is a compile-time constant,
//! true only inside `onebrain-cli`'s OWN `#[cfg(test)] mod tests` compiled
//! into ITS test binary — false in the ordinary release/debug `onebrain`
//! binary this test spawns as a separate process. `spawn_gateway` below sets
//! `ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL=1` in that subprocess's own
//! environment instead — the env-var escape hatch
//! `commands/gateway/approval_native.rs::is_available` checks FIRST, before
//! its platform/`osascript` probe (see that module's own doc comment,
//! "Disabling the channel from outside the process", for the full
//! rationale). The literal string here MUST stay in sync with
//! `approval_native::DISABLE_NATIVE_APPROVAL_ENV` — this test binary has no
//! library target to import that constant from.
//!
//! ## BINDING REQUIREMENT: no stray daemon left running
//!
//! A successful `brain_capture` ends with a best-effort reindex that calls
//! `daemon_client::ensure_running`, which spawns `onebrain daemon start`
//! when no warm daemon is already up. That is correct in production and
//! unacceptable here: an earlier revision of this test left a real
//! `onebrain daemon __run` process alive against its own tempdir vault
//! after every run, on the developer machine and on CI alike. `cfg!(test)`
//! cannot help — the gateway under test is a separately compiled,
//! separately spawned binary — and the pre-existing `ONEBRAIN_NO_DAEMON`
//! kill switch does not reach this path either (it gates only
//! `search_common::route_to_daemon`, the PASSIVE routing check, never the
//! ACTIVE `ensure_running` spawn). `spawn_gateway` therefore also sets
//! `ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX=1`, the sibling switch
//! `server::reindex_channel_enabled` reads, and the test ASSERTS the
//! outcome rather than trusting it: after two successful captures,
//! `$HOME/.onebrain/run/` must not exist, since `ensure_running` creates
//! that directory before it can discover or spawn anything. Same sync
//! caveat as above — the literal must match
//! `server::DISABLE_DAEMON_REINDEX_ENV`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::tempdir;

// ── Copied from `gateway_oauth_e2e.rs` (see module docs: not shared) ───────

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
/// process blocks on Ctrl-C after a successful bind, so `.output()` would
/// hang forever waiting for it to exit.
///
/// `ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL=1` and
/// `ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX=1` are the two BINDING
/// REQUIREMENTS from this file's own module docs — see there for the
/// rationale behind each.
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
        .env("ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL", "1")
        .env("ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX", "1")
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
            // Security: `out` may already contain the real pairing code by
            // this point (it's printed before the "gateway listening" line)
            // — never interpolate it whole into a panic message (CodeQL
            // `rust/cleartext-logging`). `err` is safe: stderr only ever
            // carries the fixed "loopback only ..." notice, never a secret.
            panic!(
                "onebrain gateway run exited early ({status}) before printing the \
                 listening line ({} bytes of stdout captured): stderr={err:?}",
                out.len()
            );
        }
        assert!(
            Instant::now() < deadline,
            "onebrain gateway run did not print the listening line within 30s \
             ({} bytes of stdout captured so far)",
            out.len()
        );
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
/// inspects `4xx`/`401` responses directly) AND never follows a redirect
/// (`max_redirects(0)`) — the `POST /authorize` success response is a `302`
/// this test parses `code`/`state` off directly; the `redirect_uri` it
/// points at (`http://127.0.0.1/callback`) is never actually hosted.
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
            "clientInfo": {"name": "gateway-approval-e2e-test", "version": "0.0.0"},
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

/// Base64url (RFC 4648 §5), unpadded — same algorithm as
/// `commands/gateway/auth/core.rs::base64url_nopad`, copied (not shared: no
/// library target) so this test can build its own PKCE pair.
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
    use sha2::{Digest, Sha256};
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS CSPRNG unavailable for test PKCE verifier");
    let verifier = base64url_nopad(&buf);
    let challenge = base64url_nopad(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// RFC 3986 §2.3 unreserved-only percent-encoder — used for the
/// `application/x-www-form-urlencoded` POST bodies this test sends. Every
/// value this test encodes is either base64url (already unreserved-only) or
/// a fixed opaque test string, so a byte-simple implementation is
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
/// percent-decoding every value.
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
/// location_header)` — `location_header` is `Some` only on the `302` success
/// path (this agent never follows it).
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

/// Read the CURRENT device-pairing code straight out of the sandboxed
/// `$HOME/.onebrain/gateway/pairing.json` — see `gateway_oauth_e2e.rs`'s own
/// doc comment for why this is a legitimate shortcut (this test IS the
/// machine's owner in the scenario it simulates), copied verbatim here.
fn read_pairing_code(home: &Path) -> String {
    let path = home.join(".onebrain").join("gateway").join("pairing.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read pairing.json at {}: {e}", path.display()));
    let json: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("pairing.json was not JSON ({e}): {raw}"));
    json["code"]
        .as_str()
        .unwrap_or_else(|| panic!("pairing.json had no string \"code\" field: {raw}"))
        .to_string()
}

// ── New for this test: /approvals HTTP surface + audit-log reading ────────

/// GET `{approvals_url}` with the operator's pairing code, returning
/// `(status, body_text)`.
fn get_approvals(agent: &ureq::Agent, approvals_url: &str, pairing_code: &str) -> (u16, String) {
    let mut resp = agent
        .get(approvals_url)
        .header("X-OneBrain-Pairing", pairing_code)
        .call()
        .unwrap_or_else(|e| panic!("GET {approvals_url} failed: {e}"));
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|e| panic!("read response body from {approvals_url}: {e}"));
    (status, text)
}

/// GET `{approvals_url}` presenting ONLY a connector's OAuth bearer token
/// (no pairing header at all) — the privilege-separation probe. Returns just
/// the status: a 401 body carries nothing worth inspecting.
fn get_approvals_with_bearer_only(agent: &ureq::Agent, approvals_url: &str, bearer: &str) -> u16 {
    let resp = agent
        .get(approvals_url)
        .header("authorization", format!("Bearer {bearer}"))
        .call()
        .unwrap_or_else(|e| panic!("GET {approvals_url} failed: {e}"));
    resp.status().as_u16()
}

/// POST `{approvals_url}/{id}` presenting ONLY a connector's OAuth bearer
/// token (no pairing header at all) — the second half of the
/// privilege-separation probe: a connector must not be able to self-approve
/// even with a real, known pending id.
fn post_resolve_with_bearer_only(
    agent: &ureq::Agent,
    approvals_url: &str,
    id: &str,
    bearer: &str,
    decision: &str,
) -> u16 {
    let url = format!("{approvals_url}/{id}");
    let resp = agent
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .send(serde_json::json!({ "decision": decision }).to_string())
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));
    resp.status().as_u16()
}

/// POST `{approvals_url}/{id}` with the operator's pairing code, returning
/// `(status, body_text)`.
fn post_resolve_approval(
    agent: &ureq::Agent,
    approvals_url: &str,
    id: &str,
    pairing_code: &str,
    decision: &str,
) -> (u16, String) {
    let url = format!("{approvals_url}/{id}");
    let mut resp = agent
        .post(&url)
        .header("content-type", "application/json")
        .header("X-OneBrain-Pairing", pairing_code)
        .send(serde_json::json!({ "decision": decision }).to_string())
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|e| panic!("read response body from {url}: {e}"));
    (status, text)
}

/// Bounded poll (10s) for `GET {approvals_url}` to show EXACTLY one pending
/// `brain_capture` approval, returning its `id`. This is what proves
/// `brain_capture` genuinely registered a pending approval and is blocked
/// waiting on a human decision — not merely that the HTTP response hasn't
/// arrived back yet for some unrelated reason.
fn wait_for_one_pending_approval(
    agent: &ureq::Agent,
    approvals_url: &str,
    pairing_code: &str,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, body) = get_approvals(agent, approvals_url, pairing_code);
        assert_eq!(status, 200, "{body}");
        let list: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("approvals list was not JSON ({e}): {body}"));
        let arr = list
            .as_array()
            .unwrap_or_else(|| panic!("approvals list was not a JSON array: {body}"));
        assert!(
            arr.len() <= 1,
            "expected at most one pending approval: {body}"
        );
        if let Some(entry) = arr.first() {
            assert_eq!(entry["tool"], "brain_capture", "{body}");
            return entry["id"]
                .as_str()
                .unwrap_or_else(|| panic!("pending approval had no id: {body}"))
                .to_string();
        }
        assert!(
            Instant::now() < deadline,
            "no pending approval appeared within 10s"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Reads every JSONL line back out of `{home}/.onebrain/gateway/audit/`,
/// across every month file present (this test is short-lived, so in
/// practice this is always exactly one file), parsed as loose
/// `serde_json::Value`s in file (hence chronological, since month files sort
/// lexically) then line order — mirrors `server.rs`'s own in-process
/// `read_audit_entries` test helper, against the REAL on-disk log a spawned
/// subprocess actually wrote.
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

/// Set up a minimal fixture vault and a machine-level `~/.onebrain/gateway.yml`
/// naming it as the default vault, with `policy.mutating: ask_once` — the
/// one config knob this whole test exists to drive `brain_capture` through.
/// Every other `policy:` field is left at its default (`approval_wait_seconds:
/// 300`, `grant_ttl_minutes: 30`) — comfortably long enough for this test's
/// own poll-then-resolve loop, and long enough that the grant recorded by
/// the first approval is still live for the second call.
fn write_fixture_vault_and_config(home: &Path) -> tempfile::TempDir {
    let vault = tempdir().unwrap();
    write(vault.path(), "onebrain.yml", "folders: {}\n");
    write(
        home,
        ".onebrain/gateway.yml",
        &format!(
            "default_vault: {v}\nvaults:\n  t1: {v}\npolicy:\n  mutating: ask_once\n",
            v = vault.path().display()
        ),
    );
    vault
}

/// The full capstone: real OAuth token, `brain_capture` blocking under
/// `ask_once`, resolution via the operator `/approvals` surface, the note's
/// real on-disk content, grant reuse on a second call, both calls in the
/// audit log with the right decisions, and connector-bearer-token rejection
/// on `/approvals`.
#[test]
fn gateway_ask_once_approval_flow_completes_writes_and_reuses_the_grant() {
    let home = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let vault = write_fixture_vault_and_config(home.path());

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

    // ── OAuth: discovery → register → authorize → token ─────────────────
    // (Trimmed relative to `gateway_oauth_e2e.rs`'s own capstone — see this
    // file's module docs for why going through the 401 challenge again
    // would be redundant: RFC 9728 guarantees the PRM document lives at this
    // fixed path off the gateway's own origin either way.)
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
        "client_name": "gateway-approval-e2e-test-client",
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
    const STATE: &str = "gateway-approval-e2e-state";
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
    // interpolate it whole into a panic/assert message from here on
    // (CodeQL `rust/cleartext-logging`).
    let access_token = tokens["access_token"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "no access_token in token response (keys: {:?})",
                response_keys(&tokens)
            )
        })
        .to_string();

    let approvals_url = format!("{as_issuer}/approvals");

    // ── `initialize` first, exactly like a real MCP client would before
    // its first `tools/call` ─────────────────────────────────────────────
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

    // ── Step: brain_capture under ask_once genuinely blocks ─────────────
    let mcp_url_bg = mcp_url.clone();
    let token_bg = access_token.clone();
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
                    "title": "Approval Test Note",
                    "text": "captured via the gateway approval e2e flow",
                }),
            ),
            &standard_headers("tools/call", Some("brain_capture")),
        )
    });

    let pending_id = wait_for_one_pending_approval(&agent, &approvals_url, &real_pairing_code);
    assert!(
        !call_handle.is_finished(),
        "brain_capture must still be blocked on the pending approval, not yet returned"
    );
    assert_eq!(
        inbox_note_count(vault.path()),
        0,
        "no note may exist before the pending approval is resolved"
    );

    // ── Step: resolve via GET/POST /approvals with the pairing code ─────
    let (status, resolve_body) = post_resolve_approval(
        &agent,
        &approvals_url,
        &pending_id,
        &real_pairing_code,
        "approve",
    );
    assert_eq!(status, 200, "{resolve_body}");
    let resolve_json: serde_json::Value = serde_json::from_str(&resolve_body)
        .unwrap_or_else(|e| panic!("resolve response was not JSON ({e}): {resolve_body}"));
    assert_eq!(resolve_json["resolved"], true, "{resolve_body}");

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
    let content1 = std::fs::read_to_string(vault.path().join(&path1))
        .unwrap_or_else(|e| panic!("read captured note {path1}: {e}"));
    assert!(content1.contains("tags: [capture]"), "{content1}");
    // The note's `# ` heading is derived from the FILENAME slug
    // (`derive_slug`'s sanitized, lowercased, hyphenated form), not the raw
    // `title` argument verbatim — matching `server.rs`'s own
    // `brain_capture_under_auto_policy_creates_a_note_with_frontmatter_and_body`
    // unit test, which deliberately checks the body text and frontmatter
    // rather than the heading's exact casing for the same reason.
    assert!(content1.contains("approval-test-note"), "{content1}");
    assert!(
        content1.contains("captured via the gateway approval e2e flow"),
        "{content1}"
    );

    // ── A second capture, same client, within the grant TTL: no approval
    // needed at all — proceeds synchronously, registers no new pending
    // entry ────────────────────────────────────────────────────────────
    let (status2, call2_body) = post_mcp(
        &agent,
        &mcp_url,
        &access_token,
        &call_body(
            3,
            "brain_capture",
            serde_json::json!({
                "title": "Second Approval Test Note",
                "text": "the grant should let this one through immediately",
            }),
        ),
        &standard_headers("tools/call", Some("brain_capture")),
    );
    assert_eq!(status2, 200, "{call2_body}");
    let call2: serde_json::Value = serde_json::from_str(&call2_body).unwrap_or_else(|e| {
        panic!("second brain_capture response was not JSON ({e}): {call2_body}")
    });
    assert!(call2.get("error").is_none(), "{call2}");
    let path2 = call2["result"]["structuredContent"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("no structuredContent.path: {call2}"))
        .to_string();
    assert_ne!(path2, path1, "the second capture must be a distinct note");
    assert!(
        vault.path().join(&path2).exists(),
        "the second capture's note must exist on disk"
    );

    let (status, empty_body) = get_approvals(&agent, &approvals_url, &real_pairing_code);
    assert_eq!(status, 200, "{empty_body}");
    let empty_list: serde_json::Value = serde_json::from_str(&empty_body).unwrap();
    assert_eq!(
        empty_list.as_array().map(Vec::len),
        Some(0),
        "the grant-satisfied second call must never have registered a pending approval: {empty_body}"
    );

    // ── No stray daemon: the second BINDING REQUIREMENT, asserted ──────
    // `daemon_client::ensure_running` creates `$HOME/.onebrain/run/` (via
    // `resolve_slot` -> `ensure_private_run_dir`) before it can discover or
    // spawn anything at all, so the absence of that directory after two
    // successful captures proves the reindex block was never entered — and
    // therefore that no `onebrain daemon __run` process was left behind.
    assert!(
        !home.path().join(".onebrain").join("run").exists(),
        "the gateway must not have spawned or probed a daemon: \
         ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX did not take effect"
    );

    // ── The audit log carries both calls with the right decisions ───────
    let entries = read_audit_entries(home.path());
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(entries[0]["tool"], "brain_capture", "{entries:?}");
    assert_eq!(entries[0]["decision"], "approved", "{entries:?}");
    assert_eq!(entries[0]["outcome"], "ok", "{entries:?}");
    assert_eq!(entries[1]["tool"], "brain_capture", "{entries:?}");
    assert_eq!(
        entries[1]["decision"], "auto",
        "the second call must be audited as `auto` — satisfied by the live grant, not a \
         fresh approval: {entries:?}"
    );
    assert_eq!(entries[1]["outcome"], "ok", "{entries:?}");
    // The audit trail must never carry a raw note body.
    for entry in &entries {
        let summary = entry["args_summary"].as_str().unwrap_or_default();
        assert!(
            !summary.contains("captured via the gateway approval e2e flow"),
            "{entries:?}"
        );
    }

    // ── Privilege separation, end to end: a connector's own live bearer
    // token must never satisfy the operator pairing gate ────────────────
    let bearer_only_status = get_approvals_with_bearer_only(&agent, &approvals_url, &access_token);
    assert_eq!(
        bearer_only_status, 401,
        "a connector's own bearer token must never list pending approvals"
    );
    // Reuses `pending_id` from the first call (already resolved and removed
    // by now) purely as a syntactically well-formed id — the point is that
    // `require_pairing_header` 401s BEFORE the handler ever looks the id up,
    // so a connector's bearer token can't even reach the "does this id
    // exist" question, let alone self-approve a REAL pending one.
    let resolve_bearer_only_status = post_resolve_with_bearer_only(
        &agent,
        &approvals_url,
        &pending_id,
        &access_token,
        "approve",
    );
    assert_eq!(
        resolve_bearer_only_status, 401,
        "a connector's own bearer token must never resolve an approval, even by a known id"
    );

    // The server must still be alive — every failure above was a per-request
    // 4xx, never a crash.
    assert!(
        child.0.try_wait().expect("poll gateway child").is_none(),
        "server must stay up through the entire flow"
    );
    assert_exits_after_kill(&mut child.0);
}

/// Count of `.md` files directly under `<vault_root>/00-inbox` — `0` (not a
/// panic) when the folder doesn't exist yet, since a still-blocked
/// `brain_capture` never even reaches `create_dir_all`.
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
