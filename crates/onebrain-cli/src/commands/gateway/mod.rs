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

pub mod auth;
pub mod config;
pub mod oauth_routes;
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

    let state = Arc::new(GatewayState { config });

    let auth_store = AuthStore::open().context("open gateway auth store")?;
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
}
