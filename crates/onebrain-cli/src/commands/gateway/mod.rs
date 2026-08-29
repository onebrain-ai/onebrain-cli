//! `onebrain gateway` — loopback MCP-over-HTTP run loop (Gateway PR 2, Task 4;
//! OAuth resource/authorization-server wiring added Gateway PR 3, Tasks 2-6).
//!
//! `run` wires together `load_gateway_config` + `build_gateway_router` and
//! hosts the result via [`crate::server::run_server_from_router`] — the SAME
//! primitive `serve`/`daemon __run` share (see `server/mod.rs`), differing
//! only in the shutdown trigger. This module mirrors `commands/serve.rs`'s
//! shutdown shape exactly (Ctrl-C only): `serve.rs` itself has no SIGTERM
//! handling to copy, so the gateway doesn't add one either.
//!
//! OAuth: `run` opens the gateway's [`auth::AuthStore`], prints the current
//! device-pairing code to stdout (the ONLY place it's ever shown — never
//! logged, never returned over HTTP), and builds the
//! [`oauth_routes::AuthCtx`] every OAuth/resource-server route shares. The
//! issuer is resolved (via [`resolve_issuer`]) and set exactly once inside
//! `on_bind`, after the listener is confirmed up
//! (`run_server_from_router`'s #278 ordering — see `server/mod.rs:368`), so
//! every request that reaches a handler observes a set issuer.
//!
//! `pair` (Task 6) is the standalone counterpart: it opens the same
//! `AuthStore` WITHOUT starting a server, so a user can read or rotate the
//! pairing code from a second terminal — whether or not `gateway run` is
//! currently up.

pub mod approval;
pub mod approval_native;
pub mod approval_routes;
pub mod audit;
pub mod auth;
pub mod config;
pub mod oauth_routes;
pub mod policy;
pub mod server;
pub mod telegram;
pub mod telegram_api;
pub mod telegram_setup;

// `gateway_config_path` / `DEFAULT_GATEWAY_PORT` stay module-internal to
// `config.rs` (used there by `load_gateway_config` + its own tests) — no
// re-export here, since nothing outside `config.rs` calls them (YAGNI; see
// Task 4's dead-code-allow removal).
pub use config::{load_gateway_config, GatewayConfig};
pub use server::{build_gateway_router, GatewayState};
pub use telegram_setup::telegram_setup;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;

use audit::AuditLog;
use auth::AuthStore;
use oauth_routes::AuthCtx;

use crate::output::OutputMode;
use crate::server::run_server_from_router;

/// `true` iff the env var `key` is set to any NON-EMPTY value — the shape
/// every boolean env switch in this crate already uses
/// (`search_common::daemon_routing_disabled`'s `ONEBRAIN_NO_DAEMON` and
/// `serve::bind_env`, both of which treat a set-but-empty value as unset, so
/// a hook-managed env block can neutralize a switch by blanking it instead
/// of having to unset the key).
///
/// It is a PRESENCE switch, not a boolean parser: `KEY=0` and `KEY=false`
/// are non-empty and therefore ON, exactly as `ONEBRAIN_NO_DAEMON=0` is.
/// Callers name their switch so the polarity reads correctly at the call
/// site (`ONEBRAIN_GATEWAY_DISABLE_*` → set means disabled).
///
/// Shared here rather than duplicated per switch: the gateway now has two of
/// them ([`approval_native::DISABLE_NATIVE_APPROVAL_ENV`] and
/// [`server::DISABLE_DAEMON_REINDEX_ENV`]) and they must not drift apart on
/// what "set" means.
pub(crate) fn env_switch_on(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

/// Default `tracing` filter for `gateway run` when `RUST_LOG` says nothing —
/// the same `info` default [`crate::commands::daemon`]'s `init_tracing` uses,
/// deliberately not a different one: an operator who already knows how to
/// turn up the daemon's logging must not have to learn a second convention
/// for the gateway.
const DEFAULT_LOG_FILTER: &str = "info";

/// The `tracing` filter [`init_tracing`] installs, read from the same
/// `RUST_LOG` variable `tracing_subscriber`'s own `try_from_default_env`
/// reads (named via its `DEFAULT_ENV` constant, so the two can never drift
/// apart on the variable's spelling).
fn log_filter() -> tracing_subscriber::EnvFilter {
    log_filter_from(std::env::var(tracing_subscriber::EnvFilter::DEFAULT_ENV).ok())
}

/// Hard floor appended to every resolved `RUST_LOG` filter, regardless of
/// where it lands in the rendered string — security review finding
/// (Gateway PR 5, Task 1 fix wave): `telegram_api`'s request URL embeds the
/// live bot token (`{base}/bot<TOKEN>/<method>`), and `ureq`'s own
/// `debug`/`trace` logging (via the `log` crate, bridged into this
/// subscriber by `tracing-subscriber`'s `tracing-log` default feature)
/// prints that full request path verbatim. An operator setting
/// `RUST_LOG=trace` to debug something UNRELATED must not be able to
/// accidentally print the token to the gateway's log file.
/// `tracing_subscriber::EnvFilter` resolves a target's effective level
/// from its most specific matching directive — a target-qualified
/// directive (`ureq=info`) always outranks a bare global level (`trace`)
/// for the `ureq` target specifically, which is what actually holds `ureq`
/// at `info` here. (`EnvFilter`'s `Display` sorts directives by
/// specificity rather than preserving input order, so the floor can print
/// FIRST in `to_string()` output — position in the string is cosmetic;
/// specificity is what enforces the floor. See
/// `log_filter_floors_ureq_logging_even_under_rust_log_trace` below.)
const UREQ_LOG_FLOOR: &str = "ureq=info";

/// [`log_filter`]'s decision, as a pure function of the raw `RUST_LOG` value
/// — so the RUST_LOG-honouring behavior is directly unit-testable without
/// mutating the process environment (and without [`init_tracing`], which any
/// one test binary can only meaningfully call once).
///
/// A set-but-EMPTY (or whitespace-only) value counts as UNSET and falls back
/// to [`DEFAULT_LOG_FILTER`]. That is the one place this deviates from
/// `daemon::init_tracing`, which would hand `""` straight to `EnvFilter` and
/// silently filter everything out; it follows this crate's own
/// [`env_switch_on`] convention instead, where blanking a variable is how a
/// hook-managed env block neutralizes it rather than a request for silence.
///
/// Every resolved filter also carries [`UREQ_LOG_FLOOR`], appended after
/// whichever base directives win below — see that constant's doc comment.
fn log_filter_from(raw: Option<String>) -> tracing_subscriber::EnvFilter {
    let base = match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(directives) => directives,
        None => DEFAULT_LOG_FILTER,
    };
    let floored = format!("{base},{UREQ_LOG_FLOOR}");
    // `floored` fails to parse only when `base` itself was invalid (the
    // hardcoded `UREQ_LOG_FLOOR` suffix always parses) — fall back to the
    // default filter, ALSO with the floor appended, rather than ever
    // constructing an EnvFilter without it.
    tracing_subscriber::EnvFilter::try_new(&floored).unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(format!("{DEFAULT_LOG_FILTER},{UREQ_LOG_FLOOR}"))
    })
}

/// Install a `tracing` subscriber for the foreground `gateway run` process.
/// Returns `true` iff THIS call is the one that installed it.
///
/// Until this existed, `gateway run` installed no subscriber at all (the only
/// one in the workspace belonged to `daemon __run`), so every
/// `tracing::warn!` the gateway emits was discarded — including the ones
/// several deliberate design decisions rest on being visible: the audit-append
/// failure path that keeps `path.display()` FOR the operator, the best-effort
/// reindex that logs a failure instead of propagating it, the pending-approval
/// cap refusal that tells an operator WHICH cap was hit,
/// [`policy::PolicyConfig::startup_warnings`]'s degenerate-config flag, the
/// pairing rate limiter's lockout notice, and every `sanitized_internal` /
/// `core_error` call whose client-facing message is deliberately stripped of
/// detail ON THE PROMISE that the full error, host path included, reaches the
/// operator here.
///
/// Follows `daemon::init_tracing`'s conventions rather than inventing new
/// ones: `RUST_LOG` via `EnvFilter` (defaulting to [`DEFAULT_LOG_FILTER`]),
/// output to **stderr**, and `try_init` so an already-installed global
/// subscriber is left alone instead of panicking.
///
/// Two things it deliberately does differently, both because this is a
/// foreground process a human is watching rather than a detached daemon
/// writing to a log file:
/// - ANSI is enabled only when stderr is a real terminal (the crate's
///   existing `std::io::IsTerminal` convention — see `banner.rs`), so a
///   redirected `2>gateway.log` gets clean text and a terminal gets colour.
/// - stdout is untouched. `run`'s pairing-code line and the
///   `gateway listening on …` line are a deliberate plain-`println!` stdout
///   contract that integration tests parse; routing logs to stderr keeps
///   them out of it entirely.
fn init_tracing() -> bool {
    use std::io::IsTerminal;

    tracing_subscriber::fmt()
        .with_env_filter(log_filter())
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .try_init()
        .is_ok()
}

/// Resolves the OAuth issuer base URL: the configured `public_url` (trailing
/// slash trimmed) when set, else `http://127.0.0.1:<bound-port>`. Pure and
/// unit-tested directly below — the `on_bind` closure that calls it isn't
/// practical to unit test on its own (it only runs inside a live
/// `run_server_from_router` call).
fn resolve_issuer(public_url: Option<&str>, bound: SocketAddr) -> String {
    match public_url {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => format!("http://127.0.0.1:{}", bound.port()),
    }
}

/// Validate a configured `public_url` (security review, Important: header
/// injection + silent-fallback-to-insecure-issuer). Called ONCE at `run()`
/// startup, before anything else happens (before opening the auth store,
/// before binding) so a bad value fails FAST with a clear error naming the
/// `public_url` key — this deliberately never falls back to the loopback
/// issuer on an invalid value, because a silently-wrong issuer would break
/// every discovery document (PRM / AS metadata) and the `/mcp` 401
/// `WWW-Authenticate` challenge in a way that's very hard to debug from the
/// client side.
///
/// Requirements:
/// - No character that cannot sit inside an HTTP header quoted-string: `"`,
///   `\\`, or any other ASCII control character (this also rules out
///   CR/LF, i.e. header/response-splitting through this value) —
///   `public_url` is echoed verbatim into the `WWW-Authenticate` challenge's
///   quoted `resource_metadata` parameter
///   (`auth::middleware::challenge`) and into every discovery document's
///   URLs.
/// - An absolute `http://`/`https://` origin — `scheme://host[:port]` —
///   with NO path, query, or fragment after it. A single trailing `/` is
///   the one exception (mirrors [`resolve_issuer`]'s own trim): `"https://x.example/"`
///   is valid, `"https://x.example/mcp"` is not.
/// - `http://` is accepted ONLY for a loopback host (`localhost` /
///   `127.0.0.1` — the same definition
///   `oauth_routes::is_loopback_redirect_uri` uses); every other host must
///   use `https://`, or the resolved issuer would be silently insecure.
fn validate_public_url(raw: &str) -> Result<(), String> {
    if let Some(bad) = raw
        .chars()
        .find(|c| matches!(c, '"' | '\\') || c.is_control())
    {
        return Err(format!(
            "contains a character that cannot appear in an HTTP header value: {bad:?}"
        ));
    }

    let trimmed = raw.trim_end_matches('/');
    let Some((scheme, authority)) = trimmed.split_once("://") else {
        return Err("must be an absolute http:// or https:// URL".to_string());
    };

    if authority.is_empty() {
        return Err("is missing a host".to_string());
    }
    if authority.contains(['/', '?', '#']) {
        return Err(
            "must be a bare origin (scheme://host[:port]) with no path, query, or fragment"
                .to_string(),
        );
    }

    let host = authority.split(':').next().unwrap_or(authority);
    let is_loopback = host == "localhost" || host == "127.0.0.1";

    match scheme {
        "https" => Ok(()),
        "http" if is_loopback => Ok(()),
        "http" => Err(format!(
            "must use https:// for a non-loopback host (got http://{authority})"
        )),
        other => Err(format!(
            "has unsupported scheme {other:?} — must be http or https"
        )),
    }
}

/// `onebrain gateway run [--port N]`.
///
/// Loopback hard-coded (`127.0.0.1`) — no bind flag or config key, unlike
/// `serve`'s `$ONEBRAIN_BIND` escape hatch: `public_url` is the deliberate,
/// explicit way to expose the gateway's OAuth issuer later (behind a tunnel —
/// PR 3's later phase), not an implicit bind-address override. This build
/// still only ever binds `127.0.0.1`.
/// `port_flag` overrides `gateway.yml`'s configured `port` when given;
/// `--port 0` asks the OS for an ephemeral port, so the bound address (and
/// therefore the fallback issuer) is only known after bind — which is why
/// both the startup line AND the issuer resolution happen from inside
/// `on_bind`, not before this call (same #278 ordering discipline
/// `serve.rs` uses).
///
/// `mode` is currently unused: the startup line is a fixed, stable stdout
/// contract (`gateway listening on http://<bound-addr>/mcp`) that tests
/// parse regardless of `--json`/`--yaml`. Accepted for signature parity with
/// the other command handlers, mirroring `serve::run`'s own `_mode` param.
pub fn run(_mode: &OutputMode, port_flag: Option<u16>) -> anyhow::Result<()> {
    // FIRST, before anything that could want to say something: every
    // `tracing` call in the gateway is a no-op until a subscriber exists, and
    // the earliest diagnostics (config warnings, audit/auth store setup) are
    // among the ones an operator most needs. See `init_tracing`'s own doc
    // comment for the full list of decisions that depend on this.
    init_tracing();

    let config = load_gateway_config()?;
    let port = port_flag.unwrap_or(config.port);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let public_url = config.public_url.clone();
    if let Some(url) = public_url.as_deref() {
        if let Err(reason) = validate_public_url(url) {
            anyhow::bail!("gateway.yml `public_url` ({url:?}) is invalid: {reason}");
        }
    }
    // Legal-but-almost-certainly-unintended `policy:` values (today: only
    // `approval_wait_seconds: 0`, which silently refuses every gated call on
    // an instant timeout). Warnings, never a hard failure — each flagged
    // value is fail-CLOSED and legitimately used by this repo's own tests.
    // The rules themselves live in `PolicyConfig::startup_warnings` and are
    // unit-tested there; this loop only logs them, because `run()` itself is
    // subprocess-only under coverage.
    for warning in config.policy.startup_warnings() {
        tracing::warn!("gateway.yml: {warning}");
    }

    // Opens `~/.onebrain/gateway/audit/` (created 0700 if absent) — Task 1
    // built this infrastructure with no external caller yet; this is its
    // first one. A failure here (unwritable home, permission problem) fails
    // gateway startup outright rather than silently running with no audit
    // trail — [`AuditLog::append`] is infallible-from-the-caller's-view for
    // a PER-ENTRY write failure (see its own doc comment), but that's a
    // different concern from the log not being OPENABLE at all at startup.
    let audit = AuditLog::open().context("open gateway audit log")?;
    let state = Arc::new(GatewayState::new(config, audit));

    let auth_store = AuthStore::open().context("open gateway auth store")?;
    // Best-effort startup housekeeping: drop expired auth codes/tokens
    // before serving. See `AuthStore::purge_expired`'s doc comment for why
    // a used, family-linked code can outlive its own TTL. Never fatal to
    // startup: a purge failure only means stale records linger a bit
    // longer on disk, not that the gateway is unsafe to serve.
    match auth_store.purge_expired() {
        Ok(dropped) => {
            tracing::debug!(
                dropped,
                "purged expired gateway auth-store records at startup"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to purge expired gateway auth-store records at startup; continuing");
        }
    }
    // The pairing code is printed here — stdout of the foreground `gateway
    // run` process — and NOWHERE else: never logged (the daemon/server log
    // is a longer-lived, potentially shared file), never returned over HTTP.
    // `pairing_code()` mints one on first call and is stable after, so
    // restarting `gateway run` doesn't rotate it out from under an
    // in-progress pairing.
    let pairing_code = auth_store
        .pairing_code()
        .context("mint/read gateway pairing code")?;
    println!("pairing code: {pairing_code}  (rotate: onebrain gateway pair --rotate)");
    let auth_ctx = Arc::new(AuthCtx::new(auth_store));

    let router = build_gateway_router(state, auth_ctx.clone());

    // One-shot foreground process — own the runtime here rather than
    // `#[tokio::main]` (mirrors `serve::run` and `daemon::run_internal`).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for gateway")?;

    runtime.block_on(async move {
        let shutdown = async {
            // Ctrl-C (SIGINT) only — mirrors `serve.rs`'s shutdown future
            // shape exactly; `serve` has no SIGTERM handling to copy.
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("Ctrl-C received; shutting down gateway");
        };
        let on_bind = move |bound: SocketAddr| {
            // Stable, single-line stdout contract — the binary integration
            // test parses this to learn the bound address (needed with
            // `--port 0`). Keep this line's shape exactly as-is.
            println!("gateway listening on http://{bound}/mcp");
            eprintln!("loopback only — /mcp requires OAuth; /.well-known/* discovery is public");
            // Resolve + publish the issuer AFTER the listener is confirmed up
            // (see module docs / #278) so every request that reaches a
            // handler observes a set value. `set` fails only if `on_bind`
            // somehow ran twice, which `run_server_from_router` never does —
            // a failure here is harmless (the first-set value stays
            // authoritative) so the result is deliberately discarded.
            let issuer = resolve_issuer(public_url.as_deref(), bound);
            let _ = auth_ctx.issuer.set(issuer);
        };
        run_server_from_router(router, addr, on_bind, shutdown).await
    })
}

/// `onebrain gateway pair [--rotate]`.
///
/// Opens the SAME on-disk auth store `run` opens (`AuthStore::open()` —
/// `~/.onebrain/gateway/pairing.json`), then either reads the current
/// pairing code (minting one on first call, per [`AuthStore::pairing_code`])
/// or mints a fresh one in its place (per [`AuthStore::rotate_pairing_code`],
/// which immediately invalidates the old code) when `--rotate` is given, and
/// prints the result to stdout — the same channel `run`'s own startup line
/// uses, and per that module's doc comment, the ONLY place a pairing code is
/// ever shown (never logged, never returned over HTTP). This lets a user
/// re-read or rotate the code from a second terminal without restarting (or
/// even having running) `gateway run` — the pairing code lives in the store,
/// not in the running process's memory.
///
/// `mode` is unused for the same reason `run`'s `_mode` is: this stdout line
/// is a stable, single-purpose contract, not a structured `--json`/`--yaml`
/// payload. Accepted for signature parity with `run` and every other verb
/// handler `dispatch.rs` calls uniformly.
pub fn pair(_mode: &OutputMode, rotate: bool) -> anyhow::Result<()> {
    let auth_store = AuthStore::open().context("open gateway auth store")?;
    if rotate {
        let code = auth_store
            .rotate_pairing_code()
            .context("rotate gateway pairing code")?;
        println!("pairing code rotated: {code}");
    } else {
        let code = auth_store
            .pairing_code()
            .context("mint/read gateway pairing code")?;
        println!("pairing code: {code}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    // ── tracing subscriber (round-2 finding D) ────────────────────────────

    /// `RUST_LOG` wins when set — an operator debugging a gateway must be
    /// able to turn the level up the same way they do for the daemon. Every
    /// resolved filter also carries [`UREQ_LOG_FLOOR`] (see that constant's
    /// doc comment) — pinned here alongside the RUST_LOG-honouring
    /// behavior, and again on its own below. Checked via `contains` rather
    /// than exact equality: `EnvFilter`'s `Display` sorts directives by
    /// specificity rather than preserving append order (the floor prints
    /// FIRST, not last — confirmed empirically), which is an
    /// implementation detail these tests shouldn't couple to.
    #[test]
    fn log_filter_honours_rust_log_when_set() {
        let debug = log_filter_from(Some("debug".to_string())).to_string();
        assert!(debug.contains("debug"), "{debug}");
        assert!(debug.contains(UREQ_LOG_FLOOR), "{debug}");

        let onebrain_trace = log_filter_from(Some("onebrain=trace".to_string())).to_string();
        assert!(
            onebrain_trace.contains("onebrain=trace"),
            "{onebrain_trace}"
        );
        assert!(onebrain_trace.contains(UREQ_LOG_FLOOR), "{onebrain_trace}");
    }

    /// …and falls back to a sensible foreground default when it is unset,
    /// blank, or unparseable, rather than silently filtering everything out.
    #[test]
    fn log_filter_falls_back_to_the_default_level() {
        let none = log_filter_from(None).to_string();
        assert!(none.contains(DEFAULT_LOG_FILTER), "{none}");
        assert!(none.contains(UREQ_LOG_FLOOR), "{none}");

        let blank = log_filter_from(Some("   ".to_string())).to_string();
        assert!(
            blank.contains(DEFAULT_LOG_FILTER),
            "a blanked RUST_LOG must not mute the gateway entirely: {blank}"
        );
        assert!(blank.contains(UREQ_LOG_FLOOR), "{blank}");

        // `EnvFilter` accepts almost any bare word as a target directive, so
        // an unparseable value has to name an invalid LEVEL to actually
        // fail — which is exactly the typo an operator makes.
        let mistyped = log_filter_from(Some("onebrain=verbose".to_string())).to_string();
        assert!(
            mistyped.contains(DEFAULT_LOG_FILTER),
            "a mistyped level must fall back to the default, not silence the gateway: {mistyped}"
        );
        assert!(mistyped.contains(UREQ_LOG_FLOOR), "{mistyped}");
    }

    /// Security review finding (Gateway PR 5, Task 1 fix wave): an operator
    /// setting `RUST_LOG=trace` for an UNRELATED reason must never turn up
    /// `ureq`'s own logging — `telegram_api`'s request URL embeds the live
    /// bot token, and `ureq` prints the full request path at `debug`/
    /// `trace`. This pins that [`UREQ_LOG_FLOOR`] is present in the
    /// resolved filter even under the broadest possible operator
    /// directive. What actually holds `ureq` at `info` is `EnvFilter`'s
    /// specificity resolution (a target-qualified directive always
    /// outranks a bare global level for that target) — NOT the floor's
    /// position in the rendered string, which `EnvFilter`'s `Display`
    /// sorts by specificity rather than append order (it renders the
    /// floor FIRST here, not last).
    #[test]
    fn log_filter_floors_ureq_logging_even_under_rust_log_trace() {
        let filter = log_filter_from(Some("trace".to_string())).to_string();
        assert!(
            filter.contains(UREQ_LOG_FLOOR),
            "ureq must be floored to info even when the rest of the filter is trace: {filter}"
        );
    }

    /// The variable actually consulted is the one `tracing_subscriber` itself
    /// names, so this never drifts to a private spelling of `RUST_LOG`.
    #[test]
    fn log_filter_reads_the_standard_rust_log_variable() {
        assert_eq!(tracing_subscriber::EnvFilter::DEFAULT_ENV, "RUST_LOG");
    }

    /// A global subscriber can only be set once per process, and
    /// `gateway run` can legitimately be reached from a context that already
    /// installed one. Installing twice must be a quiet no-op, never a panic.
    ///
    /// Deliberately asserts only on the SECOND call: whether the first one
    /// wins depends on what else in this test binary has already installed a
    /// subscriber, but once any subscriber is global, every later attempt
    /// must report `false` and leave it standing.
    ///
    /// `RUST_LOG=off` is set for the install: a global subscriber outlives
    /// the test that set it, and `tracing` writes to the real `stderr`
    /// handle rather than the one libtest captures, so a default-level
    /// subscriber installed here would spray this crate's (and tantivy's)
    /// `info` lines across the whole test binary's output. A unit test must
    /// not change what the other ~1,700 tests print. The filter is captured
    /// at install time, so it stays `off` after the env guard restores the
    /// variable.
    #[test]
    fn init_tracing_is_idempotent_and_never_panics_on_a_second_call() {
        let _env = crate::test_env::set_var("RUST_LOG", "off");
        let _first = init_tracing();
        assert!(
            !init_tracing(),
            "a second init must not claim to have installed a subscriber"
        );
    }

    #[test]
    fn resolve_issuer_uses_public_url_trimmed_of_trailing_slash() {
        assert_eq!(
            resolve_issuer(Some("https://gw.example.com/"), addr(7717)),
            "https://gw.example.com"
        );
        assert_eq!(
            resolve_issuer(Some("https://gw.example.com"), addr(7717)),
            "https://gw.example.com"
        );
    }

    #[test]
    fn resolve_issuer_falls_back_to_bound_loopback_port_when_unset() {
        assert_eq!(resolve_issuer(None, addr(54321)), "http://127.0.0.1:54321");
    }

    // ── validate_public_url (security review, Important) ───────────────────

    #[test]
    fn validate_public_url_accepts_a_valid_https_origin() {
        assert!(validate_public_url("https://gw.example.com").is_ok());
        assert!(validate_public_url("https://gw.example.com:8443").is_ok());
    }

    #[test]
    fn validate_public_url_accepts_and_trims_a_trailing_slash() {
        assert!(validate_public_url("https://gw.example.com/").is_ok());
    }

    #[test]
    fn validate_public_url_rejects_a_path() {
        let err = validate_public_url("https://gw.example.com/mcp").unwrap_err();
        assert!(
            err.contains("path"),
            "error should name the problem as a path: {err}"
        );
    }

    #[test]
    fn validate_public_url_rejects_query_and_fragment_too() {
        assert!(validate_public_url("https://gw.example.com?x=1").is_err());
        assert!(validate_public_url("https://gw.example.com#frag").is_err());
    }

    #[test]
    fn validate_public_url_rejects_an_embedded_quote() {
        let err = validate_public_url("https://gw.example.com\"evil").unwrap_err();
        assert!(
            err.contains("cannot appear in an HTTP header"),
            "error should name the header-injection problem: {err}"
        );
    }

    #[test]
    fn validate_public_url_rejects_embedded_crlf() {
        assert!(validate_public_url("https://gw.example.com\r\nX-Injected: 1").is_err());
        assert!(validate_public_url("https://gw.example.com\nX-Injected: 1").is_err());
    }

    #[test]
    fn validate_public_url_rejects_non_loopback_http() {
        let err = validate_public_url("http://gw.example.com").unwrap_err();
        assert!(
            err.contains("https"),
            "error should say https is required: {err}"
        );
    }

    #[test]
    fn validate_public_url_accepts_loopback_http() {
        assert!(validate_public_url("http://127.0.0.1:7717").is_ok());
        assert!(validate_public_url("http://localhost:7717").is_ok());
    }

    #[test]
    fn validate_public_url_rejects_missing_scheme_and_unsupported_scheme() {
        assert!(validate_public_url("gw.example.com").is_err());
        assert!(validate_public_url("ftp://gw.example.com").is_err());
    }
}
