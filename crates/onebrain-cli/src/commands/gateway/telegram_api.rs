//! Telegram Bot API client (Gateway PR 5, Task 1) — the from-scratch HTTP
//! client every later Gateway PR 5 task (approval delivery, callback
//! polling, message editing) is built on. No Telegram SDK crate exists in
//! this workspace (nor is one added here — zero new dependencies is a hard
//! constraint for this task); the whole surface is `ureq` POSTs against
//! `{base}/bot{token}/{method}` with hand-parsed `serde_json::Value`
//! bodies, following the same "no `json` feature, parse via `serde_json`
//! directly" convention `daemon_client.rs` already established for this
//! crate's other `ureq` client (see this workspace's root `Cargo.toml`,
//! the `# HTTP client` comment above the `ureq` dependency).
//!
//! ## The scrub chokepoint
//! [`TgError`] wraps a private, ALREADY-SCRUBBED `String` and has exactly
//! ONE constructor: the private [`scrub`] function. Every failure this
//! module can produce — a genuine transport failure from `ureq` (DNS,
//! connection refused, timeout, a non-2xx status when `ureq` itself
//! classifies one), a well-formed Telegram `{"ok":false,"description":…}`
//! response, or a malformed/missing-field response — is routed through
//! `scrub(method, &ureq::Error)` before it ever becomes a `TgError`. The
//! bot token (and, less critically, the base URL) must never end up in a
//! log line or a test-assertion failure message, so `scrub` deliberately
//! matches only a fixed, hardcoded set of `ureq::Error` variants — never
//! `ureq::Error`'s own `Display`, which for variants like `BadUri` or
//! `RequireHttpsOnly` embeds the offending URI verbatim (that alone would
//! defeat the whole point of having a chokepoint).
//!
//! An `ok:false` API response isn't itself a `ureq::Error` — Telegram
//! answers it as ordinary, successful HTTP — so [`BotApi::call`]
//! synthesizes one via [`synthetic`] (`ureq::Error::Other` around the
//! `description` text, token-stripped defensively) and hands THAT to
//! `scrub`. There is still only the one code path that ever builds a
//! `TgError`; `synthetic` never constructs `TgError` directly.
//!
//! ## Why `http_status_as_error(false)`
//! Telegram's real API sets a non-2xx HTTP status on most `ok:false`
//! responses, but nothing here should depend on that being reliable (and a
//! test mock is free to answer plain `200` with `{"ok":false,…}` — which is
//! exactly what this module's own tests do). Turning off `ureq`'s default
//! "non-2xx is an `Err`" behavior means [`BotApi::call`] always gets to
//! read the JSON body — success or failure — and inspect `ok` itself,
//! rather than losing `description` behind an opaque
//! `ureq::Error::StatusCode` on whichever responses happen to carry a
//! 4xx/5xx. Genuine transport failures (no response at all) still come
//! back as `Err(ureq::Error)` regardless of this setting.
//!
//! ## Long-poll timeout
//! [`BotApi::get_updates`] takes a Telegram long-poll `timeout` in seconds.
//! The agent's single [`HTTP_TIMEOUT`] is set a few seconds ABOVE
//! [`LONG_POLL_CEILING_SECS`] so a `getUpdates` call that's genuinely
//! waiting out Telegram's long-poll window doesn't get killed by this
//! client's own end-to-end timeout first — callers are expected to keep
//! `timeout_secs` at or below the ceiling.
//!
//! ## Dead-code allow
//! Same situation as [`super::auth`]'s own `#![allow(dead_code)]` (see that
//! module's doc comment for the full precedent chain): Task 1 gives
//! `BotApi` no external caller — `mod.rs` only adds `pub mod telegram_api;`
//! — so every method here is unreachable from `main` until a later Gateway
//! PR 5 task wires the poller/approval-delivery flow through it. The tests
//! below exercise every path, so none of it is untested dead code.
#![allow(dead_code)]

use std::fmt;
use std::time::Duration;

use serde_json::Value;

/// Long-poll ceiling in seconds: callers of [`BotApi::get_updates`] should
/// keep `timeout_secs` at or below this so [`HTTP_TIMEOUT`] never fires
/// before Telegram answers.
const LONG_POLL_CEILING_SECS: u32 = 35;

/// End-to-end timeout for every call this agent makes — a few seconds above
/// [`LONG_POLL_CEILING_SECS`] so a genuine long-poll wait never trips it.
const HTTP_TIMEOUT: Duration = Duration::from_secs(LONG_POLL_CEILING_SECS as u64 + 5);

/// A Telegram Bot API client. Holds the bot token and the API base URL
/// (both private — a `TgError` must never be able to echo either back, see
/// the module docs) plus a single reusable `ureq` agent.
pub struct BotApi {
    token: String,
    base: String,
    agent: ureq::Agent,
}

/// `getMe` result — just the field later tasks actually need.
pub struct BotIdentity {
    pub username: String,
}

/// One Telegram `Update`. Only two shapes are surfaced — a button press
/// ([`TgCallback`]) or a plain incoming message's `chat_id`/`from_id` —
/// because that's all the approval flow needs. Every other update kind
/// (edited messages, channel posts, chat-member changes, …) still comes
/// back as an entry with both fields `None`: its `update_id` must still be
/// SEEN by the caller to advance `getUpdates`' offset past it, so dropping
/// it here would make the poller re-fetch it forever.
pub struct TgUpdate {
    pub update_id: i64,
    pub callback: Option<TgCallback>,
    pub message_chat_id: Option<i64>,
    pub message_from_id: Option<i64>,
}

/// An inline-keyboard button press (`callback_query`). `chat_id` is
/// `Option` because Telegram omits the originating `message` (and so the
/// chat it lived in) when that message is too old or otherwise
/// inaccessible to the bot.
pub struct TgCallback {
    pub id: String,
    pub from_id: i64,
    pub chat_id: Option<i64>,
    pub data: String,
}

/// An already-scrubbed Telegram API error. See the module docs for the
/// single-constructor guarantee: this can only ever be built by [`scrub`].
pub struct TgError(String);

impl fmt::Display for TgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for TgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TgError {}

/// The single [`TgError`] constructor (see module docs). Matches a fixed
/// set of `ureq::Error` variants with a hardcoded, bounded rendering (kind
/// plus status only), never `ureq::Error`'s own `Display` — which for
/// variants like `BadUri`/`RequireHttpsOnly` embeds the offending URI
/// verbatim, and which is effectively open-ended anyway: a future `ureq`
/// upgrade could add a variant whose `Display` isn't safe to forward.
fn scrub(method: &str, e: &ureq::Error) -> TgError {
    let kind = match e {
        ureq::Error::StatusCode(code) => format!("status {code}"),
        ureq::Error::Timeout(_) => "timeout".to_string(),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        ureq::Error::Io(io_err) => format!("io: {}", io_err.kind()),
        // The one variant this module itself constructs (see `synthetic`):
        // an API-level `description` (already token-stripped) or a fixed
        // "malformed response"-style message. Both are strings this module
        // controls, so forwarding their `Display` is safe.
        ureq::Error::Other(inner) => inner.to_string(),
        _ => "request failed".to_string(),
    };
    TgError(format!("telegram {method} failed: {kind}"))
}

/// Builds a synthetic `ureq::Error::Other` around a message this module
/// already controls — an API-level `description` (already token-stripped
/// by the caller) or a fixed "malformed response" string — so
/// [`BotApi::call`] can route a non-transport failure through the SAME
/// `scrub` chokepoint a real `ureq::Error` goes through. See module docs.
fn synthetic(msg: impl Into<String>) -> ureq::Error {
    let msg: String = msg.into();
    let boxed: Box<dyn std::error::Error + Send + Sync> = msg.into();
    ureq::Error::Other(boxed)
}

/// Replaces any occurrence of `token` in `text` with a placeholder —
/// defense in depth for [`BotApi::call`]'s `description` path. Telegram's
/// own error descriptions never echo the caller's token, but this is cheap
/// insurance against a future Telegram behavior change or a misbehaving
/// mock/proxy sitting in front of it.
fn strip_token(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_string();
    }
    text.replace(token, "[redacted]")
}

impl BotApi {
    /// `base` ends WITHOUT a trailing slash, e.g.
    /// `"https://api.telegram.org"` (a trailing slash is trimmed
    /// defensively if given anyway, mirroring
    /// [`super::resolve_issuer`]'s own trim).
    pub fn new(token: &str, base: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            token: token.to_string(),
            base: base.trim_end_matches('/').to_string(),
            agent,
        }
    }

    /// `POST {base}/bot{token}/{method}` with a JSON body. Returns the
    /// envelope's `result` field on `ok:true`; every other outcome becomes
    /// a [`TgError`] via [`scrub`] (transport failures directly, `ok:false`
    /// via [`synthetic`]). See the module docs for the full rationale.
    fn call(&self, method: &str, body: Value) -> Result<Value, TgError> {
        let url = format!("{}/bot{}/{method}", self.base, self.token);
        let payload = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());

        let mut resp = self
            .agent
            .post(&url)
            .header("content-type", "application/json")
            .send(payload.as_str())
            .map_err(|e| scrub(method, &e))?;

        // `.read_to_string()` + `serde_json::from_str` rather than `ureq`'s
        // own `Body::read_json` convenience method — that method needs
        // `ureq`'s optional `json` feature, which this workspace
        // deliberately leaves off (see the module docs and this crate's
        // root `Cargo.toml`); it only appeared to work under default
        // feature unification with another workspace member. Mirrors
        // `daemon_client.rs`'s own `read_json` helper exactly.
        let body_text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| scrub(method, &e))?;
        let envelope: Value = serde_json::from_str(&body_text)
            .map_err(|_| scrub(method, &synthetic("malformed response body")))?;

        match envelope.get("ok").and_then(Value::as_bool) {
            Some(true) => Ok(envelope.get("result").cloned().unwrap_or(Value::Null)),
            _ => {
                let desc = envelope
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("request failed");
                let sanitized = strip_token(desc, &self.token);
                Err(scrub(method, &synthetic(sanitized)))
            }
        }
    }

    /// `getMe` — confirms the configured token is valid and surfaces the
    /// bot's own `@username`.
    pub fn get_me(&self) -> Result<BotIdentity, TgError> {
        let result = self.call("getMe", serde_json::json!({}))?;
        let username = result
            .get("username")
            .and_then(Value::as_str)
            .ok_or_else(|| scrub("getMe", &synthetic("missing username in response")))?
            .to_string();
        Ok(BotIdentity { username })
    }

    /// Long-polls `getUpdates`. `offset` is Telegram's own cursor — see
    /// [`TgUpdate`]'s docs for why the caller must advance it past every
    /// `update_id` returned, including entries with both fields `None`.
    pub fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u32,
    ) -> Result<Vec<TgUpdate>, TgError> {
        let mut body = serde_json::json!({ "timeout": timeout_secs });
        if let Some(o) = offset {
            body["offset"] = serde_json::json!(o);
        }
        let result = self.call("getUpdates", body)?;
        let items = result.as_array().cloned().unwrap_or_default();
        Ok(items.iter().map(parse_update).collect())
    }

    /// Sends a message, optionally with an inline keyboard rendered as ONE
    /// row (`keyboard = [(label, callback_data)]`). Returns the sent
    /// message's `message_id` — later tasks use it to edit/remove the
    /// keyboard once the approval resolves.
    pub fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        keyboard: Option<&[(&str, &str)]>,
    ) -> Result<i64, TgError> {
        let mut body = serde_json::json!({ "chat_id": chat_id, "text": text });
        if let Some(buttons) = keyboard {
            let row: Vec<Value> = buttons
                .iter()
                .map(|(label, data)| serde_json::json!({ "text": label, "callback_data": data }))
                .collect();
            body["reply_markup"] = serde_json::json!({ "inline_keyboard": [row] });
        }
        let result = self.call("sendMessage", body)?;
        result
            .get("message_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| scrub("sendMessage", &synthetic("missing message_id in response")))
    }

    /// Edits a previously sent message's text. No `reply_markup` is ever
    /// sent — this signature has no keyboard parameter, so any inline
    /// keyboard the original message carried is removed by the edit.
    pub fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> Result<(), TgError> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        });
        self.call("editMessageText", body)?;
        Ok(())
    }

    /// Acknowledges a button press — stops the client's "loading" spinner
    /// on the tapped button and optionally shows `text` as a toast.
    pub fn answer_callback_query(&self, callback_id: &str, text: &str) -> Result<(), TgError> {
        let body = serde_json::json!({
            "callback_query_id": callback_id,
            "text": text,
        });
        self.call("answerCallbackQuery", body)?;
        Ok(())
    }
}

/// Parses one raw `Update` JSON object into a [`TgUpdate`]. See that
/// struct's docs for why unrecognized update kinds still come back (with
/// both fields `None`) rather than being dropped.
fn parse_update(raw: &Value) -> TgUpdate {
    let update_id = raw.get("update_id").and_then(Value::as_i64).unwrap_or(0);

    if let Some(cb) = raw.get("callback_query") {
        let id = cb
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let from_id = cb
            .get("from")
            .and_then(|f| f.get("id"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let chat_id = cb
            .get("message")
            .and_then(|m| m.get("chat"))
            .and_then(|c| c.get("id"))
            .and_then(Value::as_i64);
        let data = cb
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return TgUpdate {
            update_id,
            callback: Some(TgCallback {
                id,
                from_id,
                chat_id,
                data,
            }),
            message_chat_id: None,
            message_from_id: None,
        };
    }

    if let Some(msg) = raw.get("message") {
        let chat_id = msg
            .get("chat")
            .and_then(|c| c.get("id"))
            .and_then(Value::as_i64);
        let from_id = msg
            .get("from")
            .and_then(|f| f.get("id"))
            .and_then(Value::as_i64);
        return TgUpdate {
            update_id,
            callback: None,
            message_chat_id: chat_id,
            message_from_id: from_id,
        };
    }

    TgUpdate {
        update_id,
        callback: None,
        message_chat_id: None,
        message_from_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::routing::post;
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
    use std::sync::{Arc, Mutex};

    /// Shared state for the mock Bot API server: canned per-method
    /// responses, plus every `(method, body)` it actually received — so a
    /// test can both script Telegram's answer AND assert on the exact
    /// request `BotApi` sent (the keyboard-row shape, chat/message ids,
    /// …).
    #[derive(Clone, Default)]
    struct MockState {
        responses: Arc<Mutex<HashMap<String, Value>>>,
        requests: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl MockState {
        fn set_response(&self, method: &str, body: Value) {
            self.responses
                .lock()
                .unwrap()
                .insert(method.to_string(), body);
        }

        fn requests(&self) -> Vec<(String, Value)> {
            self.requests.lock().unwrap().clone()
        }
    }

    /// Handles every `POST /{bot_and_token}/{method}` the mock server
    /// receives: records `(method, body)`, then answers with whatever
    /// [`MockState::set_response`] scripted for that method (or a bare
    /// `{"ok":true,"result":null}` if the test didn't care).
    async fn mock_handler(
        Path(params): Path<HashMap<String, String>>,
        State(state): State<MockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let method = params.get("method").cloned().unwrap_or_default();
        state.requests.lock().unwrap().push((method.clone(), body));
        let resp = state
            .responses
            .lock()
            .unwrap()
            .get(&method)
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "ok": true, "result": null }));
        Json(resp)
    }

    /// A mock Telegram Bot API server bound to an ephemeral `127.0.0.1`
    /// port. Mirrors `daemon_client.rs`'s own live-server test harness: a
    /// thread running a single-threaded runtime, `TcpListener::bind`ing
    /// `"127.0.0.1:0"`, and serving via `axum::serve` with a polled
    /// `AtomicBool` for graceful shutdown. `BotApi` is a real blocking
    /// `ureq` client, so its tests need an actual bound socket rather than
    /// `tower::ServiceExt::oneshot`'s in-process router driving
    /// (`server.rs`'s own fixtures use that, but it only works against
    /// direct `axum::Router` callers, not `ureq`).
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
            let bound = loop {
                let p = port.load(Ordering::SeqCst);
                if p != 0 {
                    break p;
                }
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

    #[test]
    fn send_message_returns_the_message_id_and_renders_one_keyboard_row() {
        let state = MockState::default();
        state.set_response(
            "sendMessage",
            serde_json::json!({ "ok": true, "result": { "message_id": 4242 } }),
        );
        let server = MockServer::start(state.clone());
        let api = BotApi::new("test-token-abc", &server.base);

        let msg_id = api
            .send_message(
                555,
                "Approve this?",
                Some(&[("✅ Approve", "a:X"), ("⛔ Deny", "d:X")]),
            )
            .unwrap();

        assert_eq!(msg_id, 4242);

        let requests = state.requests();
        assert_eq!(requests.len(), 1);
        let (method, body) = &requests[0];
        assert_eq!(method.as_str(), "sendMessage");
        assert_eq!(body["chat_id"], 555);
        assert_eq!(body["text"], "Approve this?");
        assert_eq!(
            body["reply_markup"]["inline_keyboard"],
            serde_json::json!([[
                { "text": "✅ Approve", "callback_data": "a:X" },
                { "text": "⛔ Deny", "callback_data": "d:X" }
            ]])
        );
    }

    #[test]
    fn get_updates_parses_callback_queries_and_ignores_other_update_kinds() {
        let state = MockState::default();
        state.set_response(
            "getUpdates",
            serde_json::json!({
                "ok": true,
                "result": [
                    {
                        "update_id": 100,
                        "callback_query": {
                            "id": "cb-1",
                            "from": { "id": 777 },
                            "message": { "chat": { "id": 888 } },
                            "data": "a:X"
                        }
                    },
                    {
                        "update_id": 101,
                        "message": {
                            "chat": { "id": 222 },
                            "from": { "id": 111 }
                        }
                    },
                    {
                        "update_id": 102,
                        "edited_channel_post": { "text": "irrelevant" }
                    }
                ]
            }),
        );
        let server = MockServer::start(state);
        let api = BotApi::new("test-token-abc", &server.base);

        let updates = api.get_updates(Some(50), 5).unwrap();

        assert_eq!(updates.len(), 3);

        assert_eq!(updates[0].update_id, 100);
        let cb = updates[0].callback.as_ref().expect("callback_query update");
        assert_eq!(cb.id, "cb-1");
        assert_eq!(cb.from_id, 777);
        assert_eq!(cb.chat_id, Some(888));
        assert_eq!(cb.data, "a:X");
        assert!(updates[0].message_chat_id.is_none());
        assert!(updates[0].message_from_id.is_none());

        assert_eq!(updates[1].update_id, 101);
        assert!(updates[1].callback.is_none());
        assert_eq!(updates[1].message_chat_id, Some(222));
        assert_eq!(updates[1].message_from_id, Some(111));

        assert_eq!(updates[2].update_id, 102);
        assert!(updates[2].callback.is_none());
        assert!(updates[2].message_chat_id.is_none());
        assert!(updates[2].message_from_id.is_none());
    }

    #[test]
    fn an_api_level_error_is_reported_without_the_token_or_url() {
        let state = MockState::default();
        state.set_response(
            "sendMessage",
            serde_json::json!({ "ok": false, "description": "Bad Request" }),
        );
        let server = MockServer::start(state);
        let api = BotApi::new("SECRETTOK123", &server.base);

        let err = api.send_message(1, "hi", None).unwrap_err();
        let rendered = err.to_string();

        assert!(rendered.contains("sendMessage"), "{rendered}");
        assert!(!rendered.contains("SECRETTOK123"), "{rendered}");
        assert!(!rendered.contains("http"), "{rendered}");
    }

    #[test]
    fn a_transport_error_is_reported_without_the_token_or_url() {
        // Port 1 is never listening locally — connect fails immediately
        // (`ECONNREFUSED`) with no server needed at all.
        let api = BotApi::new("SECRETTOK123", "http://127.0.0.1:1");

        let err = api.send_message(1, "hi", None).unwrap_err();
        let rendered = err.to_string();

        assert!(rendered.contains("sendMessage"), "{rendered}");
        assert!(!rendered.contains("SECRETTOK123"), "{rendered}");
        assert!(!rendered.contains("127.0.0.1:1"), "{rendered}");
    }
}
