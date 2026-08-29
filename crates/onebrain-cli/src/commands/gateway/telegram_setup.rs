//! `onebrain gateway telegram setup` — Gateway PR 5, Task 6.
//!
//! A one-command wizard that turns a BotFather token into a fully wired
//! `gateway.yml` `telegram:` block: paste the token, send a short setup
//! code to the bot in Telegram, and this captures the resulting `chat_id`
//! and writes both fields to disk, then sends a confirmation message
//! through the freshly configured channel.
//!
//! [`run_setup`] is the testable core — every side effect (stdin/stdout,
//! the Telegram API base, the `~/.onebrain` directory) is a parameter, so
//! tests drive it against an in-memory buffer, a mock Bot API server, and a
//! tempdir with no env-var plumbing at all. [`telegram_setup`] is the thin
//! production wrapper `dispatch.rs` calls, which resolves the real values
//! and hands them to [`run_setup`].
//!
//! ## H1 (review finding) — identity is code-proven, not order-trusted
//! A Telegram bot's `@username` resolves publicly (`t.me/<username>`) the
//! instant BotFather assigns it. The FIRST implementation of this wizard
//! trusted ARRIVAL ORDER: it called `getUpdates` and captured the first
//! message-bearing update it saw. Two review rounds found that
//! insufficient, even after an initial backlog-draining fix (round 1) and a
//! shallow, easily-defeated version of that fix (round 2's C1 finding — see
//! [`drain_backlog`]'s own doc comment): a stranger who messages the bot
//! before, or DURING, the operator's own wait could still have their
//! `chat_id` captured instead, since arrival order alone proves nothing
//! about WHO sent a message. Since `super::telegram::handle_update`
//! authorizes purely on `from_id == chat_id`, that stranger would own
//! Approve/Deny for every future gateway tool call.
//!
//! The fix (review round 2 ruling): capture is now IDENTITY-PROVING, not
//! order-trusting. [`run_setup`] mints a short, per-run setup code — the
//! same shape `gateway pair` already mints
//! ([`super::auth::core::mint_pairing_code`]; never persisted, never
//! logged, lives only for this run) — and instructs the operator to send
//! it to the bot. Only a PRIVATE message whose text contains that code is
//! ever captured ([`message_carries_code`]); everything else (a message
//! from a stranger with no code, ANY group message) is skipped and the
//! wait continues. This closes the race regardless of how many OTHER
//! long-poll cycles a stranger's message might land in relative to the
//! operator's own — arrival order is irrelevant once identity is proven by
//! content instead.
//!
//! [`drain_backlog`] still runs first, before the code is ever shown as
//! something to send — now purely hygienic (see that function's own doc
//! comment) rather than the security boundary: it keeps a chatty bot's old
//! backlog from resurfacing in later calls, and reports how many messages
//! it cleared so an operator who already messaged the bot (or pressed
//! START) before running this wizard can see their own message was
//! accounted for, instead of silently dropped (I2).
//!
//! A batch naming more than one distinct private chat sending the code at
//! once still aborts rather than silently picking one — defence in depth,
//! vanishingly unlikely once a real code match is required, but never
//! trusted blindly either way.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Context;

use super::telegram_api::{BotApi, TgError, TgUpdate};

/// Total wall-clock budget the wizard waits for the setup code to arrive —
/// see [`run_setup`]'s own comment on the wait loop for why this is
/// enforced as a bounded number of [`WAIT_POLL_SECS`]-second poll calls
/// rather than an `Instant` deadline.
const MAX_WAIT_SECS: u32 = 60;

/// `getUpdates` long-poll `timeout` sent on every wizard wait-loop call.
const WAIT_POLL_SECS: u32 = 5;

/// `getUpdates` retry attempts on a TRANSPORT failure only (I1, review
/// round 2) — see [`get_updates_with_retry`]'s own doc comment. Total
/// attempts, not "retries": `3` means the original call plus 2 retries.
const TRANSPORT_RETRY_ATTEMPTS: u32 = 3;

/// Backoff between transport-error retries — same shape as
/// `super::telegram::poll_loop`'s own production `POLL_ERROR_BACKOFF`.
const TRANSPORT_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Upper bound on how many `getUpdates` pages [`drain_backlog`] will
/// consume before giving up and proceeding anyway — a safety valve against
/// a persistently flooding peer (or a misbehaving mock), not a realistic
/// expectation for a personal bot's backlog. Hitting this is a signal
/// something unusual is going on, not normal operation.
const DRAIN_MAX_PAGES: u32 = 25;

/// Calls [`BotApi::get_updates`], retrying up to [`TRANSPORT_RETRY_ATTEMPTS`]
/// times (each preceded by [`TRANSPORT_RETRY_BACKOFF`]) on a TRANSPORT
/// failure ONLY ([`TgError::is_transport`] — I1, review round 2: an
/// earlier revision retried on ANY error, which meant a revoked token or a
/// `409 Conflict` from a concurrently running `gateway run` poller burned
/// 3 attempts and 4+ seconds of sleep before surfacing — neither is fixed
/// by waiting and trying again). A genuine transient blip during a
/// legitimate 60-second wait shouldn't force the operator back to square
/// one (re-entering the token — `run_setup` has no way to resume
/// mid-wait), but a business-level failure should surface immediately.
/// Still propagates the real, already-scrubbed [`TgError`] once retries
/// are exhausted (or immediately, for a non-transport failure) —
/// deliberately NOT reinterpreted as a generic timeout: an honest
/// "connection failed" is more useful to a troubleshooting operator than a
/// misleading "timed out" that hides a real problem behind the same
/// wording a genuine no-code timeout uses.
fn get_updates_with_retry(
    api: &BotApi,
    offset: Option<i64>,
    timeout_secs: u32,
) -> Result<Vec<TgUpdate>, TgError> {
    let mut attempt = 0;
    loop {
        match api.get_updates(offset, timeout_secs) {
            Ok(updates) => return Ok(updates),
            Err(e) => {
                attempt += 1;
                if !e.is_transport() || attempt >= TRANSPORT_RETRY_ATTEMPTS {
                    return Err(e);
                }
                std::thread::sleep(TRANSPORT_RETRY_BACKOFF);
            }
        }
    }
}

/// Drains and confirms any Telegram update backlog that already existed
/// BEFORE this wizard run started. See the module docs' "H1" section for
/// why this matters even though capture itself is now code-proven: it's
/// hygiene, not the security boundary — it keeps a chatty bot's old
/// backlog from re-appearing in every later `getUpdates` call this wizard
/// (and eventually the production poller) makes, and it lets an operator
/// who already messaged the bot (or pressed START) before running this
/// wizard see their own message counted as cleared (I2) rather than
/// silently vanishing.
///
/// Loops `getUpdates(offset, 0)` — a short poll: "return whatever's
/// already queued, don't wait for more" — advancing `offset` past every
/// page it receives, until a page comes back EMPTY (bounded by
/// [`DRAIN_MAX_PAGES`]). This is NOT two special-cased calls (a "fetch"
/// then a separate "confirm") — Telegram's own `getUpdates` semantics
/// already make every call BOTH: passing `offset` tells the server every
/// update strictly below that value has been received and may be dropped
/// from its queue, AND returns the next page starting there.
///
/// **C1 (review round 2, Critical):** an earlier revision called this
/// exactly ONCE to fetch, then made a second "confirming" call and
/// DISCARDED its response. With Telegram's default `getUpdates` page size
/// (100, never overridden here — `BotApi::get_updates` sends no `limit`),
/// a 101+ message backlog left everything from update 101 onward BOTH
/// undrained and unconfirmed — reviving the exact hijack this function
/// exists to prevent, gated only on sending enough messages to fill a
/// page. Looping until an empty page closes that regardless of backlog
/// size, and no test could catch the shallow version because the mock
/// server itself never enforced a page-size cap — a mock-only blind spot,
/// not a production one.
///
/// Returns `(offset, cleared_count)`: the offset the wait loop should
/// start from (`None` if there was no backlog at all), and how many
/// updates were drained — printed to the operator so they can see their
/// own pre-wizard message was accounted for.
fn drain_backlog(api: &BotApi) -> anyhow::Result<(Option<i64>, u32)> {
    let mut offset: Option<i64> = None;
    let mut cleared: u32 = 0;
    for _ in 0..DRAIN_MAX_PAGES {
        let page =
            get_updates_with_retry(api, offset, 0).context("drain pending Telegram updates")?;
        if page.is_empty() {
            return Ok((offset, cleared));
        }
        cleared += page.len() as u32;
        let max_id = page
            .iter()
            .map(|u| u.update_id)
            .max()
            .expect("page just checked non-empty");
        offset = Some(max_id.saturating_add(1));
    }
    tracing::warn!(
        pages = DRAIN_MAX_PAGES,
        "telegram setup: backlog drain hit its page cap; proceeding with a possibly-incomplete drain"
    );
    Ok((offset, cleared))
}

/// Normalizes text for the setup-code comparison: uppercased, with every
/// non-alphanumeric character (the code's own `-`, stray whitespace or
/// punctuation a human might add) stripped — so "abcd-1234", "ABCD1234",
/// and "abcd 1234" all compare equal to a code printed as `"ABCD-1234"`.
fn normalize_code(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// `true` iff `update`'s message text contains `code`, once both are
/// normalized (see [`normalize_code`]). `false` for any update with no
/// text at all (a callback query, a sticker with no caption, any other
/// update kind).
fn message_carries_code(update: &TgUpdate, code: &str) -> bool {
    update
        .message_text
        .as_deref()
        .map(|text| normalize_code(text).contains(&normalize_code(code)))
        .unwrap_or(false)
}

/// The wizard's testable core. Every side effect — `input`/`out` instead of
/// real stdin/stdout, `api_base` instead of [`super::telegram::api_base`],
/// `gateway_dir` instead of resolving `~/.onebrain` via
/// [`crate::home::home_dir`] — is a parameter, so tests point each one at a
/// mock server / in-memory buffer / tempdir with zero env-var plumbing.
/// [`telegram_setup`] is the thin production wrapper that resolves the
/// real values and calls this.
///
/// Flow (extended per the H1/I1/I2/L3 review findings — see the module
/// docs' "H1" section for the capture rationale):
/// 1. Print BotFather instructions, read one line of stdin as the bot
///    token.
/// 2. [`BotApi::get_me`] to validate it. A failure here is reported as
///    EXACTLY `"token was not accepted by Telegram"` — never the
///    underlying [`TgError`] text (which never carries the token anyway,
///    per that module's "scrub chokepoint" docs, but this wizard adds its
///    own fixed message on top rather than forwarding ANY dynamic text
///    from a failed auth call).
/// 3. Mint a per-run setup code and print instructions to send it to the
///    bot.
/// 4. [`drain_backlog`] — hygiene, not security (see its own doc comment)
///    — and report how many messages it cleared.
/// 5. Long-poll `getUpdates` (via [`get_updates_with_retry`], budgeted by
///    CALL COUNT rather than an `Instant` deadline — see the loop's own
///    comment) until EXACTLY ONE distinct private chat sends a message
///    CONTAINING the code. A batch naming more than one distinct private
///    chat aborts immediately (ambiguity, never silently resolved); a
///    group/channel message carrying the code is skipped and the wait
///    continues, with its own timeout hint.
/// 6. Read-modify-write `gateway_dir/gateway.yml`'s `telegram:` block via
///    [`write_telegram_config`] — see that function's own doc comment for
///    why a raw [`serde_yaml::Value`], not [`super::config::GatewayConfig`].
/// 7. Send the confirmation message — restating the already-saved
///    `chat_id` in the error text if this fails (L3) — then print the
///    chat id.
///
/// A timeout at step 5 (no code-carrying message observed) returns an
/// error WITHOUT writing any config — [`write_telegram_config`] is only
/// ever reached once a single, unambiguous, code-proven private chat id is
/// in hand.
///
/// Deliberately does NOT touch [`super::telegram`]'s `getUpdates` offset
/// file (`~/.onebrain/gateway/telegram-<token_key>.offset`) — that file is
/// the PRODUCTION poller's own persisted cursor. This wizard's own `offset`
/// variable below is local and transient: it exists only so THIS run's
/// repeated polls don't re-return the same already-seen updates, and it is
/// discarded the moment `run_setup` returns. Nobody should ever wire these
/// two together — a wizard run advancing the poller's persisted cursor (or
/// vice versa) would let a real approval-flow message or button press go
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

    // Identity is proven by this code, not by arrival order — see the
    // module docs' "H1" section. Same code-generation shape `gateway pair`
    // already uses; never persisted, never logged, lives only for this run.
    let code = super::auth::mint_pairing_code();
    writeln!(
        out,
        "Send this code to @{} in Telegram: {code}",
        identity.username
    )?;
    writeln!(
        out,
        "(Pressing START also works if this is a brand-new bot, but the code above is what identifies you.)"
    )?;
    out.flush()?;

    // H1/C1/I2: clear (and confirm) any backlog that existed BEFORE this
    // wizard run — see `drain_backlog`'s own doc comment — before waiting
    // for a reply.
    writeln!(out, "Clearing any earlier messages to this bot…")?;
    out.flush()?;
    let (mut offset, cleared) = drain_backlog(&api)?;
    writeln!(out, "Cleared {cleared} earlier message(s).")?;

    writeln!(
        out,
        "Waiting for that code to arrive (up to {MAX_WAIT_SECS} s)…"
    )?;
    out.flush()?;

    // Budgeted by CALL COUNT (`MAX_WAIT_SECS / WAIT_POLL_SECS` iterations),
    // not by wall-clock `Instant`: against the REAL Telegram API, a
    // `getUpdates` call that finds nothing genuinely blocks server-side for
    // ~`WAIT_POLL_SECS` seconds, so bounding by iteration count already
    // approximates ~`MAX_WAIT_SECS` real seconds end-to-end. Against a test
    // mock that answers every call instantly, the SAME loop finishes in
    // milliseconds instead of stalling a timeout test for a genuine 60 real
    // seconds — deterministic and fast either way, with no test-only
    // clock-injection knob needed.
    let mut captured: Option<(i64, i64)> = None;
    let mut saw_group_message_with_code = false;
    for _ in 0..(MAX_WAIT_SECS / WAIT_POLL_SECS) {
        let updates = get_updates_with_retry(&api, offset, WAIT_POLL_SECS)
            .context("poll Telegram for the setup code")?;

        // Every update in the batch must still advance `offset` past it —
        // including ones that never carry the code — or an unrelated
        // update ahead of the real one in the same batch would make
        // Telegram replay it forever. Mirrors `super::telegram::poll_loop`'s
        // own "highest update_id in the batch, clamped against the offset
        // already on record" logic.
        let batch_max = updates.iter().map(|u| u.update_id).max();

        // Only a message whose text CONTAINS the code counts — see the
        // module docs' "H1" section for why arrival order alone is never
        // trusted. Distinct private (`chat_id > 0`) chats that sent the
        // code in THIS batch are deduped and collected; more than one is
        // an ambiguity to abort on. A group/supergroup/channel message
        // (`chat_id <= 0`, Telegram's own convention — see
        // `super::telegram::is_available`'s doc comment) carrying the code
        // is noted (`saw_group_message_with_code`, for the timeout hint)
        // but never added here, so it can never win an ambiguity check or
        // a capture.
        let mut private_hits: Vec<(i64, i64)> = Vec::new();
        for update in &updates {
            let Some(chat_id) = update.message_chat_id else {
                continue;
            };
            if !message_carries_code(update, &code) {
                continue;
            }
            let from_id = update.message_from_id.unwrap_or(0);
            if chat_id > 0 {
                // Same invariant `super::telegram::handle_update` will
                // later authorize every button press on (`from_id ==
                // chat_id`) — a private chat's own id IS the human's user
                // id under Telegram's own convention, so this always
                // holds for a genuine one-to-one DM. Skip (never capture)
                // anything claiming otherwise rather than trust it
                // blindly.
                if from_id == chat_id && !private_hits.iter().any(|(c, _)| *c == chat_id) {
                    private_hits.push((chat_id, from_id));
                }
            } else {
                saw_group_message_with_code = true;
            }
        }

        if let Some(max_id) = batch_max {
            let candidate = max_id.saturating_add(1);
            offset = Some(offset.map_or(candidate, |cur| candidate.max(cur)));
        }

        if private_hits.len() > 1 {
            anyhow::bail!(
                "{} different chats sent the code at the same moment — run this setup again and make sure only you send it",
                private_hits.len()
            );
        }
        if let Some(hit) = private_hits.into_iter().next() {
            captured = Some(hit);
            // Confirm this batch server-side too (see `drain_backlog`'s
            // own doc comment for why an offset-advancing call is what
            // actually confirms) so it can't resurface on a later wizard
            // run or the production poller's first-ever `getUpdates` call.
            // Best-effort: everything needed to finish setup is already in
            // hand, so a failure here is logged, not fatal.
            if let Err(e) = get_updates_with_retry(&api, offset, 0) {
                tracing::warn!(
                    error = %e,
                    "telegram setup: failed to confirm the captured message; it may resurface on a later run"
                );
            }
            break;
        }
    }

    let (chat_id, _from_id) = captured.ok_or_else(|| {
        if saw_group_message_with_code {
            anyhow::anyhow!(
                "saw the code in a group chat — Telegram approvals need a private one-to-one chat with the bot; open a DM with @{}, send {code} there, then run this setup again",
                identity.username
            )
        } else {
            anyhow::anyhow!(
                "no message carrying the code arrived — run this setup again and send {code} to @{}",
                identity.username
            )
        }
    })?;

    write_telegram_config(gateway_dir, &token, chat_id)?;

    api.send_message(
        chat_id,
        "OneBrain gateway connected. Approval requests will appear here.",
        None,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "gateway.yml saved (chat_id {chat_id}), but the confirmation message failed to send: {e}"
        )
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
    use std::time::Instant;

    // ── Mock Telegram Bot API server ───────────────────────────────────
    //
    // Duplicated from `telegram_api.rs` / `telegram.rs`'s own test-only
    // fixtures rather than shared, per this crate's established "one
    // private copy per module" convention (see either module's doc
    // comments for the precedent). `queued` gives per-method FIFO
    // responses — several tests below need a specific SEQUENCE of
    // `getUpdates` answers (drain pages, wait-loop iterations, …), which a
    // single static response can't express.

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

        /// The request bodies for every call to `method`, in call order —
        /// used by tests that need to inspect a SPECIFIC call in a
        /// sequence (e.g. "the second `getUpdates` call must carry the
        /// drain-paging offset"), not just whether the method was called
        /// at all.
        fn bodies(&self, method: &str) -> Vec<JsonValue> {
            self.requests()
                .into_iter()
                .filter(|(m, _)| m == method)
                .map(|(_, b)| b)
                .collect()
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

    fn getme_ok(state: &MockState) {
        state.set_response(
            "getMe",
            serde_json::json!({ "ok": true, "result": { "username": "my_onebrain_bot" } }),
        );
    }

    fn empty_updates() -> JsonValue {
        serde_json::json!({ "ok": true, "result": [] })
    }

    fn sendmessage_ok() -> JsonValue {
        serde_json::json!({ "ok": true, "result": { "message_id": 1 } })
    }

    /// A message with no text — fine for drain/backlog fixtures, where
    /// content is irrelevant (drain clears everything regardless), but
    /// never matches [`message_carries_code`].
    fn private_message(update_id: i64, chat_id: i64, from_id: i64) -> JsonValue {
        serde_json::json!({
            "update_id": update_id,
            "message": { "chat": { "id": chat_id }, "from": { "id": from_id } }
        })
    }

    /// A message whose text is exactly `code` — the only shape
    /// [`message_carries_code`] accepts as a genuine capture.
    fn coded_message(update_id: i64, chat_id: i64, from_id: i64, code: &str) -> JsonValue {
        serde_json::json!({
            "update_id": update_id,
            "message": {
                "chat": { "id": chat_id },
                "from": { "id": from_id },
                "text": code,
            }
        })
    }

    /// A `Write` that also lets ANOTHER thread read what's been written so
    /// far. Several tests below need to discover the setup code
    /// `run_setup` mints and prints — there is no way to predict it in
    /// advance (drawn from the OS CSPRNG via `mint_pairing_code`) — before
    /// they can script a matching mock `getUpdates` response, so
    /// `run_setup` runs on a background thread against one of these while
    /// the test thread polls it for the printed code.
    #[derive(Clone, Default)]
    struct SharedOutput(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedOutput {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedOutput {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// Polls `out` (bounded, 5s) until `run_setup`'s "Send this code to
    /// @username: CODE" text appears, then returns just the code (the
    /// remainder of that logical line's last whitespace-separated token).
    ///
    /// Searches the whole buffer for the marker rather than an exact
    /// per-line `starts_with` — `run_setup`'s immediately-preceding
    /// `write!(out, "Paste that token here: ")` has no trailing newline,
    /// and in these tests `input` is a pre-loaded `Cursor` (not a human
    /// pausing between prompts), so `read_line` returns instantly and the
    /// "Send this code to @…" text lands on the SAME physical line right
    /// after the prompt — a purely test-harness artifact of not being
    /// genuinely interactive, not a real output-formatting bug.
    fn wait_for_printed_code(out: &SharedOutput) -> String {
        const MARKER: &str = "Send this code to @";
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = out.text();
            if let Some(idx) = text.find(MARKER) {
                let rest = &text[idx + MARKER.len()..];
                let line_end = rest.find('\n').unwrap_or(rest.len());
                let code = rest[..line_end]
                    .split_whitespace()
                    .last()
                    .expect("the code line always has at least one token")
                    .to_string();
                assert!(!code.is_empty());
                return code;
            }
            assert!(
                Instant::now() < deadline,
                "wizard never printed its setup code: {text:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// `run_setup`, spawned on a background thread and joined once the
    /// caller has scripted mock responses referencing the discovered code.
    struct RunningSetup {
        code: String,
        out: SharedOutput,
        handle: std::thread::JoinHandle<anyhow::Result<()>>,
    }

    /// Spawns `run_setup` against `server_base`/`gateway_dir`, waits for it
    /// to print its setup code, and returns it (plus the output buffer and
    /// join handle) so the caller can queue the mock response(s) that
    /// reference it before joining.
    fn start_run_setup(server_base: &str, gateway_dir: &Path, token_line: &str) -> RunningSetup {
        let out = SharedOutput::default();
        let base = server_base.to_string();
        let dir = gateway_dir.to_path_buf();
        let mut input = Cursor::new(token_line.as_bytes().to_vec());
        let mut out_writer = out.clone();
        let handle =
            std::thread::spawn(move || run_setup(&mut input, &mut out_writer, &base, &dir));
        let code = wait_for_printed_code(&out);
        RunningSetup { code, out, handle }
    }

    // ── Tests ────────────────────────────────────────────────────────

    #[test]
    fn setup_captures_chat_id_and_writes_0600_config() {
        let state = MockState::default();
        getme_ok(&state);
        state.queue_response("getUpdates", empty_updates()); // drain: nothing.
        state.set_response("sendMessage", sendmessage_ok());
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let running = start_run_setup(&server.base, &gateway_dir, "123456:ABCDEF-token\n");
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [coded_message(10, 555, 555, &running.code)] }),
        );

        running.handle.join().unwrap().unwrap();

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

        assert!(running.out.text().contains("555"));
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
        getme_ok(&state);
        state.set_response("getUpdates", empty_updates());
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let mut input = Cursor::new(b"123456:ABCDEF-token\n".to_vec());
        let mut out: Vec<u8> = Vec::new();

        let err = run_setup(&mut input, &mut out, &server.base, &gateway_dir).unwrap_err();
        let rendered = err.to_string();

        assert!(
            rendered.contains("no message carrying the code arrived"),
            "{rendered}"
        );
        assert!(
            !gateway_dir.join("gateway.yml").exists(),
            "a timeout must not write any config"
        );
        // 1 drain call (empty page, stops immediately) + the full
        // wait-loop budget.
        assert_eq!(
            state.calls("getUpdates"),
            1 + (MAX_WAIT_SECS / WAIT_POLL_SECS) as usize,
            "must poll exactly the budgeted number of times, no more, no fewer"
        );
    }

    // ── H1 (review finding): backlog/ambiguity/group-message coverage ──

    /// A stranger's message sits in the backlog from BEFORE the wizard
    /// ever starts. `drain_backlog` must consume/confirm it before the
    /// setup code is even shown, and — since it predates the code and so
    /// can never contain it — it could never be captured anyway even if it
    /// weren't drained. Pins both halves of the fix: the wait loop only
    /// ever captures the operator's own, code-carrying message.
    #[test]
    fn setup_ignores_a_pre_start_stranger_in_the_backlog_and_captures_the_operators_message() {
        let state = MockState::default();
        getme_ok(&state);
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [private_message(9, 999, 111)] }),
        );
        state.queue_response("getUpdates", empty_updates()); // drain's 2nd page: empty, stop.
        state.set_response("sendMessage", sendmessage_ok());
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let running = start_run_setup(&server.base, &gateway_dir, "123456:ABCDEF-token\n");
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [coded_message(20, 555, 555, &running.code)] }),
        );

        running.handle.join().unwrap().unwrap();

        let content = std::fs::read_to_string(gateway_dir.join("gateway.yml")).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        assert_eq!(
            parsed["telegram"]["chat_id"].as_i64(),
            Some(555),
            "must capture the operator's chat, never the pre-drain stranger's"
        );

        // The drain's second (paging) call must have carried offset 10
        // (9 + 1).
        let bodies = state.bodies("getUpdates");
        assert_eq!(bodies[1]["offset"], 10, "{bodies:?}");
    }

    /// A single `getUpdates` batch where the real, code-carrying message is
    /// NOT the first array element (a non-message update precedes it) —
    /// proves the capture loop scans the WHOLE batch rather than assuming
    /// position 0.
    #[test]
    fn setup_finds_the_coded_message_when_it_is_not_first_in_the_batch() {
        let state = MockState::default();
        getme_ok(&state);
        state.queue_response("getUpdates", empty_updates()); // drain
        state.set_response("sendMessage", sendmessage_ok());
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let running = start_run_setup(&server.base, &gateway_dir, "123456:ABCDEF-token\n");
        state.queue_response(
            "getUpdates",
            serde_json::json!({
                "ok": true,
                "result": [
                    { "update_id": 20 }, // some other update kind: no message, no callback_query
                    coded_message(21, 555, 555, &running.code)
                ]
            }),
        );

        running.handle.join().unwrap().unwrap();

        let content = std::fs::read_to_string(gateway_dir.join("gateway.yml")).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        assert_eq!(parsed["telegram"]["chat_id"].as_i64(), Some(555));
    }

    /// A GROUP message (negative `chat_id`) carrying the code must not
    /// terminate the wizard — it's skipped, the wait continues, and a
    /// later batch carrying the code from a private chat still succeeds.
    #[test]
    fn setup_skips_a_group_message_carrying_the_code_and_keeps_waiting_for_a_private_one() {
        let state = MockState::default();
        getme_ok(&state);
        state.queue_response("getUpdates", empty_updates()); // drain
        state.set_response("sendMessage", sendmessage_ok());
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let running = start_run_setup(&server.base, &gateway_dir, "123456:ABCDEF-token\n");
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [coded_message(30, -100123456, 555, &running.code)] }),
        );
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [coded_message(31, 555, 555, &running.code)] }),
        );

        running.handle.join().unwrap().unwrap();

        let content = std::fs::read_to_string(gateway_dir.join("gateway.yml")).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        // The chat_id capture succeeding at all already proves the group
        // message didn't terminate the wizard (a bail! there would leave
        // no config on disk to read). `>=`, not `==`: the test thread
        // queues both scripted responses only AFTER discovering the code
        // (there is no way to know it sooner), so the wait loop can race
        // ahead and consume a few harmless default-empty responses first —
        // never fewer than this floor, but not a fixed count either.
        assert_eq!(parsed["telegram"]["chat_id"].as_i64(), Some(555));
        assert!(
            state.calls("getUpdates") >= 4,
            "expected at least drain + the group-message cycle + the successful cycle + its post-capture confirm, got {}",
            state.calls("getUpdates")
        );
    }

    /// L2: a wait that times out having seen the code ONLY from a group
    /// chat must report the group-specific hint (and the private-chat
    /// wording it carries), not the generic timeout message — and must
    /// still write no config.
    #[test]
    fn setup_times_out_with_a_group_specific_hint_when_only_a_group_message_carried_the_code() {
        let state = MockState::default();
        getme_ok(&state);
        state.queue_response("getUpdates", empty_updates()); // drain
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let running = start_run_setup(&server.base, &gateway_dir, "123456:ABCDEF-token\n");
        let code = running.code.clone();
        state.set_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [coded_message(99, -1009999, 555, &code)] }),
        );

        let err = running.handle.join().unwrap().unwrap_err();
        let rendered = err.to_string();

        assert!(
            rendered.contains("group chat") && rendered.contains("private one-to-one chat"),
            "{rendered}"
        );
        assert!(!gateway_dir.join("gateway.yml").exists());
    }

    /// H1's exact reviewer repro, reproduced directly at the code-matching
    /// layer: a SINGLE batch carries the code from two distinct private
    /// chats at once. The wizard must refuse to silently pick one — it
    /// aborts, naming the count, and writes no config and sends no
    /// confirmation.
    #[test]
    fn setup_aborts_on_an_ambiguous_batch_with_two_distinct_private_chats() {
        let state = MockState::default();
        getme_ok(&state);
        state.queue_response("getUpdates", empty_updates()); // drain
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let running = start_run_setup(&server.base, &gateway_dir, "123456:ABCDEF-token\n");
        state.queue_response(
            "getUpdates",
            serde_json::json!({
                "ok": true,
                "result": [
                    coded_message(9, 999, 999, &running.code),
                    coded_message(10, 555, 555, &running.code)
                ]
            }),
        );

        let err = running.handle.join().unwrap().unwrap_err();
        let rendered = err.to_string();

        assert!(rendered.contains('2'), "{rendered}");
        assert!(
            !gateway_dir.join("gateway.yml").exists(),
            "an ambiguous batch must never write config"
        );
        assert_eq!(
            state.calls("sendMessage"),
            0,
            "an ambiguous batch must never send a confirmation"
        );
    }

    /// Dedicated coverage that the drain step's paging call actually
    /// advances and confirms server-side: the SECOND `getUpdates` call
    /// must carry `offset = <page 1's max update_id> + 1` and
    /// `timeout = 0`. Deliberately does NOT need to know the (unpredictable)
    /// setup code — the wait loop simply times out afterward, which is
    /// irrelevant to what this test checks.
    #[test]
    fn setup_drain_pages_with_a_confirming_offset_and_zero_timeout() {
        let state = MockState::default();
        getme_ok(&state);
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [private_message(5, 42, 42)] }),
        );
        state.queue_response("getUpdates", empty_updates()); // drain's 2nd page: empty, stop.
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");
        let mut input = Cursor::new(b"123456:ABCDEF-token\n".to_vec());
        let mut out: Vec<u8> = Vec::new();

        // The wait loop never sees a code-carrying message (the code is
        // unknown to this test), so this times out — only the drain calls
        // themselves are under test here.
        let _ = run_setup(&mut input, &mut out, &server.base, &gateway_dir);

        let bodies = state.bodies("getUpdates");
        assert!(bodies.len() >= 2, "{bodies:?}");
        assert_eq!(bodies[1]["offset"], 6, "drain paging offset: {bodies:?}");
        assert_eq!(bodies[1]["timeout"], 0, "drain paging timeout: {bodies:?}");
    }

    /// C1 (review round 2, Critical): an earlier revision of
    /// `drain_backlog` fetched exactly ONE page, then made a second
    /// "confirming" call and DISCARDED its response. With Telegram's
    /// default `getUpdates` page size (100), a 101+ message backlog left
    /// everything from update 101 onward both undrained AND unconfirmed —
    /// reviving the exact hijack this function exists to prevent, gated
    /// only on sending enough messages to fill a page. This pins the fix:
    /// a 100-message first page, a 1-message second page, and a
    /// code-less message the wait loop also sees must all be drained or
    /// ignored — nothing is ever captured (none of them carries the
    /// unknown, per-run setup code), and no config is written.
    #[test]
    fn setup_drains_a_multi_page_backlog_and_never_captures_a_stranger_without_the_code() {
        let state = MockState::default();
        getme_ok(&state);

        let page1: Vec<JsonValue> = (1..=100).map(|i| private_message(i, 999, 999)).collect();
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": page1 }),
        );
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [private_message(101, 999, 999)] }),
        );
        state.queue_response("getUpdates", empty_updates()); // drain's 3rd page: empty, stop.
                                                             // The wait loop sees the same flood a bit more, still never
                                                             // matching (no code).
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [private_message(102, 999, 999)] }),
        );
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");
        let mut input = Cursor::new(b"123456:ABCDEF-token\n".to_vec());
        let mut out: Vec<u8> = Vec::new();

        let err = run_setup(&mut input, &mut out, &server.base, &gateway_dir).unwrap_err();
        let rendered = err.to_string();

        assert!(
            rendered.contains("no message carrying the code arrived"),
            "{rendered}"
        );
        assert!(!gateway_dir.join("gateway.yml").exists());
        assert_eq!(state.calls("sendMessage"), 0);

        // Drain must have looped across all 3 scripted pages, not stopped
        // after 2 calls the way the old fetch+confirm shape did.
        let bodies = state.bodies("getUpdates");
        assert!(bodies.len() >= 3, "{bodies:?}");

        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("Cleared 101 earlier message(s)."),
            "{printed}"
        );
    }

    // ── L3 (review round 2): send-confirmation failure restates chat_id ──

    #[test]
    fn setup_send_confirmation_failure_restates_the_saved_chat_id() {
        let state = MockState::default();
        getme_ok(&state);
        state.queue_response("getUpdates", empty_updates()); // drain
        let server = MockServer::start(state.clone());

        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join("home").join(".onebrain");

        let running = start_run_setup(&server.base, &gateway_dir, "123456:ABCDEF-token\n");
        state.queue_response(
            "getUpdates",
            serde_json::json!({ "ok": true, "result": [coded_message(1, 555, 555, &running.code)] }),
        );
        state.set_response(
            "sendMessage",
            serde_json::json!({ "ok": false, "description": "blocked by user" }),
        );

        let err = running.handle.join().unwrap().unwrap_err();
        let rendered = err.to_string();

        assert!(rendered.contains("555"), "{rendered}");
        assert!(
            gateway_dir.join("gateway.yml").exists(),
            "config must already be saved before the confirmation send is even attempted"
        );
    }

    // ── M1 (review finding): existing gateway.yml keys survive the write ──

    #[test]
    fn write_telegram_config_preserves_unrelated_existing_keys() {
        let root = tempfile::tempdir().unwrap();
        let gateway_dir = root.path().join(".onebrain");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        std::fs::write(
            gateway_dir.join("gateway.yml"),
            "port: 9999\nvaults:\n  work: /tmp/work\npolicy:\n  mutating: deny\n",
        )
        .unwrap();

        write_telegram_config(&gateway_dir, "tok-123", 555).unwrap();

        let content = std::fs::read_to_string(gateway_dir.join("gateway.yml")).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        assert_eq!(parsed["port"].as_i64(), Some(9999), "{content}");
        assert_eq!(
            parsed["vaults"]["work"].as_str(),
            Some("/tmp/work"),
            "{content}"
        );
        assert_eq!(
            parsed["policy"]["mutating"].as_str(),
            Some("deny"),
            "{content}"
        );
        assert_eq!(parsed["telegram"]["bot_token"].as_str(), Some("tok-123"));
        assert_eq!(parsed["telegram"]["chat_id"].as_i64(), Some(555));
    }
}
