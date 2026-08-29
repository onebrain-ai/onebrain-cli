//! Telegram approval channel. Gateway PR 5, Task 2 gave this module the
//! config-availability gate ([`is_available`]) and the API host resolver
//! ([`api_base`]); Task 4 gives it the actual send/outcome delivery flow
//! ([`TelegramChannel`]) built on top of [`super::telegram_api::BotApi`].
//! Task 5 (not yet landed) still owns the OTHER half: long-polling
//! `getUpdates` for a human's button press and calling
//! [`super::approval::Approvals::resolve`] with [`super::approval::ResolvedVia::Telegram`]
//! — this module only SENDS the prompt and EDITS it once an answer arrives
//! from whatever channel actually answered, native/HTTP/Telegram alike.
//!
//! ## `is_available`: configured-ness, not liveness
//! [`is_available`] answers a purely local question — does `gateway.yml`
//! carry both a `telegram.bot_token` and a `telegram.chat_id`, and has the
//! channel not been explicitly disabled? It never calls Telegram. See
//! [`is_available`]'s own doc comment, and
//! [`super::server::approval_channels`]'s `telegram` bullet (the caller
//! that turns this into the `capabilities` tool's client-facing report),
//! for the full rationale.
//!
//! ## `api_base`: the one place the default host lives
//! [`api_base`] resolves the Telegram Bot API base URL every
//! [`super::telegram_api::BotApi`] construction in this crate uses,
//! [`TelegramChannel::new`] included. The literal
//! `"https://api.telegram.org"` string appears in ACTUAL CODE exactly once
//! — inside this function — so nothing else in this crate can drift from
//! it (`telegram_api.rs`'s own doc comment mentions the same host, but only
//! as a `///` example, never as a value the code constructs).
//!
//! ## `TelegramChannel`: fire-and-forget by construction
//! [`TelegramChannel::fire`] and [`TelegramChannel::note_outcome`] are
//! called directly from `server::await_approval`'s hot path — the same
//! function that also `.await`s the human decision itself — so NEITHER may
//! ever block the caller on real network I/O. Both hand the actual
//! blocking `ureq` call off to [`tokio::task::spawn_blocking`] and return
//! immediately, exactly like [`super::approval_native::prompt`] already
//! does for the native dialog channel; the returned `JoinHandle` is
//! deliberately dropped, not awaited (same precedent). A delivery failure
//! (a network error, a bad token, Telegram itself being down) is a
//! `tracing::warn!`, never a propagated `Result` — nothing here can fail
//! the tool call it happens to run alongside, since the approval already
//! stands or falls on its own `Approvals::wait` timeout independent of
//! whether a human ever SAW a Telegram prompt for it.
//!
//! `sent`/`api` are `Arc`-wrapped internally (not just the type as a whole)
//! specifically so `fire`/`note_outcome` — both plain `&self` methods, per
//! `server::await_approval`'s call site (`&state.telegram`, not an owned
//! handle) — can clone an owned, `'static` handle for their own
//! `spawn_blocking` closure without needing `Arc<Self>` as the receiver
//! type.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::approval::PendingApproval;
use super::auth::core::now_epoch_secs;
use super::config::TelegramConfig;
use super::telegram_api::BotApi;

/// Presence switch (see [`super::env_switch_on`]'s own doc comment for the
/// shared semantics: non-empty value = on, empty = unset) that force-disables
/// the Telegram approval channel regardless of what `gateway.yml` configures
/// — the Telegram sibling of
/// [`super::approval_native::DISABLE_NATIVE_APPROVAL_ENV`], for the same
/// reason: a test harness (or an operator) that needs to prove a code path
/// behaves correctly with the channel unavailable, without having to unset
/// real credentials from `gateway.yml` to do it.
pub const DISABLE_TELEGRAM_APPROVAL_ENV: &str = "ONEBRAIN_GATEWAY_DISABLE_TELEGRAM_APPROVAL";

/// Env var that overrides the Telegram Bot API base URL. Overriding this is
/// how this crate's own tests point [`super::telegram_api::BotApi`] at a
/// local mock server instead of the real `https://api.telegram.org` — see
/// `telegram_api.rs`'s `MockServer` fixture, which passes its own base
/// straight to `BotApi::new` today; a later task that builds `BotApi`
/// through [`api_base`] instead gets the same override for free via this
/// var.
pub const TELEGRAM_API_BASE_ENV: &str = "ONEBRAIN_TELEGRAM_API_BASE";

/// Default Telegram Bot API host. Appears in code EXACTLY here — see the
/// module docs' "the one place the default host lives" section.
const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// Telegram's own hard limit on `callback_data`: 1–64 bytes. Referenced by
/// [`TelegramChannel::fire`]'s `debug_assert!` (Task 4 review, F3) — see
/// that call site for why this needs enforcing at all, not just documenting.
const CALLBACK_DATA_MAX_BYTES: usize = 64;

/// `true` iff the Telegram approval channel is CONFIGURED on this running
/// gateway process: `cfg.bot_token` is non-empty, `cfg.chat_id` is
/// non-zero, and [`DISABLE_TELEGRAM_APPROVAL_ENV`] is not set.
///
/// This reports configured-ness, not liveness — it never makes a network
/// call. Whether the configured token is actually VALID (a live `getMe`
/// call against the real Telegram API) is a setup-time concern for a later
/// task's pairing wizard, not something re-checked on every call here: a
/// wizard validates once, at setup, when a human is already watching and
/// can act on a bad-token error immediately; re-probing Telegram on every
/// `capabilities` call (this function's only caller today, via
/// [`super::server::approval_channels`]) would add real latency, and a real
/// failure mode — a transient Telegram outage — to a field every other
/// [`super::server::ApprovalChannels`] field answers from purely local
/// state. Mirrors [`super::approval_native::is_available`]'s own
/// local-state-only shape for the same reason.
pub fn is_available(cfg: &TelegramConfig) -> bool {
    !super::env_switch_on(DISABLE_TELEGRAM_APPROVAL_ENV)
        && !cfg.bot_token.is_empty()
        && cfg.chat_id != 0
}

/// Telegram Bot API base URL: [`TELEGRAM_API_BASE_ENV`] when set to a
/// non-empty value, else [`DEFAULT_API_BASE`]. A set-but-empty value counts
/// as unset (the same convention [`super::env_switch_on`] uses), so a
/// hook-managed env block can neutralize an override by blanking it rather
/// than having to unset the key.
///
/// [`TelegramChannel::new`] is the real production caller (Gateway PR 5,
/// Task 4) — every [`super::telegram_api::BotApi`] this crate constructs
/// resolves its base through this function, so a test pointing
/// [`TELEGRAM_API_BASE_ENV`] at a local mock server (see this module's own
/// `MockServer` fixture) redirects that `BotApi` too, with no separate
/// override plumbed through `TelegramChannel::new`'s own signature.
pub fn api_base() -> String {
    std::env::var(TELEGRAM_API_BASE_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

/// One sent-and-not-yet-resolved Telegram approval prompt: the message
/// [`TelegramChannel::fire`] sent, remembered so
/// [`TelegramChannel::note_outcome`] can both find it (`message_id`, to
/// edit) and reconstruct its body (`text`, the SAME text `fire` sent —
/// `editMessageText` replaces the whole message text, so the edit has to
/// re-supply it, not just prepend the outcome). `expires` is
/// [`super::approval::PendingApproval::expires`] carried along verbatim —
/// the SAME TTL [`super::approval::Approvals::register`] used for this id
/// — purely so [`TelegramChannel::fire`]'s own sweep (see
/// [`TelegramChannel::sent`]'s doc comment) can tell a genuinely orphaned
/// entry from a live one without a second, independently-drifting source
/// of truth for when an approval expires.
struct Sent {
    message_id: i64,
    text: String,
    expires: u64,
}

/// The Telegram approval channel (Gateway PR 5, Task 4): sends an approval
/// prompt with inline Approve/Deny buttons ([`Self::fire`]) and edits it
/// once the approval resolves, from WHATEVER channel resolved it
/// ([`Self::note_outcome`]) — removing the buttons in the same edit (see
/// [`super::telegram_api::BotApi::edit_message_text`]'s own doc comment for
/// why that requires an explicit empty keyboard, not just omitting the
/// field). See the module docs' "`TelegramChannel`: fire-and-forget by
/// construction" section for the blocking/error-handling contract both
/// methods share.
///
/// Built once at startup, iff [`is_available`] — [`super::server::GatewayState::new`]
/// is the only production caller of [`Self::new`] — and held as
/// `Option<Arc<TelegramChannel>>` on [`super::server::GatewayState`] for the
/// lifetime of the running gateway process. It resolves ONE `chat_id`
/// (`gateway.yml`'s `telegram.chat_id`) — this crate has no notion of
/// per-approval Telegram routing.
pub struct TelegramChannel {
    chat_id: i64,
    api: Arc<BotApi>,
    /// Keyed by [`super::approval::PendingApproval::id`].
    ///
    /// This is a SWEEP-bounded guarantee, not a one-to-one one — an
    /// earlier revision of this doc claimed every entry is always removed
    /// by exactly one later [`Self::note_outcome`] call, which is false:
    /// `note_outcome` is reachable only from `server::await_approval`'s
    /// three `WaitOutcome` arms, and a CANCELLED tool-call future (a
    /// client disconnecting mid-wait) runs none of them — the exact path
    /// [`super::approval::Approvals::register`]'s own doc comment already
    /// names and defends against for `Approvals`'s own registry (it prunes
    /// expired entries before counting, for the identical reason). Without
    /// an equivalent sweep here, that cancelled call's entry — and the
    /// live Telegram buttons its `message_id` points at — would live for
    /// the rest of the process.
    ///
    /// [`Self::fire`] closes that gap: every successful insert first
    /// `retain`s out any entry whose OWN [`Sent::expires`] has already
    /// passed (mirroring `Approvals::register`'s identical precedent).
    /// That bounds this map at roughly
    /// [`super::approval::MAX_PENDING_APPROVALS`] concurrently pending
    /// entries plus one TTL window of not-yet-swept stragglers — never
    /// unbounded — at the cost of an orphan surviving up to one more
    /// `fire` call's worth of time past its own approval resolving. The
    /// SAME sweep also closes the send/resolve race for free: if `fire`'s
    /// `spawn_blocking` insert lands AFTER `note_outcome` already ran for
    /// the same id (found nothing, since the insert hadn't happened yet),
    /// the resulting orphan carries the SAME `expires` stamp and is swept
    /// on the next `fire` call, at no extra cost.
    sent: Arc<Mutex<HashMap<String, Sent>>>,
}

impl TelegramChannel {
    /// Builds the channel: a fresh [`BotApi`] pointed at [`api_base`] with
    /// `cfg.bot_token`, and an empty `sent` map. Cheap and side-effect-free
    /// — no network call happens here (mirrors [`BotApi::new`]'s own doc
    /// comment: it only builds the reusable `ureq` agent).
    pub fn new(cfg: &TelegramConfig) -> Self {
        Self {
            chat_id: cfg.chat_id,
            api: Arc::new(BotApi::new(&cfg.bot_token, &api_base())),
            sent: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Sends `pending` as a Telegram message with two inline buttons —
    /// `"✅ Approve"`/`"⛔ Deny"`, `callback_data` `"a:<id>"`/`"d:<id>"` (the
    /// wire shape Task 5's poller will parse back out) — and, on success,
    /// remembers the returned `message_id` (plus the exact text sent) in
    /// `sent` so [`Self::note_outcome`] can find and edit it later.
    ///
    /// The message text matches [`super::approval_native::build_dialog_script`]'s
    /// own framing (Task 4 review, F8): a `Client: {client_id}` line, a
    /// `Tool: {tool}` line, then a blank line, then `pending.summary`
    /// verbatim. A Telegram approver is very often the PRIMARY approver
    /// specifically because they're away from the machine the native
    /// dialog would pop on — they cannot see which connected client is
    /// asking unless this message says so, arguably the most
    /// security-relevant field on the whole prompt. `summary` alone is
    /// already bounded at registration
    /// ([`super::server::bounded_summary`], applied once in
    /// `server::await_approval` before `pending` is ever constructed — see
    /// that function's own doc comment for why re-bounding it here would be
    /// redundant); `client_id`/`tool` add only a small, fixed amount on top
    /// of that bound, nowhere near Telegram's 4096-character message limit.
    ///
    /// Never blocks the caller and never returns a `Result` — see the
    /// module docs' "fire-and-forget by construction" section. A send
    /// failure is a `tracing::warn!` and `sent` simply never gains an entry
    /// for this id, which makes a later [`Self::note_outcome`] call for the
    /// same id a harmless no-op (nothing to edit — there was never a
    /// message to edit in the first place).
    pub fn fire(&self, pending: &PendingApproval) {
        let api = Arc::clone(&self.api);
        let sent = Arc::clone(&self.sent);
        let chat_id = self.chat_id;
        let id = pending.id.clone();
        let expires = pending.expires;
        let text = format!(
            "Client: {}\nTool: {}\n\n{}",
            pending.client_id, pending.tool, pending.summary
        );
        tokio::task::spawn_blocking(move || {
            let approve_data = format!("a:{id}");
            let deny_data = format!("d:{id}");
            // Task 4 review, F3: Telegram hard-caps `callback_data` at 64
            // bytes. `mint_secret_32` ids keep today's payload (`"a:"` + 43
            // chars = 45 bytes) comfortably under that, but nothing else
            // enforces the relationship — a future id-generator change
            // could silently blow past it and take this whole channel
            // offline in production while `capabilities` keeps reporting
            // `telegram: true`. Same shape as `telegram_api.rs`'s own
            // `LONG_POLL_CEILING_SECS` `debug_assert!` precedent: fails
            // loudly in tests/debug builds instead of silently in release.
            debug_assert!(
                approve_data.len() <= CALLBACK_DATA_MAX_BYTES
                    && deny_data.len() <= CALLBACK_DATA_MAX_BYTES,
                "telegram callback_data exceeded the {CALLBACK_DATA_MAX_BYTES}-byte limit \
                 ({} / {} bytes)",
                approve_data.len(),
                deny_data.len()
            );
            let keyboard = [
                ("✅ Approve", approve_data.as_str()),
                ("⛔ Deny", deny_data.as_str()),
            ];
            match api.send_message(chat_id, &text, Some(&keyboard)) {
                Ok(message_id) => {
                    let mut sent = sent.lock().unwrap_or_else(|e| e.into_inner());
                    // Task 4 review, F1: sweep out anything already past
                    // its own TTL before inserting — see `Self::sent`'s own
                    // doc comment for why (a cancelled tool-call future
                    // runs none of `await_approval`'s `WaitOutcome` arms,
                    // so `note_outcome` would otherwise never run for that
                    // id) and for the bound this gives the map.
                    let now = now_epoch_secs();
                    sent.retain(|_, s| s.expires > now);
                    sent.insert(
                        id,
                        Sent {
                            message_id,
                            text,
                            expires,
                        },
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        approval_id = %short_id(&id),
                        "telegram: failed to send an approval prompt"
                    );
                }
            }
        });
    }

    /// Edits the Telegram message [`Self::fire`] sent for `approval_id`
    /// (if any) to show `outcome` — the exact caller-supplied string,
    /// followed by a blank line, followed by the ORIGINAL message text
    /// (client/tool/summary framing, as `fire` sent it), since
    /// `editMessageText` replaces the whole text rather than appending to
    /// it. Removes the entry from `sent` FIRST, so at most one edit is
    /// ever attempted per approval — a second call for the same
    /// `approval_id` (however that could happen; nothing in this crate
    /// calls it twice for one id today) finds nothing and is a silent
    /// no-op, same as a `fire` that never succeeded (or one whose entry
    /// [`Self::fire`]'s own sweep already reclaimed as an orphan).
    ///
    /// `outcome` is composed entirely by the caller
    /// (`server::await_approval`) — this method has no opinion on wording,
    /// it only interpolates. See that function's own doc comment for the
    /// three exact strings it passes.
    ///
    /// Never blocks the caller and never returns a `Result` — see the
    /// module docs' "fire-and-forget by construction" section. An edit
    /// failure is a `tracing::warn!`; the approval itself already resolved
    /// (or timed out) independent of whether this edit lands.
    pub fn note_outcome(&self, approval_id: &str, outcome: &str) {
        let removed = {
            let mut sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
            sent.remove(approval_id)
        };
        let Some(Sent {
            message_id, text, ..
        }) = removed
        else {
            return;
        };
        let api = Arc::clone(&self.api);
        let chat_id = self.chat_id;
        let id = approval_id.to_string();
        let edited_text = format!("{outcome}\n\n{text}");
        tokio::task::spawn_blocking(move || {
            if let Err(e) = api.edit_message_text(chat_id, message_id, &edited_text) {
                tracing::warn!(
                    error = %e,
                    approval_id = %short_id(&id),
                    "telegram: failed to edit an approval outcome message"
                );
            }
        });
    }
}

/// First 8 characters of an approval id — enough for an operator to
/// correlate a log line with `GET /approvals`'s own listing (matching a
/// prefix) without ever logging the WHOLE minted secret. Approval ids come
/// from `mint_secret_32` (Base64url, ASCII-only, so a byte-index slice
/// never lands mid-character) — the same value this crate's own CodeQL
/// guard already documents as never safe to interpolate whole into a log
/// line or assertion message (`auth/core.rs`'s `mint_secret_32` tests, Task
/// 4 review F6).
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> TelegramConfig {
        TelegramConfig {
            bot_token: "T".to_string(),
            chat_id: 5,
        }
    }

    /// The full truth table [`is_available`] promises: both fields must be
    /// set, AND the disable switch must be unset. Holds the crate-wide
    /// `test_env` lock across every assertion (mirrors
    /// `approval_native::is_available_is_true_on_macos_with_osascript_on_path`'s
    /// own guard) since this mutates
    /// [`DISABLE_TELEGRAM_APPROVAL_ENV`] twice.
    #[test]
    fn is_available_requires_both_fields_and_honors_the_disable_switch() {
        let _env = crate::test_env::set_var(DISABLE_TELEGRAM_APPROVAL_ENV, "");

        assert!(
            !is_available(&TelegramConfig::default()),
            "an empty bot_token and zero chat_id must not be available"
        );

        let empty_token = TelegramConfig {
            bot_token: String::new(),
            chat_id: 5,
        };
        assert!(
            !is_available(&empty_token),
            "an empty bot_token alone must not be available"
        );

        let zero_chat = TelegramConfig {
            bot_token: "T".to_string(),
            chat_id: 0,
        };
        assert!(
            !is_available(&zero_chat),
            "a zero chat_id alone must not be available"
        );

        assert!(
            is_available(&configured()),
            "a non-empty bot_token, non-zero chat_id, and unset disable switch must be available"
        );

        drop(_env);
        let _env = crate::test_env::set_var(DISABLE_TELEGRAM_APPROVAL_ENV, "1");
        assert!(
            !is_available(&configured()),
            "the disable switch must win even when both fields are set"
        );
    }

    /// [`api_base`]'s own env-override contract, independent of
    /// [`is_available`]'s. Holds the `test_env` lock since it mutates
    /// [`TELEGRAM_API_BASE_ENV`] twice.
    #[test]
    fn api_base_defaults_to_the_real_host_and_honors_the_env_override() {
        let _env = crate::test_env::set_var(TELEGRAM_API_BASE_ENV, "");
        assert_eq!(api_base(), DEFAULT_API_BASE);
        drop(_env);

        let _env = crate::test_env::set_var(TELEGRAM_API_BASE_ENV, "http://127.0.0.1:9");
        assert_eq!(api_base(), "http://127.0.0.1:9");
    }

    // ── TelegramChannel: fire / note_outcome (Gateway PR 5, Task 4) ───────
    //
    // Same mock-Bot-API-server pattern as `telegram_api.rs`'s own
    // `MockServer`/`MockState` (see that module's doc comment for the full
    // rationale: a real bound socket is needed because `BotApi` is a real
    // blocking `ureq` client, not something an in-process `axum::Router`
    // caller can drive). Duplicated here rather than shared — this crate's
    // existing precedent (`daemon_client.rs`'s own independent
    // `start_live_server` harness) is one private mock server per module
    // that needs one, not a shared test-only crate export.

    use axum::extract::{Path, State};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::Value;
    use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
    use std::time::{Duration, Instant};

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
            // Bounded for the same reason `telegram_api.rs`'s own copy of
            // this harness is: an unbounded wait here means a sandbox that
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

    /// Polls `state`'s recorded requests until at least `n` have arrived (up
    /// to ~2s). Needed because `fire`/`note_outcome` hand their real work
    /// off to `tokio::task::spawn_blocking` and return immediately — the
    /// same "wait for the async background call to actually happen" shape
    /// `server.rs`'s own `wait_for_one_pending` uses for the analogous
    /// reason.
    async fn wait_for_requests(state: &MockState, n: usize) -> Vec<(String, Value)> {
        for _ in 0..200 {
            let requests = state.requests();
            if requests.len() >= n {
                return requests;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("fewer than {n} request(s) arrived within the poll window");
    }

    /// A pending approval whose TTL is comfortably live — mirrors
    /// `approval_native.rs`'s and `approval.rs`'s own `sample()` fixtures.
    fn sample_pending(id: &str) -> PendingApproval {
        let now = crate::commands::gateway::auth::core::now_epoch_secs();
        PendingApproval {
            id: id.to_string(),
            client_id: "client-1".to_string(),
            tool: "brain_capture".to_string(),
            vault: Some("t1".to_string()),
            summary: "note: Quarterly Plan".to_string(),
            created: now,
            expires: now + 300,
            class: crate::commands::gateway::policy::RiskClass::Mutating,
        }
    }

    #[tokio::test]
    async fn fire_sends_one_bounded_message_with_approve_and_deny_buttons() {
        let state = MockState::default();
        state.set_response(
            "sendMessage",
            serde_json::json!({ "ok": true, "result": { "message_id": 4242 } }),
        );
        let server = MockServer::start(state.clone());
        let _env = crate::test_env::set_var(TELEGRAM_API_BASE_ENV, server.base.as_str());

        let channel = TelegramChannel::new(&configured());
        let pending = sample_pending("appr-1");
        channel.fire(&pending);

        let requests = wait_for_requests(&state, 1).await;
        let (method, body) = &requests[0];
        assert_eq!(method, "sendMessage");
        assert_eq!(body["chat_id"], 5, "{body}");
        let text = body["text"].as_str().unwrap_or_default();
        // Task 4 review, F8: matches the native dialog's own framing —
        // client and tool lines above the summary — so a Telegram approver
        // (very often the one AWAY from the machine the native dialog
        // would pop on) can see which connected client is asking.
        assert!(
            text.contains(&format!("Client: {}", pending.client_id)),
            "{body}"
        );
        assert!(text.contains(&format!("Tool: {}", pending.tool)), "{body}");
        assert!(text.contains(&pending.summary), "{body}");
        let buttons = body["reply_markup"]["inline_keyboard"][0]
            .as_array()
            .unwrap_or_else(|| panic!("no inline keyboard row: {body}"));
        assert_eq!(buttons.len(), 2, "{body}");
        assert_eq!(buttons[0]["text"], "✅ Approve", "{body}");
        assert_eq!(buttons[0]["callback_data"], "a:appr-1", "{body}");
        assert_eq!(buttons[1]["text"], "⛔ Deny", "{body}");
        assert_eq!(buttons[1]["callback_data"], "d:appr-1", "{body}");

        // The "one" in this test's own name: settle, then confirm `fire`
        // never sent a second message (nit, Task 4 review).
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(state.requests().len(), 1, "{:?}", state.requests());
    }

    /// Fires, then resolves twice — proves both the edit CONTENT (the
    /// outcome string plus the original summary, per `note_outcome`'s own
    /// doc comment) and the AT-MOST-ONCE guarantee (`sent` is drained by
    /// the first call, so the second is a no-op). Uses `"native"` in the
    /// outcome string (not `"telegram"`) specifically to pin that
    /// `note_outcome` interpolates whatever wording the caller gives it
    /// verbatim, rather than hardcoding its own — `server::await_approval`
    /// is the one place that actually composes the real wording, from
    /// WHICHEVER channel resolved the approval, not necessarily Telegram.
    #[tokio::test]
    async fn note_outcome_edits_the_original_message_and_only_once() {
        let state = MockState::default();
        state.set_response(
            "sendMessage",
            serde_json::json!({ "ok": true, "result": { "message_id": 77 } }),
        );
        let server = MockServer::start(state.clone());
        let _env = crate::test_env::set_var(TELEGRAM_API_BASE_ENV, server.base.as_str());

        let channel = TelegramChannel::new(&configured());
        let pending = sample_pending("appr-2");
        channel.fire(&pending);
        wait_for_requests(&state, 1).await;

        channel.note_outcome("appr-2", "✅ Approved via native");
        // `sent` is drained synchronously inside the call above (before it
        // ever spawns the edit), so this second call is a no-op it can
        // prove without waiting for anything async to happen first.
        channel.note_outcome("appr-2", "✅ Approved via native");

        let requests = wait_for_requests(&state, 2).await;
        assert_eq!(requests.len(), 2, "{requests:?}");
        let (method, body) = &requests[1];
        assert_eq!(method, "editMessageText");
        assert_eq!(body["chat_id"], 5, "{body}");
        assert_eq!(body["message_id"], 77, "{body}");
        let text = body["text"].as_str().unwrap_or_default();
        assert!(text.contains("Approved via native"), "{body}");
        assert!(
            text.contains(&pending.summary),
            "the edit must still carry the original summary text: {body}"
        );

        // Give any errant background task a moment to land, then confirm
        // the no-op second call above never produced a third request.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            state.requests().len(),
            2,
            "a second note_outcome for the same id must not edit again"
        );
    }

    /// A send that Telegram itself rejects (`ok:false`) must degrade to a
    /// warning, exactly like `approval_native::prompt`'s own failure modes:
    /// nothing panics, and — because `fire` never populated `sent` — the
    /// approval has nothing to edit later, so `note_outcome` for the same
    /// id is a silent no-op too.
    #[tokio::test]
    async fn a_send_failure_is_a_warning_not_an_error() {
        let state = MockState::default();
        state.set_response(
            "sendMessage",
            serde_json::json!({ "ok": false, "description": "boom" }),
        );
        let server = MockServer::start(state.clone());
        let _env = crate::test_env::set_var(TELEGRAM_API_BASE_ENV, server.base.as_str());

        let channel = TelegramChannel::new(&configured());
        channel.fire(&sample_pending("appr-3"));

        // Proves the blocking closure actually ran the failing call, not
        // just that nothing has happened yet.
        wait_for_requests(&state, 1).await;

        channel.note_outcome("appr-3", "✅ Approved via http");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            state.requests().len(),
            1,
            "a failed send must leave nothing to edit: {:?}",
            state.requests()
        );
    }
}
