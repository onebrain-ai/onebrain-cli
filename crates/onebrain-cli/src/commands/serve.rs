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
use crate::output::{item, section, OutputMode};
use crate::server::{self, resolve_token, ServeConfig};
use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr};

/// Default port for `serve` when `--port` is omitted. `6789` is memorable
/// (the 6-7-8-9 run) and avoids the busy "round" ports — 8888 (Jupyter),
/// 5555 (Android ADB), 6666 (IRC), 3000/5173/8080 (dev servers) — as well as
/// 4317/4318 (OpenTelemetry OTLP gRPC/HTTP), which the previous default
/// collided with. The daemon uses the same default (one surface, one port).
pub const DEFAULT_PORT: u16 = 6789;

/// Format the web UI label for the serve banner: `OneBrain Web UI v0.1.1
/// (2026-07-01)`, or without the ` (date)` when the release date isn't known.
fn ui_label(version: &str, released: Option<&str>) -> String {
    match released {
        Some(date) => format!("OneBrain Web UI v{version} ({date})"),
        None => format!("OneBrain Web UI v{version}"),
    }
}

/// Build the startup banner (no I/O) in the grouped-status convention:
///
/// ```text
/// 🌐  Serving
///     URL           http://127.0.0.1:6789/?token=…
///     Vault         /Users/…/ob-1
///     Web UI        <ui_line>
///
/// ⏹️   Ctrl-C to stop
/// ```
///
/// Returns the whole block including a trailing newline so the caller can
/// `print!` it verbatim. `⏹️` is followed by three spaces (one convention
/// space plus two — the emoji is a variation-selector glyph that renders
/// narrower than a full two-column emoji, so the extra space keeps the hint
/// text visually aligned with the section body).
fn build_banner(url: &str, vault: &str, ui_line: &str) -> String {
    let mut out = String::new();
    out.push_str(&section("🌐", "Serving"));
    out.push('\n');
    out.push_str(&item("URL", url));
    out.push('\n');
    out.push_str(&item("Vault", vault));
    out.push('\n');
    out.push_str(&item("Web UI", ui_line));
    out.push('\n');
    out.push('\n');
    out.push_str("⏹️   Ctrl-C to stop");
    out.push('\n');
    out
}

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
    // Honours $ONEBRAIN_TOKEN (≥16 chars) for a stable, bookmarkable URL behind a
    // tunnel; otherwise a fresh random per-process token.
    let token = resolve_token();

    let cfg = ServeConfig {
        dist_dir: args.dir.clone(),
        // `serve` is vault-required (resolved above), so it always binds `Some`
        // — its vault endpoints never hit the daemon's 503 "no vault" path.
        vault_root: Some(vault_root),
        host,
        port,
        token: token.clone(),
        // Foreground `serve` is short-lived and not the canonical engine owner,
        // so it opens the search engine per-request as before rather than
        // holding it. Only the daemon (`daemon __run`) sets `hold_engine`.
        hold_engine: false,
    };

    // Jupyter-style URL — the token rides in the query string so a copy-paste
    // (or the `--open` browser launch) authenticates the SPA on first load.
    // Printed to stdout (not just the tracing log) so the user sees it
    // immediately in the foreground console. The framed, emoji-prefixed banner
    // mirrors OneBrain's session-greeting look (a `────` rule + fields).
    let url = format!("http://{host}:{port}/?token={token}");
    // The 🎨 line reports the web UI source and, when the dist exposes a
    // `version.json` (onebrain-webui ≥ 0.1.1), the live UI version + release date
    // inline as `OneBrain Web UI vX.Y.Z (YYYY-MM-DD)` — read from the `--dir`
    // dist on disk, else the embedded assets. The date comes from the sibling
    // `changelog.json`; an absent marker degrades gracefully (version without
    // date, or the plain source description).
    let ui_line = match &cfg.dist_dir {
        Some(d) => match std::fs::read(d.join("version.json"))
            .ok()
            .and_then(|bytes| server::parse_webui_version(&bytes))
        {
            Some(v) => {
                let released = std::fs::read(d.join("changelog.json"))
                    .ok()
                    .and_then(|bytes| server::parse_webui_released(&bytes));
                format!("{} — {}", d.display(), ui_label(&v, released.as_deref()))
            }
            None => d.display().to_string(),
        },
        None if server::has_embedded_ui() => match server::webui_version() {
            Some(v) => ui_label(&v, server::webui_released().as_deref()),
            None => "embedded web UI".to_string(),
        },
        None => "no UI — API only (placeholder page)".to_string(),
    };
    // Grouped-convention banner (matches `search status` / `doctor`): a
    // `🌐  Serving` section header, then indented `Label  value` rows, a blank
    // line, and the stop hint.
    print!(
        "{}",
        build_banner(
            &url,
            &resolved.root.as_path().display().to_string(),
            &ui_line
        )
    );

    // Binding beyond loopback exposes the daemon on the network over PLAIN HTTP
    // — the token + vault content would travel unencrypted. Warn loudly and
    // point at the safe way to do it (a TLS tunnel/proxy in front).
    if !host.is_loopback() {
        eprintln!();
        eprintln!(
            "  ⚠️  WARNING: --host {host} exposes OneBrain beyond this machine over PLAIN HTTP."
        );
        eprintln!("     The auth token and all vault content would travel UNENCRYPTED.");
        eprintln!("     Do NOT expose this port directly. Put a TLS tunnel/proxy in front:");
        eprintln!(
            "       • Cloudflare Tunnel + Access   • Tailscale Serve   • Caddy + Let's Encrypt"
        );
        eprintln!("     Keep the default --host 127.0.0.1 unless you've set one up.");
        eprintln!();
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_label_with_and_without_date() {
        assert_eq!(
            ui_label("0.1.1", Some("2026-07-01")),
            "OneBrain Web UI v0.1.1 (2026-07-01)"
        );
        assert_eq!(ui_label("0.1.1", None), "OneBrain Web UI v0.1.1");
    }

    #[test]
    fn banner_uses_grouped_convention() {
        let b = build_banner(
            "http://127.0.0.1:6789/?token=abc",
            "/Users/keng/ob-1",
            "OneBrain Web UI v0.1.1 (2026-07-01)",
        );
        // Section header: emoji + two spaces + Capitalized title.
        assert!(b.starts_with("🌐  Serving\n"), "{b:?}");
        // Indented, fixed-width label rows carry every field.
        assert!(
            b.contains("    URL           http://127.0.0.1:6789/?token=abc"),
            "{b:?}"
        );
        assert!(b.contains("    Vault         /Users/keng/ob-1"), "{b:?}");
        assert!(
            b.contains("    Web UI        OneBrain Web UI v0.1.1 (2026-07-01)"),
            "{b:?}"
        );
        // A blank line separates the section from the stop hint.
        assert!(b.contains("\n\n⏹️"), "{b:?}");
        assert!(b.trim_end().ends_with("⏹️   Ctrl-C to stop"), "{b:?}");
    }

    #[test]
    fn banner_no_ui_placeholder_line_preserved() {
        // The "none — API only" description still flows into the Web UI row.
        let b = build_banner(
            "http://127.0.0.1:6789/?token=x",
            "/v",
            "no UI — API only (placeholder page)",
        );
        assert!(
            b.contains("    Web UI        no UI — API only (placeholder page)"),
            "{b:?}"
        );
    }

    #[test]
    fn banner_rows_share_one_value_column() {
        // Every value must start at the same column (4 + LABEL_W) so the
        // banner lines up regardless of label length.
        let b = build_banner("URLVAL", "VAULTVAL", "UIVAL");
        for (needle, value) in [
            ("    URL", "URLVAL"),
            ("    Vault", "VAULTVAL"),
            ("    Web UI", "UIVAL"),
        ] {
            let line = b
                .lines()
                .find(|l| l.starts_with(needle))
                .unwrap_or_else(|| panic!("missing {needle} row in {b:?}"));
            assert_eq!(
                line.find(value),
                Some(4 + crate::output::LABEL_W),
                "value column drifted: {line:?}"
            );
        }
    }
}
