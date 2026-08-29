//! `onebrain gateway telegram setup` — Gateway PR 5, Task 6.
//!
//! A one-command wizard that turns a BotFather token into a fully wired
//! `gateway.yml` `telegram:` block: paste the token, press START on the bot
//! from Telegram, and this captures the resulting `chat_id` and writes both
//! fields to disk, then sends a confirmation message through the freshly
//! configured channel.
//!
//! [`run_setup`] is the testable core — every side effect (stdin/stdout,
//! the Telegram API base, the `~/.onebrain` directory) is a parameter, so
//! tests drive it against an in-memory buffer, a mock Bot API server, and a
//! tempdir with no env-var plumbing at all. [`telegram_setup`] is the thin
//! production wrapper `dispatch.rs` calls, which resolves the real values
//! and hands them to [`run_setup`].

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::Context;

use super::telegram_api::BotApi;

/// Total wall-clock budget the wizard waits for the human to open Telegram
/// and press START — see [`run_setup`]'s own doc comment for why this is
/// enforced as a bounded number of [`WAIT_POLL_SECS`]-second poll calls
/// rather than an `Instant` deadline.
const MAX_WAIT_SECS: u32 = 60;

/// `getUpdates` long-poll `timeout` sent on every wizard poll call.
const WAIT_POLL_SECS: u32 = 5;

/// The wizard's testable core (Gateway PR 5, Task 6 brief's own signature).
/// Every side effect — `input`/`out` instead of real stdin/stdout,
/// `api_base` instead of [`super::telegram::api_base`], `gateway_dir`
/// instead of resolving `~/.onebrain` via [`crate::home::home_dir`] — is a
/// parameter, so tests point each one at a mock server / in-memory buffer /
/// tempdir with zero env-var plumbing. [`telegram_setup`] is the thin
/// production wrapper that resolves the real values and calls this.
///
/// Flow (exact, per the Task 6 brief):
/// 1. Print BotFather instructions, read one line of stdin as the bot
///    token.
/// 2. [`BotApi::get_me`] to validate it. A failure here is reported as
///    EXACTLY `"token was not accepted by Telegram"` — never the
///    underlying [`super::telegram_api::TgError`] text (which never
///    carries the token anyway, per that module's "scrub chokepoint" docs,
///    but this wizard adds its own fixed message on top rather than
///    forwarding ANY dynamic text from a failed auth call).
/// 3. Print the "find @{username}, press START" prompt, then long-poll
///    `getUpdates` until a `message` update arrives — see the loop below
///    for why this is budgeted by CALL COUNT rather than an `Instant`
///    deadline.
/// 4. Reject a captured `chat_id` that isn't POSITIVE — Telegram gives
///    private (human-to-bot) chats positive ids and groups/supergroups
///    negative ones, and [`super::telegram::is_available`] requires a
///    positive id for exactly this reason (see that function's own doc
///    comment): a group `chat_id` would silently configure a dead channel.
/// 5. Read-modify-write `gateway_dir/gateway.yml`'s `telegram:` block via
///    [`write_telegram_config`] — see that function's own doc comment for
///    why a raw [`serde_yaml::Value`], not [`super::config::GatewayConfig`].
/// 6. Send the confirmation message, then print the chat id.
///
/// A timeout at step 3 (no `/start` observed) returns an error WITHOUT
/// writing any config — [`write_telegram_config`] is only ever reached once
/// a chat id is in hand.
///
/// Deliberately does NOT touch [`super::telegram`]'s `getUpdates` offset
/// file (`~/.onebrain/gateway/telegram-<token_key>.offset`) — that file is
/// the PRODUCTION poller's own persisted cursor. This wizard's own `offset`
/// variable below is local and transient: it exists only so THIS run's
/// repeated polls don't re-return the same already-seen updates, and it is
/// discarded the moment `run_setup` returns. Nobody should ever wire these
/// two together — a wizard run advancing the poller's persisted cursor (or
/// vice versa) would let a real approval-flow `/start` or button press go
/// unseen by whichever side didn't get credit for consuming it.
pub(crate) fn run_setup(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    api_base: &str,
    gateway_dir: &Path,
) -> anyhow::Result<()> {
    writeln!(out, "OneBrain gateway — Telegram setup")?;
    writeln!(out)?;
    writeln!(out, "1. Open Telegram and message @BotFather")?;
    writeln!(out, "2. Send /newbot and follow the prompts")?;
    writeln!(
        out,
        "3. BotFather replies with a token like 123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"
    )?;
    write!(out, "Paste that token here: ")?;
    out.flush()?;

    let mut token_line = String::new();
    input
        .read_line(&mut token_line)
        .context("read bot token from stdin")?;
    let token = token_line.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("no token entered");
    }

    let api = BotApi::new(&token, api_base);
    let identity = api
        .get_me()
        .map_err(|_| anyhow::anyhow!("token was not accepted by Telegram"))?;

    writeln!(
        out,
        "Open Telegram, find @{}, press START. Waiting (up to {MAX_WAIT_SECS} s)…",
        identity.username
    )?;
    out.flush()?;

    // Budgeted by CALL COUNT (`MAX_WAIT_SECS / WAIT_POLL_SECS` iterations),
    // not by wall-clock `Instant`: against the REAL Telegram API, a
    // `getUpdates` call that finds nothing genuinely blocks server-side for
    // ~`WAIT_POLL_SECS` seconds, so bounding by iteration count already
    // approximates ~`MAX_WAIT_SECS` real seconds end-to-end. Against a test
    // mock that answers every call instantly, the SAME loop finishes in
    // milliseconds instead of stalling `setup_times_out_cleanly_without_writing_config`
    // for a genuine 60 real seconds — deterministic and fast either way,
    // with no test-only clock-injection knob needed.
    let mut offset: Option<i64> = None;
    let mut captured: Option<(i64, i64)> = None;
    for _ in 0..(MAX_WAIT_SECS / WAIT_POLL_SECS) {
        let updates = api
            .get_updates(offset, WAIT_POLL_SECS)
            .context("poll Telegram for /start")?;

        // Every update in the batch must still advance `offset` past it —
        // including ones that AREN'T the `/start` message this loop is
        // looking for — or an unrelated update (a chat-member change, an
        // edited message, …) ahead of the real one in the same batch would
        // make Telegram replay it forever. Mirrors
        // `super::telegram::poll_loop`'s own "highest update_id in the
        // batch, clamped against the offset already on record" logic.
        let batch_max = updates.iter().map(|u| u.update_id).max();
        for update in &updates {
            if captured.is_none() {
                if let Some(chat_id) = update.message_chat_id {
                    captured = Some((chat_id, update.message_from_id.unwrap_or(0)));
                }
            }
        }
        if let Some(max_id) = batch_max {
            let candidate = max_id.saturating_add(1);
            offset = Some(offset.map_or(candidate, |cur| candidate.max(cur)));
        }
        if captured.is_some() {
            break;
        }
    }

    let (chat_id, _from_id) = captured.ok_or_else(|| {
        anyhow::anyhow!(
            "timed out waiting for /start — open Telegram, press START on the bot, then run this setup again"
        )
    })?;

    if chat_id <= 0 {
        anyhow::bail!(
            "that looks like a group chat — Telegram approvals need a private one-to-one chat with the bot; open a DM with @{}, press START, then run this setup again",
            identity.username
        );
    }

    write_telegram_config(gateway_dir, &token, chat_id)?;

    api.send_message(
        chat_id,
        "OneBrain gateway connected. Approval requests will appear here.",
        None,
    )
    .map_err(|e| {
        anyhow::anyhow!("gateway.yml saved, but the confirmation message failed to send: {e}")
    })?;

    writeln!(out, "Telegram gateway connected (chat_id {chat_id}).")?;
    Ok(())
}

/// Read-modify-write `gateway_dir.join("gateway.yml")`'s `telegram:` block
/// via a raw [`serde_yaml::Value`] — NOT the typed
/// [`super::config::GatewayConfig`], because that struct's `bot_token`
/// field is `#[serde(skip_serializing)]` (see that field's own doc
/// comment): serializing the typed struct back out would silently DROP the
/// token this wizard just captured. The raw-`Value` round-trip also
/// preserves every OTHER top-level key already in the file (`port`,
/// `vaults`, `policy`, …) untouched — exactly the brief's "preserving
/// unknown keys" requirement.
///
/// Creates `gateway_dir` (0700) and the file itself (0600) via
/// [`write_private_yaml`] if neither exists yet — this is the FIRST writer
/// `gateway.yml` has ever had in this crate (every earlier task only read
/// it; see `config.rs`'s own module doc comment).
fn write_telegram_config(gateway_dir: &Path, bot_token: &str, chat_id: i64) -> anyhow::Result<()> {
    let path = gateway_dir.join("gateway.yml");

    let mut mapping = match std::fs::read_to_string(&path) {
        Ok(content) => {
            let value: serde_yaml::Value = serde_yaml::from_str(&content)
                .with_context(|| format!("parse existing {}", path.display()))?;
            match value {
                serde_yaml::Value::Mapping(m) => m,
                serde_yaml::Value::Null => serde_yaml::Mapping::new(),
                other => anyhow::bail!(
                    "{} must be a YAML mapping at its root, found {other:?}",
                    path.display()
                ),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_yaml::Mapping::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };

    let mut telegram = serde_yaml::Mapping::new();
    telegram.insert(
        serde_yaml::Value::String("bot_token".to_string()),
        serde_yaml::Value::String(bot_token.to_string()),
    );
    telegram.insert(
        serde_yaml::Value::String("chat_id".to_string()),
        serde_yaml::Value::Number(chat_id.into()),
    );
    mapping.insert(
        serde_yaml::Value::String("telegram".to_string()),
        serde_yaml::Value::Mapping(telegram),
    );

    let rendered = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .context("serialize gateway.yml")?;
    write_private_yaml(&path, rendered.as_bytes())
}

/// Create `dir` with owner-only (0700) permissions on Unix, re-asserting
/// the mode (warn, never silently swallow) if it already existed with
/// looser bits. Plain recursive create on non-Unix. Mirrors
/// `telegram::ensure_private_dir` / `auth::store::ensure_private_dir`
/// exactly — duplicated, not imported, per this crate's established
/// "one private copy per module" convention (see either of those modules'
/// own doc comments).
fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create gateway config dir {}", dir.display()))?;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(error = %e, path = %dir.display(), "could not re-assert 0700 on gateway config dir");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create gateway config dir {}", dir.display()))
    }
}

/// Atomically replace `path` with `bytes` (already-rendered YAML) via a
/// `.tmp` sibling written 0600, re-asserted, then renamed over the real
/// path. Mirrors `auth::store::write_json_atomic` exactly, minus the
/// `Serialize` step — this module renders YAML text itself (see
/// [`write_telegram_config`]) rather than serializing a typed value.
fn write_private_yaml(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let tmp = path.with_extension("yml.tmp");
    {
        use std::fs::OpenOptions;
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write {}", tmp.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(error = %e, path = %tmp.display(), "could not re-assert 0600 on gateway.yml");
        }
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// `onebrain gateway telegram setup` — the CLI-facing entry point. Resolves
/// the real stdin/stdout, the real Telegram API base
/// ([`super::telegram::api_base`]), and `~/.onebrain` (via
/// [`super::config::gateway_config_path`]'s own parent — reusing that
/// function's home resolution rather than re-deriving it, so this can never
/// drift from where `gateway run` itself reads `gateway.yml`), then hands
/// everything to [`run_setup`].
///
/// `mode` is accepted for signature parity with every other verb handler
/// `dispatch.rs` calls uniformly ([`super::pair`]'s own doc comment
/// explains the same convention) — this wizard's output is a fixed,
/// human-read prompt/confirmation flow, not a structured `--json`/`--yaml`
/// payload.
pub fn telegram_setup(_mode: &crate::output::OutputMode) -> anyhow::Result<()> {
    let gateway_yml = super::config::gateway_config_path()?;
    let gateway_dir = gateway_yml
        .parent()
        .map(Path::to_path_buf)
        .context("resolve gateway config directory")?;

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    run_setup(
        &mut input,
        &mut out,
        &super::telegram::api_base(),
        &gateway_dir,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path as AxumPath, State};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::Value as JsonValue;
    use std::collections::{HashMap, VecDeque};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    // ── Mock Telegram Bot API server ───────────────────────────────────
    //
    // Duplicated from `telegram_api.rs` / `telegram.rs`'s own test-only
    // fixtures rather than shared, per this crate's established "one
    // private copy per module" convention (see either module's doc
    // comments for the precedent). `queued` gives per-method FIFO
    // responses — `setup_captures_chat_id_and_writes_0600_config` needs
    // the FIRST `getUpdates` call to answer empty and the SECOND to carry
    // the `/start` message, which a single static response can't express.

    #[derive(Clone, Default)]
    struct MockState {
        responses: Arc<Mutex<HashMap<String, JsonValue>>>,
        queued: Arc<Mutex<HashMap<String, VecDeque<JsonValue>>>>,
        requests: Arc<Mutex<Vec<(String, JsonValue)>>>,
    }

    impl MockState {
        fn set_response(&self, method: &str, body: JsonValue) {
            self.responses
                .lock()
                .unwrap()
                .insert(method.to_string(), body);
        }

        fn queue_response(&self, method: &str, body: JsonValue) {
            self.queued
                .lock()
                .unwrap()
                .entry(method.to_string())
                .or_default()
                .push_back(body);
        }

        fn requests(&self) -> Vec<(String, JsonValue)> {
            self.requests.lock().unwrap().clone()
        }

        fn calls(&self, method: &str) -> usize {
            self.requests().iter().filter(|(m, _)| m == method).count()
        }
    }

    async fn mock_handler(
        AxumPath(params): AxumPath<HashMap<String, String>>,
        State(state): State<MockState>,
        Json(body): Json<JsonValue>,
    ) -> Json<JsonValue> {
        let method = params.get("method").cloned().unwrap_or_default();
        state.requests.lock().unwrap().push((method.clone(), body));

        let queued = state
            .queued
            .lock()
            .unwrap()
            .get_mut(&method)
            .and_then(|q| q.pop_front());
        let scripted = state.responses.lock().unwrap().get(&method).cloned();
        let resp = match queued {
            Some(v) => v,
            None => scripted.unwrap_or_else(|| serde_json::json!({ "ok": true, "result": null })),
        };
        Json(resp)
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
            // Bounded, mirroring `telegram_api.rs`'s own copy of this
            // harness exactly: an unbounded wait here means a sandbox that
            // refuses loopback binds hangs the whole test binary instead of
            // failing one test.
            let deadline = Instant::now() + Duration::from_secs(5);
            let bound = loop {
                let p = port.load(Ordering::SeqCst);
                if p != 0 {
                    break p;
                }
                assert!(Instant::now() < deadline, "server never bound");
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

    #[cfg(unix)]
    fn file_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // ── Tests ────────────────────────────────────────────────────────

    #[test]
    fn setup_captures_chat_id_and_writes_0600_config() {
        let state = MockState::default();
        state.set_response(
            "getMe",
            serde_json::json!({ "ok": true, "result": { "username": "my_onebrain_bot" } }),
        );
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [] }),
        );
        state.queue_response(
            "getUpdates",
            serde_json::json!({
                "ok": true,
                "result": [{
                    "update_id": 10,
                    "message": { "chat": { "id": 555 }, "from": { "id": 777 } }
                }]
            }),
        );
        state.set_response(
            "sendMessage",
            serde_json::json!({ "ok": true, "result": { "message_id": 1 } }),
        );
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let mut input = Cursor::new(b"123456:ABCDEF-token\n".to_vec());
        let mut out: Vec<u8> = Vec::new();

        run_setup(&mut input, &mut out, &server.base, &gateway_dir).unwrap();

        let gateway_yml = gateway_dir.join("gateway.yml");
        let content = std::fs::read_to_string(&gateway_yml).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        assert_eq!(
            parsed["telegram"]["bot_token"].as_str(),
            Some("123456:ABCDEF-token")
        );
        assert_eq!(parsed["telegram"]["chat_id"].as_i64(), Some(555));

        #[cfg(unix)]
        assert_eq!(
            file_mode(&gateway_yml),
            0o600,
            "gateway.yml must be 0600 (holds the bot token)"
        );

        let requests = state.requests();
        let send = requests
            .iter()
            .find(|(m, _)| m == "sendMessage")
            .expect("sendMessage must have been called");
        assert_eq!(send.1["chat_id"], 555);
        assert_eq!(
            send.1["text"],
            "OneBrain gateway connected. Approval requests will appear here."
        );

        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("555"), "{printed}");
    }

    #[test]
    fn setup_rejects_a_bad_token_without_echoing_it() {
        let state = MockState::default();
        state.set_response(
            "getMe",
            serde_json::json!({ "ok": false, "description": "Unauthorized" }),
        );
        let server = MockServer::start(state);

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let mut input = Cursor::new(b"BAD-SECRET-TOKEN-999\n".to_vec());
        let mut out: Vec<u8> = Vec::new();

        let err = run_setup(&mut input, &mut out, &server.base, &gateway_dir).unwrap_err();
        let rendered = err.to_string();

        assert_eq!(rendered, "token was not accepted by Telegram");
        assert!(!rendered.contains("BAD-SECRET-TOKEN-999"), "{rendered}");
        assert!(
            !String::from_utf8(out)
                .unwrap()
                .contains("BAD-SECRET-TOKEN-999"),
            "the token must never be echoed to output"
        );
        assert!(
            !gateway_dir.join("gateway.yml").exists(),
            "a rejected token must not write any config"
        );
    }

    #[test]
    fn setup_times_out_cleanly_without_writing_config() {
        let state = MockState::default();
        state.set_response(
            "getMe",
            serde_json::json!({ "ok": true, "result": { "username": "my_onebrain_bot" } }),
        );
        state.set_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [] }),
        );
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let mut input = Cursor::new(b"123456:ABCDEF-token\n".to_vec());
        let mut out: Vec<u8> = Vec::new();

        let err = run_setup(&mut input, &mut out, &server.base, &gateway_dir).unwrap_err();
        let rendered = err.to_string();

        assert!(
            rendered.contains("timed out") || rendered.contains("START"),
            "{rendered}"
        );
        assert!(
            !gateway_dir.join("gateway.yml").exists(),
            "a timeout must not write any config"
        );
        assert_eq!(
            state.calls("getUpdates"),
            (MAX_WAIT_SECS / WAIT_POLL_SECS) as usize,
            "must poll exactly the budgeted number of times, no more, no fewer"
        );
    }
}
