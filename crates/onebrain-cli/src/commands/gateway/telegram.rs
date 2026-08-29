//! Telegram approval channel. Gateway PR 5, Task 2 gave this module the
//! config-availability gate ([`is_available`]) and the API host resolver
//! ([`api_base`]); Task 4 gave it the send/outcome delivery flow
//! ([`TelegramChannel::fire`]/[`TelegramChannel::note_outcome`]) built on top
//! of [`super::telegram_api::BotApi`]. Task 5 gives it the OTHER half — the
//! RECEIVE side ([`TelegramChannel::ensure_polling`]): a demand-driven
//! `getUpdates` long-poller that watches for a human's inline-button tap and
//! calls [`super::approval::Approvals::resolve`] with
//! [`super::approval::ResolvedVia::Telegram`] once one arrives.
//!
//! ## `ensure_polling`: one thread, spawned on demand, self-terminating
//! [`TelegramChannel::ensure_polling`] is a nudge, not a scheduler — every
//! [`TelegramChannel::fire`] call in `server::await_approval` calls it right
//! after firing the prompt (see that function's own doc comment). The first
//! call (per [`TelegramChannel`] instance, i.e. per running gateway process)
//! wins a `compare_exchange` on `polling` and spawns exactly ONE named OS
//! thread (`"tg-approval-poll"`); every later call — whether from a second
//! concurrent approval or from firing a second prompt while the first
//! poller is still up — loses the exchange and returns immediately, since
//! one poller already sees every callback for this bot's single configured
//! `chat_id` regardless of how many approvals are pending. The thread exits
//! on its own once a poll cycle observes no LIVE (non-expired)
//! [`super::approval::Approvals::list`] entries left to answer — see
//! [`poll_loop`]'s own doc comment for exactly what "live" excludes (Task 5
//! review, F3) — rather than running for the life of the process; because
//! each `getUpdates` call is a real `timeout`-second long poll
//! ([`POLL_TIMEOUT_SECS`] = **25 seconds**, the brief's own binding figure),
//! the thread can linger up to **25 seconds** past the LAST live approval
//! going away before it notices and exits. That lingering window is bounded
//! (never more than one long-poll cycle) and deliberate, not a leak: a
//! single named, self-terminating thread is easy to reason about and easy
//! to spot in a process listing, unlike a thread that either never exits or
//! gets leaked on every call. A per-cycle floor ([`MIN_CYCLE_INTERVAL`])
//! additionally paces any cycle that returns FASTER than that — an
//! instant-answering `getUpdates` (a misbehaving proxy, or simply a very
//! chatty chat) must never turn this loop into a hot spin; the floor never
//! makes the worst-case 25-second linger any worse, since it only adds
//! delay to cycles that already came back quickly. `polling` resets to
//! `false` through a `Drop` guard owned by the thread's own closure, so a
//! panic inside the poll loop can't wedge the flag and permanently block a
//! later `ensure_polling` call from ever spawning a replacement — see
//! [`poll_loop`]'s own doc comment for how that guard interacts with the
//! exit/reclaim race it also closes (Task 5 review, F1).
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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use sha2::{Digest, Sha256};

use super::approval::{Approvals, Decision, PendingApproval, ResolvedVia};
use super::auth::core::{base64url_nopad, now_epoch_secs};
use super::config::TelegramConfig;
use super::telegram_api::{BotApi, TgUpdate};

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

/// `getUpdates` long-poll `timeout` (seconds) [`TelegramChannel::ensure_polling`]
/// sends on every call — **25**, not the full 35-second ceiling
/// `telegram_api.rs`'s own `debug_assert!` enforces (Task 5 review, F6: an
/// earlier revision of this doc wrongly said "35" here). 25 leaves 10
/// seconds of headroom below `BotApi`'s own end-to-end `HTTP_TIMEOUT`
/// (ceiling + 5 = 40 seconds): a legitimate long-poll response that takes
/// slightly longer than the requested `timeout` to arrive (network
/// jitter, Telegram's own processing) still has room to complete before
/// `HTTP_TIMEOUT` would kill the connection out from under it, rather than
/// racing the two timeouts against each other with only 5 seconds between
/// them. Also chosen to minimize the request volume this channel puts on
/// the real Telegram API (a shorter timeout would mean more frequent empty
/// round trips while nothing is happening). Not configurable per call —
/// see [`TelegramChannel::ensure_polling`]'s own doc comment for why a
/// single hardcoded value is the right shape here.
const POLL_TIMEOUT_SECS: u32 = 25;

/// Backoff after a `getUpdates` transport/API error, so a Telegram outage
/// (or a misconfigured token) degrades to a slow retry rather than a tight
/// loop hammering the API once a second.
const POLL_ERROR_BACKOFF: Duration = Duration::from_secs(2);

/// Floor on how often a SUCCESSFUL poll cycle may repeat (Task 5 review,
/// F2): if a cycle's `getUpdates` call plus its update handling together
/// took less than this, [`poll_loop`] sleeps out the remainder before
/// starting the next cycle. `getUpdates` is a real long poll in
/// production, so a genuinely idle chat already paces itself at
/// [`POLL_TIMEOUT_SECS`]; this floor exists for the case that DOESN'T
/// pace itself — an instant-answering `getUpdates` (this module's own test
/// mock, before this fix, was exactly that: answering unscripted calls
/// immediately let a live poller spin as fast as the loopback round trip
/// allowed, confirmed the hard way as a genuine CPU-melting hang during
/// this task's own test development). An `Ok` cycle with zero updates is
/// still a completely ordinary, non-error outcome (see [`poll_loop`]'s own
/// doc comment on the Task 1 #6 empty-result-streak semantics) — this
/// floor paces it, it does not flag it as unusual.
const MIN_CYCLE_INTERVAL: Duration = Duration::from_secs(1);

/// `true` iff the Telegram approval channel is CONFIGURED on this running
/// gateway process: `cfg.bot_token` is non-empty, `cfg.chat_id` is
/// POSITIVE, and [`DISABLE_TELEGRAM_APPROVAL_ENV`] is not set.
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
///
/// **Positive, not merely non-zero** (Task 5 review, F4): Telegram's own id
/// convention gives private (one-to-one, human-to-bot) chats POSITIVE ids
/// and gives groups/supergroups/channels NEGATIVE ones — this channel's
/// whole design (see [`TelegramChannel`]'s own doc comment) resolves ONE
/// `chat_id` and treats a button press's `from_id` matching that SAME
/// number as the entire authorization check, which only makes sense for a
/// private chat with exactly one human on the other end. A `chat_id`
/// pointing at a group would silently configure a DEAD channel: prompts
/// would post into the group, but no individual group member's `from_id`
/// ever equals the group's own negative `chat_id`, so `handle_update`
/// would refuse every single button press as "not authorized" — with
/// `capabilities` still truthfully reporting `telegram: true` (the config
/// IS present) and nothing anywhere logging why it never actually works.
/// Rejecting `chat_id <= 0` here — before any of that — turns a silent,
/// hard-to-diagnose dead channel into an honest "not configured", the same
/// signal an operator already knows how to act on.
pub fn is_available(cfg: &TelegramConfig) -> bool {
    !super::env_switch_on(DISABLE_TELEGRAM_APPROVAL_ENV)
        && !cfg.bot_token.is_empty()
        && cfg.chat_id > 0
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
    /// `true` iff the [`Self::ensure_polling`] thread is currently up. Every
    /// call `compare_exchange`s this from `false` to `true` — the winner
    /// spawns the thread, every loser (this flag was already `true`) returns
    /// immediately. See [`Self::ensure_polling`]'s own doc comment for the
    /// full lifecycle, including the `Drop`-guard reset that keeps a panic
    /// from wedging this flag `true` forever.
    polling: Arc<AtomicBool>,
    /// [`token_key`] of `cfg.bot_token`, computed once here rather than
    /// re-derived on every poll cycle — see that function's own doc comment
    /// for why [`poll_loop`]'s persisted-offset file must be keyed by bot
    /// identity at all (Task 5 review, F9).
    token_key: String,
}

impl TelegramChannel {
    /// Builds the channel: a fresh [`BotApi`] pointed at [`api_base`] with
    /// `cfg.bot_token`, an empty `sent` map, and `polling` starting `false`
    /// (no poller thread up yet — [`Self::ensure_polling`] is what starts
    /// one). Cheap and side-effect-free — no network call happens here
    /// (mirrors [`BotApi::new`]'s own doc comment: it only builds the
    /// reusable `ureq` agent) — [`token_key`]'s own SHA-256 call is
    /// negligible next to that.
    pub fn new(cfg: &TelegramConfig) -> Self {
        Self {
            chat_id: cfg.chat_id,
            api: Arc::new(BotApi::new(&cfg.bot_token, &api_base())),
            sent: Arc::new(Mutex::new(HashMap::new())),
            polling: Arc::new(AtomicBool::new(false)),
            token_key: token_key(&cfg.bot_token),
        }
    }

    /// `true` iff this channel's poller thread is currently up — the SAME
    /// `polling` flag [`Self::ensure_polling`] itself gates on, exposed
    /// read-only and ONLY under `#[cfg(test)]`. `polling` is a private
    /// field for a reason (nothing outside this module has legitimate
    /// business inspecting it in production), but `server.rs`'s own tests
    /// — a DIFFERENT module, so ordinary Rust privacy does not already
    /// grant them access the way it does for this module's own `mod tests`
    /// — need a way to wait out a real poller thread before their test
    /// function returns (Task 5 review, F7). `pub(crate)` rather than
    /// fully `pub`: this crate's own tests are the only legitimate caller,
    /// and the whole point is that this is NOT part of the type's real
    /// public API.
    #[cfg(test)]
    pub(crate) fn is_polling(&self) -> bool {
        self.polling.load(Ordering::Acquire)
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

    /// Ensures the `getUpdates` long-poll thread is running, nudging one
    /// into existence if not. `server::await_approval` calls this
    /// unconditionally right after every [`Self::fire`] — see the module
    /// docs' "`ensure_polling`" section for the full one-thread-per-process,
    /// self-terminating lifecycle this method and its spawned thread
    /// implement together; this doc comment covers the mechanics.
    ///
    /// `compare_exchange(false, true, ..)` is the whole gate: this call wins
    /// iff `polling` was `false`, and only the winner spawns the thread
    /// (named `"tg-approval-poll"` via [`std::thread::Builder`], so it's
    /// identifiable in a process listing or a panic backtrace). Every other
    /// call — concurrent or sequential, it doesn't matter which — loses and
    /// returns immediately: one poller already watches every callback for
    /// this bot's single configured `chat_id`, so a second thread would only
    /// double the `getUpdates` traffic for the exact same updates.
    ///
    /// The spawned thread's loop, once per iteration:
    /// 1. `api.get_updates(offset, `[`POLL_TIMEOUT_SECS`]`)`. On `Err` — a
    ///    transport failure or a malformed/rejected response, already
    ///    scrubbed of any secret by [`BotApi`]'s own chokepoint — warns
    ///    ONCE per consecutive-error streak (not once per attempt; a real
    ///    Telegram outage would otherwise flood the log at the retry
    ///    cadence below) and sleeps [`POLL_ERROR_BACKOFF`] before looping
    ///    again. `Ok(updates)` resets the streak; an `Ok` with ZERO updates
    ///    is the ordinary, expected shape of "nothing happened this cycle"
    ///    (`telegram_api::BotApi::get_updates`'s own doc comment notes a
    ///    malformed `result` silently collapses to the same empty `Vec` a
    ///    genuinely quiet chat produces — this loop cannot and need not
    ///    tell the two apart, and treats a long streak of empty-but-`Ok`
    ///    cycles as completely normal, never a warning).
    /// 2. For every update returned (including ones [`handle_update`]
    ///    ignores or refuses): advances `offset` to `update_id + 1` and
    ///    persists it via [`persist_offset`] BEFORE handling it — see
    ///    [`super::telegram_api::TgUpdate`]'s own doc comment for why every
    ///    `update_id` must be acknowledged this way regardless of what kind
    ///    of update it turned out to be, or Telegram would keep re-sending
    ///    it forever. Then calls [`handle_update`] for the callback
    ///    validation/resolution flow — see that function's own doc comment
    ///    for the exact rules.
    /// 3. Decides whether to keep going — see [`poll_loop`]'s own doc
    ///    comment for the exact exit/reclaim logic (Task 5 review, F1 and
    ///    F3): no LIVE pending approval left means the loop, and the
    ///    thread, end; anything live loops back to step 1.
    ///
    /// The thread's closure owns a small `Drop` guard around `polling`
    /// (constructed first thing, before the loop) that resets the flag to
    /// `false` unconditionally when the closure returns OR unwinds — so a
    /// panic mid-loop still frees a later `ensure_polling` call to spawn a
    /// replacement, rather than leaving `polling` wedged `true` forever with
    /// no thread actually running to ever flip it back. See [`poll_loop`]'s
    /// own doc comment for how that guard is DISARMED on the one exit path
    /// where storing `false` here would be wrong (handing off to a
    /// freshly-spawned replacement thread that already owns `true`).
    pub fn ensure_polling(&self, approvals: Arc<Approvals>) {
        if self
            .polling
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let api = Arc::clone(&self.api);
        let chat_id = self.chat_id;
        let token_key = self.token_key.clone();
        let polling = Arc::clone(&self.polling);
        let spawned = std::thread::Builder::new()
            .name("tg-approval-poll".to_string())
            .spawn(move || poll_loop(&api, chat_id, &token_key, &approvals, polling));
        if let Err(e) = spawned {
            // Couldn't even start the thread (extremely unlikely — OS
            // resource exhaustion). No `Drop` guard ever ran to reset the
            // flag, since the closure it would have wrapped never got a
            // chance to run at all — reset it directly, or every later
            // approval on this process would find `polling` permanently
            // wedged `true` with no thread behind it.
            self.polling.store(false, Ordering::Release);
            tracing::warn!(error = %e, "telegram: failed to spawn the approval-poll thread");
        }
    }
}

/// The body of the [`TelegramChannel::ensure_polling`] thread — see that
/// method's own doc comment for the full loop contract. A free function
/// (not a `TelegramChannel` method) because it only needs `api`/`chat_id`/
/// `token_key`/`approvals`, never `sent`, and taking exactly those
/// parameters makes it directly unit-testable without a full
/// `TelegramChannel` in scope.
///
/// ## The exit/reclaim race (Task 5 review, F1)
///
/// A naive "check [`Approvals::list`], then stop" exit has a lost-wakeup
/// window: this thread could decide there is nothing live left, but
/// before its `Drop` guard actually stores `polling = false`, a NEW
/// approval could register and call [`TelegramChannel::ensure_polling`] —
/// which finds `polling` still (momentarily) `true`, loses the
/// `compare_exchange`, and returns WITHOUT spawning a replacement. This
/// thread then finishes exiting and stores `false` anyway — and now
/// NOTHING is polling for the new approval's button press, dead until
/// some LATER, unrelated approval happens to nudge the poller again. This
/// is not a rare edge case: it opens every time a burst of approvals
/// settles down to the last one, ordinary use.
///
/// The fix below closes the window without a mutex around the whole
/// decision: when a cycle finds nothing live, it stores `polling = false`
/// ITSELF (rather than leaving that solely to the `Drop` guard at
/// function return) and immediately re-checks.
/// - Still nothing live: genuinely done. The `Drop` guard is disarmed (its
///   job is already done — an extra `store(false)` on top would be
///   harmless, but pointless) and the function returns.
/// - Something showed up in that exact gap: try to reclaim ownership via
///   this thread's OWN `compare_exchange(false, true, ..)`. Win (nobody
///   else touched the flag yet) → loop again, serving the new work from
///   the SAME thread. Lose (a concurrent `ensure_polling` call already
///   reclaimed the flag and is spawning its own replacement) → disarm the
///   `Drop` guard and return; the replacement thread now legitimately owns
///   `polling = true`, and this thread must NOT flip it back to `false`
///   out from under it.
///
/// Either way, no wakeup is lost, and `compare_exchange`'s own atomicity
/// means at most one thread can ever WIN ownership of `polling = true` at
/// a time — never two pollers running concurrently. A fully deterministic
/// reproduction of the exact race window would need a test-only
/// synchronization hook inside this loop (to pause it mid-decision on
/// command); that was judged out of proportion to this fix and was not
/// added — the Task 5 review-round report states this explicitly rather
/// than leaving it implicit.
fn poll_loop(
    api: &BotApi,
    chat_id: i64,
    token_key: &str,
    approvals: &Approvals,
    polling: Arc<AtomicBool>,
) {
    /// Resets `polling` to `false` on drop — panic-safe, see
    /// [`TelegramChannel::ensure_polling`]'s own doc comment for why this
    /// must be unconditional (unwind included) rather than a plain
    /// end-of-loop store. `armed` starts `true` and is flipped `false`
    /// (via [`Self::disarm`]) on exactly the one exit path where storing
    /// `false` here would be wrong — see this function's own doc comment,
    /// "The exit/reclaim race".
    struct ReleaseOnDrop {
        flag: Arc<AtomicBool>,
        armed: bool,
    }
    impl ReleaseOnDrop {
        fn disarm(&mut self) {
            self.armed = false;
        }
    }
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            if self.armed {
                self.flag.store(false, Ordering::Release);
            }
        }
    }
    let mut release = ReleaseOnDrop {
        flag: Arc::clone(&polling),
        armed: true,
    };

    let mut offset = read_offset(token_key);
    let mut error_streak: u32 = 0;

    loop {
        let cycle_start = Instant::now();

        match api.get_updates(offset, POLL_TIMEOUT_SECS) {
            Ok(updates) => {
                error_streak = 0;
                // Batch-persist ONCE per `getUpdates` call (Task 5 review,
                // F2), not once per update inside it: an unauthenticated
                // stranger spamming this bot's DM in one burst would
                // otherwise drive one disk write per message, unbounded by
                // anything this loop controls. Safe regardless of where a
                // crash lands relative to this write, because `Approvals`
                // is entirely in-memory and per-process (see that type's
                // own doc comment) — a crash-and-restart always starts
                // EVERY approval fresh anyway, so replaying an
                // already-handled update against a freshly-empty registry
                // on restart is a harmless no-op (`Approvals::resolve`
                // just returns `false`, answered as the ordinary "too
                // late" case `handle_update` already handles). A crash
                // between `persist_offset` and the next cycle would only
                // ever mean "a few already-seen updates get re-fetched and
                // re-handled on restart" — never "an approval gets
                // resolved twice" or "an offset write races a decision
                // that depends on it" — so batching the write is a pure
                // efficiency win with no new crash-window hazard.
                let new_offset = updates.last().map(|u| u.update_id + 1);
                for update in updates {
                    handle_update(api, chat_id, approvals, update);
                }
                if let Some(o) = new_offset {
                    persist_offset(token_key, o);
                    offset = Some(o);
                }

                // Cycle floor (Task 5 review, F2): a cycle that came back
                // in under a second didn't really long-poll — pace it so
                // an instant-answering `getUpdates` can never turn this
                // into a hot loop (this module's own test mock, before
                // this fix, was exactly that kind of instant-answering
                // peer, and reproduced the hazard for real: a poller
                // spinning as fast as the loopback round trip allowed).
                // Never fires on a genuine ~25s long poll (elapsed already
                // exceeds the floor by then), so this can only ever ADD
                // delay to a cycle that was already fast, never lengthen
                // one that was already slow — the worst-case linger this
                // module's own docs promise stays bounded at
                // `POLL_TIMEOUT_SECS`.
                if let Some(remaining) = MIN_CYCLE_INTERVAL.checked_sub(cycle_start.elapsed()) {
                    std::thread::sleep(remaining);
                }
            }
            Err(e) => {
                if error_streak == 0 {
                    tracing::warn!(error = %e, "telegram: getUpdates failed; retrying");
                }
                error_streak = error_streak.saturating_add(1);
                std::thread::sleep(POLL_ERROR_BACKOFF);
            }
        }

        if no_live_approvals(approvals) {
            // See this function's own doc comment, "The exit/reclaim
            // race": relinquish `polling` BEFORE the final recheck, not
            // after, so a concurrent `ensure_polling` racing this exact
            // moment can correctly tell whether it actually needs to spawn
            // a replacement.
            polling.store(false, Ordering::Release);
            if no_live_approvals(approvals) {
                release.disarm();
                return;
            }
            if polling
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                // Someone else's `ensure_polling` call already reclaimed
                // the flag and is spawning its own replacement — hand off.
                // Our own `Drop` guard must not store `false` behind that
                // thread's back.
                release.disarm();
                return;
            }
            // Reclaimed it ourselves — fall through and loop again.
        }
    }
}

/// `true` iff [`Approvals::list`] has no entry whose TTL hasn't already
/// elapsed (Task 5 review, F3). Plain `Approvals::list().is_empty()` is
/// NOT the right exit signal on its own: [`Approvals::register`] only
/// prunes expired entries lazily, on the NEXT `register` call — an
/// abandoned approval (the tool call that registered it was cancelled, so
/// nothing ever calls [`Approvals::wait`] for it either, per that method's
/// own doc comment on cancelled futures) sits in the registry, visible to
/// `list()`, until some UNRELATED later `register` call happens to sweep
/// it. Without this filter, [`poll_loop`]'s exit check could see that one
/// stale, un-prunable entry forever and never exit — directly
/// contradicting the module doc's "bounded... not a leak" claim for the
/// poller thread itself.
fn no_live_approvals(approvals: &Approvals) -> bool {
    let now = now_epoch_secs();
    approvals.list().iter().all(|p| p.expires <= now)
}

/// Handles one Telegram [`super::telegram_api::TgUpdate`] the poll loop
/// received — the callback validation and resolution rules Task 5's brief
/// specifies, in order:
///
/// 1. Not a `callback_query` (`update.callback` is `None`) — some other
///    update kind `getUpdates` still had to hand back to let the poller
///    advance past its `update_id` (see [`super::telegram_api::TgUpdate`]'s
///    own doc comment). Nothing to validate or resolve: ignored outright,
///    no log line, no Telegram API call.
/// 2. `callback.from_id != chat_id` — a button press from anyone other than
///    the one operator this bot is configured for (this channel resolves
///    ONE `chat_id`; see [`TelegramChannel`]'s own doc comment). Answered
///    with `"not authorized"` (so the tapper's client stops showing a
///    spinner) and logged at `warn` — using [`short_id`] on the raw
///    `callback.data` (which embeds the approval id after its `"a:"`/`"d:"`
///    prefix), never the whole string, matching this crate's "no secret or
///    full id in a log line" rule. The registry is untouched: this is
///    refused before any `Approvals::resolve` call is even considered.
/// 3. `callback.data` doesn't parse as `"a:<id>"`/`"d:<id>"`
///    ([`parse_callback_data`]) — answered `"unrecognized"` and logged at
///    `warn`, same [`short_id`]-only redaction as step 2.
/// 4. Valid `from_id` and parseable `data`: calls
///    [`super::approval::Approvals::resolve`] with the parsed
///    [`Decision`] and `via: `[`ResolvedVia::Telegram`]. `true` (this
///    poller's own answer was first) answers the button with a bare `"✅"`
///    or `"⛔"` matching the decision; `false` (already resolved by another
///    channel, or the approval's own TTL already expired and
///    [`Approvals::wait`] already cleaned it up) answers `"too late —
///    already decided or expired"` — the SAME first-response-wins semantics
///    every other resolution channel already relies on (see
///    [`Approvals::resolve`]'s own doc comment), so this poller never needs
///    to know or care whether it actually won.
///
/// Every `answer_callback_query` call's own result is deliberately ignored
/// (`let _ =`): it is a courtesy to the tapper's Telegram client (stopping
/// the button's loading spinner, showing a toast), never something the
/// approval's own correctness depends on — the resolution already happened
/// (or didn't) by the time this call is made, and a failure here has
/// nothing left to roll back.
fn handle_update(api: &BotApi, chat_id: i64, approvals: &Approvals, update: TgUpdate) {
    let Some(callback) = update.callback else {
        return;
    };

    if callback.from_id != chat_id {
        tracing::warn!(
            approval_id = %short_id(&callback.data),
            "telegram: callback from an unauthorized sender; ignoring"
        );
        let _ = api.answer_callback_query(&callback.id, "not authorized");
        return;
    }

    // Task 5 review, F4: `from_id` (checked above) is the real, sufficient
    // authorization boundary — a legitimate button press's OWN sender
    // matching the configured `chat_id` (this channel is a private,
    // one-human chat; see `TelegramChannel`'s own doc comment). This
    // `debug_assert!` is not a second security check on top of that — it's
    // a defensive internal-consistency check that forecloses a future
    // regression where this channel's messages start getting fired to the
    // wrong chat (a bug that would otherwise surface only as confused
    // operators, not a loud failure). Deliberately tolerant of `None`:
    // `TgCallback::chat_id` is `Option` because Telegram itself omits the
    // originating message (and so its chat id) when that message has
    // become too old or otherwise inaccessible — a legitimate, if stale,
    // button press can carry `chat_id: None`, and asserting equality
    // against it unconditionally would panic on that entirely normal case.
    debug_assert!(
        callback.chat_id.is_none_or(|c| c == chat_id),
        "telegram callback's own chat_id disagreed with the configured chat_id"
    );

    let Some((decision, id)) = parse_callback_data(&callback.data) else {
        tracing::warn!(
            approval_id = %short_id(&callback.data),
            "telegram: unrecognized callback_data; ignoring"
        );
        let _ = api.answer_callback_query(&callback.id, "unrecognized");
        return;
    };

    if approvals.resolve(id, decision, ResolvedVia::Telegram) {
        let symbol = match decision {
            Decision::Approve => "✅",
            Decision::Deny => "⛔",
        };
        let _ = api.answer_callback_query(&callback.id, symbol);
    } else {
        let _ = api.answer_callback_query(&callback.id, "too late — already decided or expired");
    }
}

/// Parses Telegram `callback_data` in the exact wire shape [`TelegramChannel::fire`]
/// mints (`"a:<id>"` for Approve, `"d:<id>"` for Deny — see that method's
/// own doc comment). `None` for anything else: an unknown prefix, a missing
/// id after the colon, or (with no callers in this crate, but theoretically
/// reachable from a stale/foreign button press) a stray callback this bot
/// never sent.
///
/// This is the security boundary between "arbitrary remote input Telegram
/// handed us" and "an id we'll pass to [`super::approval::Approvals::resolve`]"
/// — Task 5 review, F5 added `tests::parse_callback_data_table` as DIRECT
/// coverage of every malformed shape this function has to reject, rather
/// than relying only on the end-to-end poller tests to exercise it
/// incidentally.
fn parse_callback_data(data: &str) -> Option<(Decision, &str)> {
    if let Some(id) = data.strip_prefix("a:") {
        return (!id.is_empty()).then_some((Decision::Approve, id));
    }
    if let Some(id) = data.strip_prefix("d:") {
        return (!id.is_empty()).then_some((Decision::Deny, id));
    }
    None
}

// ── getUpdates offset persistence ───────────────────────────────────────
//
// `~/.onebrain/gateway/telegram-<token_key>.offset` — a single decimal
// `i64`, nothing else. Path derivation follows `gateway.yml`'s own
// convention exactly ([`crate::home::home_dir`], never `dirs::` — see that
// module's own doc comment for why: `dirs::home_dir()` ignores
// `%USERPROFILE%` on Windows, which would let a sandboxed test process
// reach past its tempdir into the real profile). Written 0600 / dir 0700,
// mirroring `audit.rs`'s own file-permission discipline (see that module's
// doc comment) — duplicated here rather than imported, per this crate's
// established "one private copy per module" convention (this module's own
// docs already do the same for its mock-server test fixture).

/// A short (8-character), non-reversible digest of a Telegram bot token —
/// the first 6 bytes of `SHA-256(token)`, base64url-encoded (reusing
/// [`super::auth::core::base64url_nopad`] rather than hand-rolling a
/// second encoder — zero new deps either way, since `sha2` is already a
/// workspace dependency `auth/core.rs` itself uses for PKCE). Used ONLY to
/// key [`offset_file_path`] by bot identity (Task 5 review, F9): without
/// this, the SAME `~/.onebrain/gateway/` directory would persist an offset
/// under a fixed filename regardless of WHICH bot token minted it. If an
/// operator ever rotates `gateway.yml`'s `telegram.bot_token` (a new
/// `@BotFather` token, or simply a different bot), `getUpdates`'s offset is
/// a cursor scoped to the bot that issued it — replaying the OLD bot's
/// high-water offset against the NEW bot's own update stream makes
/// Telegram return nothing (any offset at or above its current cursor
/// reads as "no updates"), silently killing the whole channel with no
/// warning anywhere: `capabilities` still reports `telegram: true`, prompts
/// still send, and every button press just... does nothing. Keying the
/// filename by token sidesteps this: a rotated token gets its own fresh
/// offset file (and therefore `None` → no offset → a normal cold start)
/// instead of inheriting a stale one.
///
/// Deliberately NOT reversible to the original token — never embed the
/// token itself, or anything an attacker could feasibly invert back to it,
/// in a filename that could end up in a log line, a backup listing, or a
/// support screenshot. 6 bytes of SHA-256 is far more collision resistance
/// than this purely-cosmetic key needs (it only has to disambiguate the
/// operator's own past tokens from each other, not resist a deliberate
/// attacker) while staying short enough for a readable filename.
fn token_key(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    base64url_nopad(&digest[..6])
}

/// `~/.onebrain/gateway/telegram-<token_key>.offset`. `Err` only if home
/// resolution itself fails (an unset `$HOME`/`%USERPROFILE%` with no OS
/// fallback) — callers treat that the same as "file absent".
fn offset_file_path(token_key: &str) -> Result<PathBuf> {
    let home = crate::home::home_dir().context("resolve home directory for telegram offset")?;
    Ok(home
        .join(".onebrain")
        .join("gateway")
        .join(format!("telegram-{token_key}.offset")))
}

/// Reads the persisted `getUpdates` offset for the bot identified by
/// `token_key` ([`token_key`]). `None` covers every reason there might not
/// be one to read: no prior run under THIS bot token (file never written),
/// the file was removed, its content doesn't parse as an `i64`, or home
/// resolution itself failed — all of these mean the same thing to
/// [`poll_loop`]'s caller: start `getUpdates` with no offset, exactly like
/// a fresh bot.
fn read_offset(token_key: &str) -> Option<i64> {
    let path = offset_file_path(token_key).ok()?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Persists `offset` to [`offset_file_path`] for the bot identified by
/// `token_key`. Infallible from the caller's view, same shape as
/// `audit::AuditLog::append` (see that method's own doc comment for the
/// rationale): a write failure here must never stop the poll loop — the
/// WORST case of a lost offset write is Telegram re-sending an
/// already-seen update on the next `getUpdates` call, which
/// [`handle_update`] handles safely either way (a stale `resolve` call for
/// an already-resolved id is just first-response-wins's normal `false`
/// case).
fn persist_offset(token_key: &str, offset: i64) {
    if let Err(e) = try_persist_offset(token_key, offset) {
        tracing::warn!(error = %e, "telegram: failed to persist the getUpdates offset");
    }
}

fn try_persist_offset(token_key: &str, offset: i64) -> Result<()> {
    let path = offset_file_path(token_key)?;
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&path)
        .with_context(|| format!("open telegram offset file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "could not re-assert 0600 on telegram offset file"
            );
        }
    }
    file.write_all(offset.to_string().as_bytes())
        .with_context(|| format!("write telegram offset file {}", path.display()))?;
    Ok(())
}

/// Create `dir` with owner-only (0700) permissions on Unix, re-asserting the
/// mode if it already existed with looser bits. Plain recursive create on
/// non-Unix. Mirrors `audit::ensure_private_dir` exactly (duplicated, not
/// imported — see this section's own header comment).
fn ensure_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create telegram offset dir {}", dir.display()))?;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(error = %e, path = %dir.display(), "could not re-assert 0700 on telegram offset dir");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create telegram offset dir {}", dir.display()))
    }
}

/// First 8 CHARACTERS (not bytes) of a string — enough for an operator to
/// correlate a log line with `GET /approvals`'s own listing (matching a
/// prefix) without ever logging the whole thing. Originally written for
/// approval ids alone (`mint_secret_32`'s Base64url, ASCII-only output, for
/// which a byte-index slice never lands mid-character) but Task 5 review
/// finding F8 caught that this module now ALSO feeds it `callback.data` —
/// remote, Telegram-controlled input that carries no such guarantee. The
/// original `id.get(..8)` byte-index slice fails closed on ASCII (safe) but
/// fails OPEN on a non-ASCII string whose byte 8 lands mid-character: `.get`
/// returns `None` for a non-boundary index, and the old `.unwrap_or(id)`
/// fallback logged the ENTIRE string in that case — for attacker-controlled
/// input, defeating the whole point of truncating at all. `.chars().take(8)`
/// has no such failure mode: it always stops at 8 CHARACTERS regardless of
/// how many bytes those characters occupy, so there is no boundary to land
/// on badly and no fallback path that can silently widen to the whole
/// string. Returns an owned `String` (not `&str`) since truncating by
/// `char` can only be expressed as a fresh allocation, not a sub-slice of
/// the original.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
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

        // Task 5 review, F4: a NEGATIVE chat_id is a group/supergroup/
        // channel id under Telegram's own convention, never a private
        // chat — this channel's whole design needs a private chat (see
        // `is_available`'s own doc comment), so this must be rejected the
        // same as zero, not silently accepted as "configured."
        let group_chat = TelegramConfig {
            bot_token: "T".to_string(),
            chat_id: -1_001_234_567_890,
        };
        assert!(
            !is_available(&group_chat),
            "a negative (group/channel) chat_id must not be available"
        );

        assert!(
            is_available(&configured()),
            "a non-empty bot_token, positive chat_id, and unset disable switch must be available"
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[derive(Clone, Default)]
    struct MockState {
        responses: Arc<Mutex<HashMap<String, Value>>>,
        /// One-shot, FIFO-per-method responses — consumed by the NEXT call
        /// to that method, falling back to [`Self::set_response`]'s static
        /// value (or the default) once drained. Gateway PR 5, Task 5's
        /// poller tests need this: `getUpdates` is called repeatedly by the
        /// SAME poll loop, and different calls need different answers (one
        /// callback, then empty forever after) — `set_response`'s single
        /// static value can't express that.
        queued: Arc<Mutex<HashMap<String, VecDeque<Value>>>>,
        requests: Arc<Mutex<Vec<(String, Value)>>>,
        /// Forces every call to a given method to sleep before answering —
        /// used by `ensure_polling_never_double_spawns` to make sure at
        /// least one real `getUpdates` round trip is actually in flight
        /// while the concurrency assertion below runs.
        delays: Arc<Mutex<HashMap<String, Duration>>>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
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

        fn set_delay(&self, method: &str, d: Duration) {
            self.delays.lock().unwrap().insert(method.to_string(), d);
        }

        fn requests(&self) -> Vec<(String, Value)> {
            self.requests.lock().unwrap().clone()
        }

        /// The highest number of requests this mock ever had in flight at
        /// once, across every method — the observable proxy
        /// `ensure_polling_never_double_spawns` uses to prove at most one
        /// `getUpdates` call is ever outstanding.
        fn max_in_flight(&self) -> usize {
            self.max_in_flight.load(Ordering::SeqCst)
        }
    }

    async fn mock_handler(
        Path(params): Path<HashMap<String, String>>,
        State(state): State<MockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let method = params.get("method").cloned().unwrap_or_default();

        let n = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        state.max_in_flight.fetch_max(n, Ordering::SeqCst);

        let delay = state.delays.lock().unwrap().get(&method).copied();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }

        state.requests.lock().unwrap().push((method.clone(), body));

        let queued = state
            .queued
            .lock()
            .unwrap()
            .get_mut(&method)
            .and_then(|q| q.pop_front());
        // `scripted` is bound to an owned `Option<Value>` (`.cloned()`) in
        // its OWN `let` statement, deliberately NOT inlined into the `match`
        // scrutinee below: a `MutexGuard` created directly in a `match`
        // scrutinee is kept alive for the ENTIRE match expression (temporary
        // lifetime extension), which would hold `state.responses`' lock
        // across the `.await` inside the `None` arm below — making this
        // future `!Send` (axum's `Handler` blanket impl requires `Send`,
        // which is how this bug first surfaced: a "Handler is not
        // implemented" error with no obvious cause) and, worse, an actual
        // held-lock-across-await hazard at runtime.
        let scripted = state.responses.lock().unwrap().get(&method).cloned();
        // Task 5 review, F2 ride-along: this used to sleep 50ms before
        // answering an UNSCRIPTED `getUpdates` call, standing in for real
        // Telegram long-poll pacing (this mock never actually waits for a
        // new update to exist, so nothing else paced it). That sleep is
        // now REDUNDANT: production's own `poll_loop` paces every
        // successful cycle to `MIN_CYCLE_INTERVAL` regardless of how fast
        // the peer answers, so a mock that answers instantly can no longer
        // turn a live poller into a hot loop — the production fix
        // subsumes the workaround. Removed rather than kept "for realism"
        // to avoid two independent knobs claiming to solve the same
        // problem.
        let resp = match queued {
            Some(v) => v,
            None => scripted.unwrap_or_else(|| serde_json::json!({ "ok": true, "result": null })),
        };

        state.in_flight.fetch_sub(1, Ordering::SeqCst);
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

    /// Bound every wait helper below against, in real elapsed time rather
    /// than a fixed iteration count — Task 5 review, F2 gave `poll_loop` a
    /// 1-second-per-cycle floor (`MIN_CYCLE_INTERVAL`), so a wait that
    /// needs to observe a SECOND poll cycle (not just the first) can take
    /// several real seconds even in the ordinary, non-flaky case. 20s is
    /// generous enough to comfortably cover a handful of floored cycles
    /// plus scheduling jitter on a loaded CI runner, while still failing a
    /// genuinely broken test in well under a minute.
    const WAIT_BOUND: Duration = Duration::from_secs(20);

    /// Polls `state`'s recorded requests until at least `n` have arrived.
    /// Needed because `fire`/`note_outcome` hand their real work off to
    /// `tokio::task::spawn_blocking` and return immediately — the same
    /// "wait for the async background call to actually happen" shape
    /// `server.rs`'s own `wait_for_one_pending` uses for the analogous
    /// reason.
    async fn wait_for_requests(state: &MockState, n: usize) -> Vec<(String, Value)> {
        let deadline = Instant::now() + WAIT_BOUND;
        loop {
            let requests = state.requests();
            if requests.len() >= n {
                return requests;
            }
            assert!(
                Instant::now() < deadline,
                "fewer than {n} request(s) arrived within the poll window: {requests:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits for the FIRST recorded request whose method is `method`,
    /// regardless of how many OTHER requests (e.g. background `getUpdates`
    /// polling with nothing to report) landed before or after it —
    /// Gateway PR 5, Task 5's poller tests script a specific method they
    /// care about without needing to predict every OTHER call's exact
    /// position among the full request log.
    async fn wait_for_request_method(state: &MockState, method: &str) -> Value {
        let deadline = Instant::now() + WAIT_BOUND;
        loop {
            if let Some((_, body)) = state.requests().into_iter().find(|(m, _)| m == method) {
                return body;
            }
            assert!(
                Instant::now() < deadline,
                "no {method} request arrived within the poll window: {:?}",
                state.requests()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits for `channel`'s [`TelegramChannel::ensure_polling`] thread to
    /// have actually exited — i.e. for [`TelegramChannel::is_polling`] to
    /// read `false` again — rather than merely for the thread's last
    /// OBSERVABLE side effect (an `answerCallbackQuery` call, say) to have
    /// landed.
    ///
    /// Polls the flag instead of `JoinHandle::join`ing the thread because
    /// there IS no handle to join: `ensure_polling` deliberately drops it
    /// (see that method's own doc comment — the thread is fire-and-forget
    /// by design, matching `fire`/`note_outcome`'s own dropped
    /// `spawn_blocking` handles). Polling `is_polling()` is still a
    /// faithful stand-in for "the thread is completely done," not merely
    /// "about to be": `polling`'s `false` store is the VERY LAST thing
    /// `poll_loop`'s `Drop` guard (or, on the reclaim path, the loop body
    /// itself) does before the thread function returns — nothing in this
    /// module ever observes `false` and then has more work left to do
    /// beyond an already-armed guard's own drop. Uses
    /// `TelegramChannel::is_polling` — a `#[cfg(test)]`-only accessor,
    /// since `polling` itself is a private field this module happens to
    /// share visibility into (being `mod tests`, a child module) but
    /// `server.rs`'s OWN tests (Task 5 review, F7) do not, hence the
    /// accessor existing at all.
    async fn wait_for_polling_false(channel: &TelegramChannel) {
        let deadline = Instant::now() + WAIT_BOUND;
        loop {
            if !channel.is_polling() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the telegram poller did not exit within the poll window"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Table test for [`parse_callback_data`] (Task 5 review, F5) — direct,
    /// mock-free coverage of the exact security boundary between "raw
    /// remote input" and "an id this module will pass to
    /// `Approvals::resolve`": every malformed shape must be rejected, and
    /// only the two well-formed shapes must parse, id-with-embedded-colon
    /// included (nothing about this format forbids a `:` inside the id
    /// itself — `strip_prefix` only ever looks at the FIRST one).
    #[test]
    fn parse_callback_data_table() {
        assert_eq!(
            parse_callback_data("a:appr-1"),
            Some((Decision::Approve, "appr-1"))
        );
        assert_eq!(
            parse_callback_data("d:appr-1"),
            Some((Decision::Deny, "appr-1"))
        );
        assert_eq!(parse_callback_data("x:appr-1"), None, "unknown prefix");
        assert_eq!(parse_callback_data("a:"), None, "empty id after the colon");
        assert_eq!(parse_callback_data("a"), None, "no colon at all");
        assert_eq!(parse_callback_data(""), None, "empty input");
        assert_eq!(
            parse_callback_data("a:has:embedded:colons"),
            Some((Decision::Approve, "has:embedded:colons")),
            "only the FIRST colon is a delimiter; the id may contain more"
        );
        let oversized = format!("a:{}", "x".repeat(10_000));
        assert_eq!(
            parse_callback_data(&oversized),
            Some((Decision::Approve, &oversized[2..])),
            "no length cap on this side — Telegram itself bounds callback_data"
        );
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

    // ── ensure_polling: the receive side (Gateway PR 5, Task 5) ──────────
    //
    // Every test below points `HOME`/`USERPROFILE` at its own tempdir
    // (`crate::test_env::set_vars`, same shape `server.rs`'s own Telegram
    // fixture uses) BEFORE ever calling `ensure_polling` — that method's
    // poller persists the `getUpdates` offset via `crate::home::home_dir`,
    // so without this every one of these tests would touch the developer's
    // or CI runner's REAL `~/.onebrain/gateway/telegram.offset`. Every test
    // also drives its pending approval to resolution before returning (via
    // the scripted callback itself, or an out-of-band `resolve` call) and
    // `.await`s [`wait_for_polling_false`] — the "no test may leave the
    // thread running after it ends" poller-discipline rule this brief
    // makes binding.

    /// A valid Approve button press — correct `from_id`, well-formed
    /// `"a:<id>"` data — resolves the SAME pending entry `Approvals::wait`
    /// is holding (proven via the `oneshot::Receiver` itself, not just
    /// `Approvals::list`), answers the callback with a bare `"✅"`, and
    /// advances the persisted offset past the update it consumed.
    #[tokio::test]
    async fn a_valid_approve_callback_resolves_the_pending_and_answers_the_button() {
        let state = MockState::default();
        let server = MockServer::start(state.clone());
        // ONE combined `set_vars` call — `crate::test_env`'s `ENV_LOCK` is a
        // plain, non-reentrant `std::sync::Mutex`, so two SEPARATE
        // `set_var`/`set_vars` calls on the same thread (one for
        // `TELEGRAM_API_BASE_ENV`, a second for `HOME`/`USERPROFILE`) would
        // have the second try to re-acquire the lock the first's still-live
        // guard already holds — an immediate self-deadlock (verified the
        // hard way: an earlier revision of these tests did exactly that,
        // and every one of them hung forever).
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            (TELEGRAM_API_BASE_ENV, server.base.as_str().as_ref()),
            ("HOME", home.path().as_os_str()),
            ("USERPROFILE", home.path().as_os_str()),
        ]);

        let approvals = Arc::new(Approvals::new());
        let pending = sample_pending("appr-poll-1");
        let rx = approvals.register(pending.clone()).unwrap();

        state.queue_response(
            "getUpdates",
            serde_json::json!({
                "ok": true,
                "result": [{
                    "update_id": 500,
                    "callback_query": {
                        "id": "cbq-1",
                        "from": { "id": 5 },
                        "message": { "chat": { "id": 5 } },
                        "data": "a:appr-poll-1"
                    }
                }]
            }),
        );

        let channel = TelegramChannel::new(&configured());
        channel.ensure_polling(approvals.clone());

        assert_eq!(
            rx.await.unwrap(),
            (Decision::Approve, ResolvedVia::Telegram),
            "the button press must resolve THIS pending approval's own receiver"
        );

        let ack = wait_for_request_method(&state, "answerCallbackQuery").await;
        assert_eq!(ack["callback_query_id"], "cbq-1", "{ack}");
        assert_eq!(ack["text"], "✅", "{ack}");

        wait_for_polling_false(&channel).await;

        // Task 5 review, F9: the offset file is keyed by a digest of the
        // bot token (`token_key`, a private function this test module can
        // call directly — same-module visibility), so the exact filename
        // depends on `configured()`'s own `bot_token` ("T").
        let offset_path = home
            .path()
            .join(".onebrain")
            .join("gateway")
            .join(format!("telegram-{}.offset", token_key("T")));
        let content = std::fs::read_to_string(&offset_path).unwrap_or_else(|e| {
            panic!("offset file not written at {}: {e}", offset_path.display())
        });
        assert_eq!(
            content.trim(),
            "501",
            "offset must advance past the consumed update_id (500 + 1): {content:?}"
        );
    }

    /// A callback whose `from_id` doesn't match the configured `chat_id` is
    /// refused before the registry is ever touched: answered `"not
    /// authorized"`, and the pending entry stays pending (proven via
    /// `Approvals::list`, not just "the wrong answer wasn't sent").
    #[tokio::test]
    async fn a_callback_from_the_wrong_chat_is_refused_and_resolves_nothing() {
        let state = MockState::default();
        let server = MockServer::start(state.clone());
        // ONE combined `set_vars` call — `crate::test_env`'s `ENV_LOCK` is a
        // plain, non-reentrant `std::sync::Mutex`, so two SEPARATE
        // `set_var`/`set_vars` calls on the same thread (one for
        // `TELEGRAM_API_BASE_ENV`, a second for `HOME`/`USERPROFILE`) would
        // have the second try to re-acquire the lock the first's still-live
        // guard already holds — an immediate self-deadlock (verified the
        // hard way: an earlier revision of these tests did exactly that,
        // and every one of them hung forever).
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            (TELEGRAM_API_BASE_ENV, server.base.as_str().as_ref()),
            ("HOME", home.path().as_os_str()),
            ("USERPROFILE", home.path().as_os_str()),
        ]);

        let approvals = Arc::new(Approvals::new());
        let pending = sample_pending("appr-poll-2");
        let _rx = approvals.register(pending.clone()).unwrap();

        state.queue_response(
            "getUpdates",
            serde_json::json!({
                "ok": true,
                "result": [{
                    "update_id": 600,
                    "callback_query": {
                        "id": "cbq-2",
                        "from": { "id": 999 },
                        "message": { "chat": { "id": 999 } },
                        "data": "a:appr-poll-2"
                    }
                }]
            }),
        );

        let channel = TelegramChannel::new(&configured());
        channel.ensure_polling(approvals.clone());

        let ack = wait_for_request_method(&state, "answerCallbackQuery").await;
        assert_eq!(ack["text"], "not authorized", "{ack}");

        assert_eq!(
            approvals.list().len(),
            1,
            "a wrong-chat callback must not resolve the pending entry"
        );

        // The pending entry was never actually answered — resolve it
        // out-of-band (as an operator using the HTTP channel instead
        // would) purely so the poller notices `Approvals::list` is empty
        // and exits, rather than leaving its thread running past this
        // test's own end.
        assert!(approvals.resolve(&pending.id, Decision::Deny, ResolvedVia::Http));
        wait_for_polling_false(&channel).await;
    }

    /// A callback that arrives AFTER the same approval was already resolved
    /// through another channel (first-response-wins, `Approvals::resolve`'s
    /// own contract) is answered "too late" rather than silently ignored or
    /// mis-reported as a fresh decision.
    #[tokio::test]
    async fn a_stale_callback_gets_too_late() {
        let state = MockState::default();
        let server = MockServer::start(state.clone());
        // ONE combined `set_vars` call — `crate::test_env`'s `ENV_LOCK` is a
        // plain, non-reentrant `std::sync::Mutex`, so two SEPARATE
        // `set_var`/`set_vars` calls on the same thread (one for
        // `TELEGRAM_API_BASE_ENV`, a second for `HOME`/`USERPROFILE`) would
        // have the second try to re-acquire the lock the first's still-live
        // guard already holds — an immediate self-deadlock (verified the
        // hard way: an earlier revision of these tests did exactly that,
        // and every one of them hung forever).
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            (TELEGRAM_API_BASE_ENV, server.base.as_str().as_ref()),
            ("HOME", home.path().as_os_str()),
            ("USERPROFILE", home.path().as_os_str()),
        ]);

        let approvals = Arc::new(Approvals::new());
        let pending = sample_pending("appr-poll-3");
        let _rx = approvals.register(pending.clone()).unwrap();
        assert!(approvals.resolve(&pending.id, Decision::Approve, ResolvedVia::Http));

        state.queue_response(
            "getUpdates",
            serde_json::json!({
                "ok": true,
                "result": [{
                    "update_id": 700,
                    "callback_query": {
                        "id": "cbq-3",
                        "from": { "id": 5 },
                        "message": { "chat": { "id": 5 } },
                        "data": "a:appr-poll-3"
                    }
                }]
            }),
        );

        let channel = TelegramChannel::new(&configured());
        channel.ensure_polling(approvals.clone());

        let ack = wait_for_request_method(&state, "answerCallbackQuery").await;
        assert_eq!(
            ack["text"], "too late — already decided or expired",
            "{ack}"
        );

        wait_for_polling_false(&channel).await;
    }

    /// The poller exits once its poll cycle observes no pending approvals
    /// left — and, critically, `polling` genuinely resets: a SECOND
    /// `ensure_polling` call afterward really spawns a new thread (proven
    /// by driving a second approval through it end to end), not merely
    /// reads `false` for one instant before something re-flips it.
    #[tokio::test]
    async fn the_poller_exits_when_no_approvals_remain() {
        let state = MockState::default();
        let server = MockServer::start(state.clone());
        // ONE combined `set_vars` call — `crate::test_env`'s `ENV_LOCK` is a
        // plain, non-reentrant `std::sync::Mutex`, so two SEPARATE
        // `set_var`/`set_vars` calls on the same thread (one for
        // `TELEGRAM_API_BASE_ENV`, a second for `HOME`/`USERPROFILE`) would
        // have the second try to re-acquire the lock the first's still-live
        // guard already holds — an immediate self-deadlock (verified the
        // hard way: an earlier revision of these tests did exactly that,
        // and every one of them hung forever).
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            (TELEGRAM_API_BASE_ENV, server.base.as_str().as_ref()),
            ("HOME", home.path().as_os_str()),
            ("USERPROFILE", home.path().as_os_str()),
        ]);

        let approvals = Arc::new(Approvals::new());
        let pending = sample_pending("appr-poll-4");
        let rx = approvals.register(pending.clone()).unwrap();

        let channel = TelegramChannel::new(&configured());
        channel.ensure_polling(approvals.clone());
        assert!(
            channel.is_polling(),
            "polling must already read true synchronously, right after ensure_polling returns"
        );

        assert!(approvals.resolve(&pending.id, Decision::Approve, ResolvedVia::Http));
        assert_eq!(rx.await.unwrap(), (Decision::Approve, ResolvedVia::Http));

        wait_for_polling_false(&channel).await;

        // Respawn: a second pending approval, resolved the same way, must
        // be picked up by a BRAND NEW poller thread.
        let pending2 = sample_pending("appr-poll-4b");
        let rx2 = approvals.register(pending2.clone()).unwrap();
        channel.ensure_polling(approvals.clone());
        assert!(
            channel.is_polling(),
            "a fresh ensure_polling call after the first thread exited must respawn one"
        );
        assert!(approvals.resolve(&pending2.id, Decision::Deny, ResolvedVia::Http));
        assert_eq!(rx2.await.unwrap(), (Decision::Deny, ResolvedVia::Http));
        wait_for_polling_false(&channel).await;
    }

    /// Two `ensure_polling` calls while a poller is already up must never
    /// result in two threads racing `getUpdates` — proven end to end via
    /// the mock's own observed concurrency (`MockState::max_in_flight`),
    /// not merely by inspecting `polling` (which the `compare_exchange`
    /// guard alone would already guarantee even if the SPAWNED thread body
    /// were buggy in some other way).
    #[tokio::test]
    async fn ensure_polling_never_double_spawns() {
        let state = MockState::default();
        // Widens the window a hypothetical double-spawn bug would need to
        // land a second, genuinely concurrent `getUpdates` inside.
        state.set_delay("getUpdates", Duration::from_millis(80));
        let server = MockServer::start(state.clone());
        // ONE combined `set_vars` call — `crate::test_env`'s `ENV_LOCK` is a
        // plain, non-reentrant `std::sync::Mutex`, so two SEPARATE
        // `set_var`/`set_vars` calls on the same thread (one for
        // `TELEGRAM_API_BASE_ENV`, a second for `HOME`/`USERPROFILE`) would
        // have the second try to re-acquire the lock the first's still-live
        // guard already holds — an immediate self-deadlock (verified the
        // hard way: an earlier revision of these tests did exactly that,
        // and every one of them hung forever).
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            (TELEGRAM_API_BASE_ENV, server.base.as_str().as_ref()),
            ("HOME", home.path().as_os_str()),
            ("USERPROFILE", home.path().as_os_str()),
        ]);

        let approvals = Arc::new(Approvals::new());
        let pending = sample_pending("appr-poll-5");
        let _rx = approvals.register(pending.clone()).unwrap();

        let channel = TelegramChannel::new(&configured());
        channel.ensure_polling(approvals.clone());
        // The loser: `polling` is already `true`, so this must return
        // immediately without spawning a second thread.
        channel.ensure_polling(approvals.clone());

        wait_for_requests(&state, 1).await;
        // Long enough to span a few of `poll_loop`'s own `MIN_CYCLE_INTERVAL`
        // (1s)-floored cycles, not just the first — the double-spawn
        // hazard this test guards against would show up on ANY cycle, not
        // only the first one.
        tokio::time::sleep(Duration::from_millis(3_500)).await;

        assert_eq!(
            state.max_in_flight(),
            1,
            "at most one getUpdates request may ever be in flight at once"
        );

        assert!(approvals.resolve(&pending.id, Decision::Approve, ResolvedVia::Http));
        wait_for_polling_false(&channel).await;
    }

    /// A persisted offset from a prior run is read at poller start and sent
    /// on the very first `getUpdates` call — proving Telegram's own
    /// replay-prevention semantics (server-side, driven by this `offset`)
    /// actually get the value they need from this process.
    #[tokio::test]
    async fn offset_survives_restart_and_prevents_replay() {
        let state = MockState::default();
        let server = MockServer::start(state.clone());
        // ONE combined `set_vars` call — `crate::test_env`'s `ENV_LOCK` is a
        // plain, non-reentrant `std::sync::Mutex`, so two SEPARATE
        // `set_var`/`set_vars` calls on the same thread (one for
        // `TELEGRAM_API_BASE_ENV`, a second for `HOME`/`USERPROFILE`) would
        // have the second try to re-acquire the lock the first's still-live
        // guard already holds — an immediate self-deadlock (verified the
        // hard way: an earlier revision of these tests did exactly that,
        // and every one of them hung forever).
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            (TELEGRAM_API_BASE_ENV, server.base.as_str().as_ref()),
            ("HOME", home.path().as_os_str()),
            ("USERPROFILE", home.path().as_os_str()),
        ]);

        // Task 5 review, F9: keyed by `token_key("T")` — `configured()`'s
        // own `bot_token` — matching production's own derivation exactly.
        let offset_path = home
            .path()
            .join(".onebrain")
            .join("gateway")
            .join(format!("telegram-{}.offset", token_key("T")));
        std::fs::create_dir_all(offset_path.parent().unwrap()).unwrap();
        std::fs::write(&offset_path, "999").unwrap();

        let approvals = Arc::new(Approvals::new());
        let pending = sample_pending("appr-poll-6");
        let _rx = approvals.register(pending.clone()).unwrap();

        let channel = TelegramChannel::new(&configured());
        channel.ensure_polling(approvals.clone());

        let first = wait_for_request_method(&state, "getUpdates").await;
        assert_eq!(
            first["offset"], 999,
            "the first getUpdates call after restart must carry the persisted offset: {first}"
        );

        assert!(approvals.resolve(&pending.id, Decision::Approve, ResolvedVia::Http));
        wait_for_polling_false(&channel).await;
    }

    /// Task 5 review, F3: an expired-but-not-yet-pruned entry must NOT keep
    /// the poller alive forever. `Approvals::register` only prunes expired
    /// entries lazily (on the NEXT `register` call, per that method's own
    /// doc comment) — an abandoned approval (its own tool call cancelled,
    /// so nothing ever calls `Approvals::wait` for it either) sits in the
    /// registry, visible to `Approvals::list`, with no further `register`
    /// call ever coming along to sweep it in this test. Before this fix,
    /// `poll_loop`'s exit check (`list().is_empty()`) would have seen that
    /// one stale entry forever and never exited.
    #[tokio::test]
    async fn an_expired_unpruned_entry_does_not_keep_the_poller_alive() {
        let state = MockState::default();
        let server = MockServer::start(state.clone());
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            (TELEGRAM_API_BASE_ENV, server.base.as_str().as_ref()),
            ("HOME", home.path().as_os_str()),
            ("USERPROFILE", home.path().as_os_str()),
        ]);

        let approvals = Arc::new(Approvals::new());

        // A SHORT-lived entry (expires ~1s from now) rather than one
        // constructed already-expired: `Approvals::register` prunes
        // existing entries BEFORE inserting a new one, so an
        // already-past-`expires` entry registered first would just get
        // swept the moment the SECOND `register` call below runs —
        // defeating this test's whole premise before the poller ever sees
        // it. A short-but-still-live TTL at registration time survives
        // that second `register` call (it hasn't expired YET), then
        // genuinely expires a moment later with no THIRD `register` call
        // ever coming along to prune it — a faithful stand-in for an
        // abandoned approval whose own tool call was cancelled.
        let now = now_epoch_secs();
        let mut stale = sample_pending("appr-poll-stale");
        stale.created = now;
        stale.expires = now + 1;
        let _stale_rx = approvals.register(stale).unwrap();

        let live = sample_pending("appr-poll-live");
        let live_rx = approvals.register(live.clone()).unwrap();

        let channel = TelegramChannel::new(&configured());
        channel.ensure_polling(approvals.clone());

        assert!(approvals.resolve(&live.id, Decision::Approve, ResolvedVia::Http));
        assert_eq!(
            live_rx.await.unwrap(),
            (Decision::Approve, ResolvedVia::Http)
        );

        // Precondition: the stale entry is still sitting in the registry
        // right after resolving `live` — nothing else has called
        // `register` to prune it (`Approvals::list` itself never prunes).
        assert_eq!(
            approvals.list().len(),
            1,
            "the not-yet-expired entry must still be sitting in the registry"
        );

        // Wait past the stale entry's own TTL WITHOUT ever calling
        // `register` again, so nothing but this fix could ever notice it's
        // gone. The poller's own first cycle or two will have already
        // observed it as still-live (its `expires` genuinely hadn't
        // passed yet) and looped instead of exiting — proving the fix
        // isn't a fluke of the entry never being checked at all.
        tokio::time::sleep(Duration::from_millis(1_500)).await;

        // Despite `list()` still reporting the stale entry, the poller
        // must exit: `no_live_approvals` filters it out by its own past
        // `expires`, not by `list()` being empty.
        wait_for_polling_false(&channel).await;
    }
}
