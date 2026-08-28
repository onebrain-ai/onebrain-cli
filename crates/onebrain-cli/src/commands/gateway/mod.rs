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

// `gateway_config_path` / `DEFAULT_GATEWAY_PORT` stay module-internal to
// `config.rs` (used there by `load_gateway_config` + its own tests) — no
// re-export here, since nothing outside `config.rs` calls them (YAGNI; see
// Task 4's dead-code-allow removal).
pub use config::{load_gateway_config, GatewayConfig};
pub use server::{build_gateway_router, GatewayState};

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;

use audit::AuditLog;
use auth::AuthStore;
use oauth_routes::AuthCtx;

use crate::output::OutputMode;
use crate::server::run_server_from_router;

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
    let config = load_gateway_config()?;
    let port = port_flag.unwrap_or(config.port);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let public_url = config.public_url.clone();
    if let Some(url) = public_url.as_deref() {
        if let Err(reason) = validate_public_url(url) {
            anyhow::bail!("gateway.yml `public_url` ({url:?}) is invalid: {reason}");
        }
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
