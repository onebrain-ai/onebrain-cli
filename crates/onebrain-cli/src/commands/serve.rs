//! `onebrain serve [--dir <dist>] [--port N] [--host <addr>] [--open]`
//!
//! Foreground, ephemeral HTTP surface for the current session. Brings up ONE
//! local listener that serves a static web dist (SPA) + the read-only vault
//! JSON API, then blocks until Ctrl-C.
//!
//! Shares the entire server with `daemon __run` via [`crate::server`] — the
//! only difference is the shutdown trigger (Ctrl-C here, SIGTERM there). See
//! the build-level design `2026-06-04-daemon-serve-design.md` §2–4.
//
// step 2b: daemon-aware reuse — "if a daemon already runs, reuse its HTTP
//          surface instead of starting a second listener" (design §2). For now
//          `serve` always starts its own ephemeral server.

use crate::cli::ServeArgs;
use crate::output::OutputMode;
use crate::server::{self, generate_token, ServeConfig};
use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr};

/// Default port for `serve` when `--port` is omitted. `6789` is memorable
/// (the 6-7-8-9 run) and avoids the busy "round" ports — 8888 (Jupyter),
/// 5555 (Android ADB), 6666 (IRC), 3000/5173/8080 (dev servers) — as well as
/// 4317/4318 (OpenTelemetry OTLP gRPC/HTTP), which the previous default
/// collided with. The daemon uses the same default (one surface, one port).
pub const DEFAULT_PORT: u16 = 6789;

/// Run the foreground serve command. `mode` is currently unused for output
/// shaping (serve streams `tracing` lines, not an envelope) but is accepted for
/// signature parity with the other command handlers and future `--json`
/// startup-info support.
pub fn run(args: &ServeArgs, _mode: &OutputMode) -> Result<()> {
    // Resolve the vault from the standard chain (flag > env > walk-up). serve is
    // vault-required — the API has nothing to serve without one.
    let resolved = crate::vault_ctx::require(args.vault_dir.clone())?;
    let vault_root = resolved.root.as_path().to_path_buf();

    // Host: default 127.0.0.1; `--host 0.0.0.0` opts into remote self-host.
    let host: IpAddr = match args.host.as_deref() {
        None => IpAddr::V4(Ipv4Addr::LOCALHOST),
        Some(h) => h
            .parse()
            .with_context(|| format!("invalid --host address: {h}"))?,
    };
    let port = args.port.unwrap_or(DEFAULT_PORT);
    let token = generate_token();

    let cfg = ServeConfig {
        dist_dir: args.dir.clone(),
        // `serve` is vault-required (resolved above), so it always binds `Some`
        // — its vault endpoints never hit the daemon's 503 "no vault" path.
        vault_root: Some(vault_root),
        host,
        port,
        token: token.clone(),
    };

    // Jupyter-style URL — the token rides in the query string so a copy-paste
    // (or the `--open` browser launch) authenticates the SPA on first load.
    // Printed to stdout (not just the tracing log) so the user sees it
    // immediately in the foreground console.
    let url = format!("http://{host}:{port}/?token={token}");
    println!("OneBrain serving on {url}");
    println!("  vault: {}", resolved.root.as_path().display());
    match &cfg.dist_dir {
        Some(d) => println!("  dist:  {}", d.display()),
        None => println!("  dist:  (none — API only, placeholder page)"),
    }
    println!("  press Ctrl-C to stop");

    if args.open {
        // Best-effort: a failed browser launch must not stop the server.
        if let Err(e) = open_browser(&url) {
            eprintln!("warning: could not open browser: {e}");
        }
    }

    // Build a tokio runtime + block on the server, shutting down on Ctrl-C.
    // `serve` is a one-shot foreground process, so it's fine to own the runtime
    // here rather than the `#[tokio::main]` macro (the rest of the CLI is sync).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for serve")?;

    runtime.block_on(async move {
        let shutdown = async {
            // Ctrl-C (SIGINT). On error (extremely rare) we fall through to an
            // immediate shutdown rather than serving forever un-stoppably.
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("Ctrl-C received; shutting down serve");
        };
        server::run_server(cfg, shutdown).await
    })?;

    Ok(())
}

/// Open `url` in the platform default browser (best-effort).
///
/// macOS → `open`; other Unix → `xdg-open`. Windows isn't wired (the daemon /
/// serve are Unix-first); it returns an error the caller logs as a warning.
fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";

    #[cfg(unix)]
    {
        std::process::Command::new(cmd)
            .arg(url)
            .spawn()
            .with_context(|| format!("spawn `{cmd} {url}`"))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = url;
        anyhow::bail!("--open is not supported on this platform yet")
    }
}
