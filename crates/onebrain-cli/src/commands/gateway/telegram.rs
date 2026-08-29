//! Telegram approval channel — config availability gate (Gateway PR 5,
//! Task 2). Later Gateway PR 5 tasks (send/poll) build the actual delivery
//! flow on top of [`super::telegram_api::BotApi`]; this task only decides
//! WHETHER that flow can run at all on this running gateway process, and
//! where the Telegram API host is configured from.
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
//! [`api_base`] resolves the Telegram Bot API base URL every later task's
//! [`super::telegram_api::BotApi`] construction should use. The literal
//! `"https://api.telegram.org"` string appears in ACTUAL CODE exactly once
//! — inside this function — so nothing else in this crate can drift from
//! it (`telegram_api.rs`'s own doc comment mentions the same host, but only
//! as a `///` example, never as a value the code constructs).

use super::config::TelegramConfig;

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
/// No production caller yet — [`is_available`] is the only piece of this
/// module Task 2 wires into `server.rs`; the send/poll flow that actually
/// constructs a [`super::telegram_api::BotApi`] from this base is a later
/// Gateway PR 5 task. Same precedent as `telegram_api.rs`'s own
/// module-level `#![allow(dead_code)]` (its own doc comment names the
/// chain this continues) — scoped here to just this one function, since
/// [`is_available`] already has a real caller today.
#[allow(dead_code)]
pub fn api_base() -> String {
    std::env::var(TELEGRAM_API_BASE_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
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
}
