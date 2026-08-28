//! `onebrain gateway` — loopback MCP-over-HTTP run loop (Gateway PR 2, Task 4).
//!
//! `run` wires together Tasks 1–2's `load_gateway_config` + `build_gateway_router`
//! and hosts the result via [`crate::server::run_server_from_router`] — the
//! SAME primitive `serve`/`daemon __run` share (see `server/mod.rs`), differing
//! only in the shutdown trigger. This module mirrors `commands/serve.rs`'s
//! shutdown shape exactly (Ctrl-C only): `serve.rs` itself has no SIGTERM
//! handling to copy, so the gateway doesn't add one either.

pub mod config;
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

use crate::output::OutputMode;
use crate::server::run_server_from_router;

/// `onebrain gateway run [--port N]`.
///
/// Loopback hard-coded (`127.0.0.1`) — no bind flag or config key, unlike
/// `serve`'s `$ONEBRAIN_BIND` escape hatch: this build ships no auth, so
/// remote exposure isn't offered even as an opt-in (tunnel + OAuth land
/// later in v3.5). `port_flag` overrides `gateway.yml`'s configured `port`
/// when given; `--port 0` asks the OS for an ephemeral port, so the bound
/// address is only known after bind — which is why the startup line prints
/// from inside `on_bind`, not before this call (same #278 ordering
/// discipline `serve.rs` uses).
///
/// `mode` is currently unused: the startup line is a fixed, stable stdout
/// contract (`gateway listening on http://<bound-addr>/mcp`) that tests
/// parse regardless of `--json`/`--yaml`. Accepted for signature parity with
/// the other command handlers, mirroring `serve::run`'s own `_mode` param.
pub fn run(_mode: &OutputMode, port_flag: Option<u16>) -> anyhow::Result<()> {
    let config = load_gateway_config()?;
    let port = port_flag.unwrap_or(config.port);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let state = Arc::new(GatewayState { config });
    let router = build_gateway_router(state);

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
            eprintln!("loopback only — no auth in this build; do not tunnel yet");
        };
        run_server_from_router(router, addr, on_bind, shutdown).await
    })
}
