//! `onebrain serve [--dir <dist>] [--port N] [--host <addr>] [--open]`
//!
//! Foreground, ephemeral HTTP surface for the current session. Brings up ONE
//! local listener that serves a static web dist (SPA) + the read-only vault
//! JSON API, then blocks until Ctrl-C.
//!
//! Shares the entire server with `daemon __run` via [`crate::server`] — the
//! only difference is the shutdown trigger (Ctrl-C here, SIGTERM there). See
//! the build-level design `2026-06-04-daemon-serve-design.md` §2–4.
//!
//! **Daemon-aware since v3.4.8 (#197, design §2's "step 2b"):** when a daemon
//! is already serving THIS vault, `serve` does not bind a second listener (the
//! two share port 6789 by design, and both would want the engine) — it prints
//! the daemon's webui URL, honours `--open`, and exits. Explicit `--port` /
//! `--host` / `--dir` flags always mean a standalone server (see
//! [`plan_serve`]).

use crate::cli::ServeArgs;
use crate::commands::daemon_client::{self, DaemonInfo};
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

/// What `serve` should do, decided from the daemon landscape (see [`plan_serve`]).
#[derive(Debug)]
enum ServePlan {
    /// A live daemon already serves this vault — print its webui URL (and
    /// `--open` it); do NOT bind a second listener or open a second engine.
    OpenDaemon { url: String },
    /// No matching daemon (or the user asked for a specific listener) — start
    /// the foreground server exactly as before.
    Standalone,
}

/// Decide between routing to an existing daemon and standalone serving.
///
/// `daemon` is the discovery record of a live, version- AND vault-matched
/// daemon (`daemon_client::discover_matching` already applied every guard —
/// a mismatch arrives here as `None`). `explicit_listener` is `true` when the
/// user passed `--port`, `--host`, or `--dir`: they asked for a SPECIFIC
/// standalone listener, so a daemon never hijacks that (the standalone bind
/// will fail loudly on a port conflict rather than silently rerouting).
///
/// Extracted from [`run`] so the decision is unit-testable without sockets.
fn plan_serve(daemon: Option<&DaemonInfo>, explicit_listener: bool) -> ServePlan {
    match daemon {
        Some(info) if !explicit_listener => ServePlan::OpenDaemon {
            // Same Jupyter-style token-bearing URL shape the daemon's own
            // `daemon status` dashboard prints (the daemon always binds
            // 127.0.0.1 — see `daemon::addr_from`).
            url: format!("http://127.0.0.1:{}/?token={}", info.port, info.token),
        },
        _ => ServePlan::Standalone,
    }
}

/// Whether `serve` should even LOOK for a running daemon. `false` when the
/// user asked for a specific standalone listener (`--port`/`--host`/`--dir`)
/// or the CLI-wide `$ONEBRAIN_NO_DAEMON` kill switch is set — the same switch
/// the search verbs honour ([`crate::commands::search_common::daemon_routing_disabled`]).
/// Extracted from [`run`] so the routing gate is unit-testable (the composition
/// with `discover_matching` is exercised live by the release-binary audit).
fn wants_daemon_routing(explicit_listener: bool) -> bool {
    !explicit_listener && !crate::commands::search_common::daemon_routing_disabled()
}

/// The banner printed when `serve` routes to an already-running daemon instead
/// of binding its own listener. Grouped-status convention, plus a hint at the
/// dashboard and the standalone escape hatch.
fn build_daemon_banner(url: &str) -> String {
    let mut out = String::new();
    out.push_str(&section("🌐", "Daemon already serving this vault"));
    out.push('\n');
    out.push_str(&item("URL", url));
    out.push('\n');
    out.push_str(&item(
        "Hint",
        "`onebrain daemon status` for the dashboard · pass --port/--host/--dir for a standalone server",
    ));
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

    // Daemon-aware routing (#197): if a live daemon already serves THIS vault,
    // don't bind a second listener (shared port, and two engine owners would
    // collide on the redb lock) — hand the user the daemon's webui URL instead.
    // PASSIVE discovery only (`discover_matching`): a version/vault mismatch or
    // a dead record yields `None` and we serve standalone — `serve` never
    // starts, stops, or restarts a daemon. `$ONEBRAIN_NO_DAEMON` (the CLI-wide
    // routing kill switch) skips discovery entirely. Explicit listener flags
    // skip it too: the user asked for a specific standalone server.
    let explicit_listener = args.port.is_some() || args.host.is_some() || args.dir.is_some();
    let daemon_info = if wants_daemon_routing(explicit_listener) {
        daemon_client::discover_matching(Some(&vault_root))?.map(|handle| handle.info().clone())
    } else {
        None
    };
    if let ServePlan::OpenDaemon { url } = plan_serve(daemon_info.as_ref(), explicit_listener) {
        print!("{}", build_daemon_banner(&url));
        if args.open {
            // Best-effort, same as the standalone path below.
            if let Err(e) = open_browser(&url) {
                eprintln!("warning: could not open browser: {e}");
            }
        }
        return Ok(());
    }

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
        None => "placeholder page (this binary has no bundled web UI)".to_string(),
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
        // The no-bundle placeholder description still flows into the Web UI row.
        let b = build_banner(
            "http://127.0.0.1:6789/?token=x",
            "/v",
            "placeholder page (this binary has no bundled web UI)",
        );
        assert!(
            b.contains("    Web UI        placeholder page (this binary has no bundled web UI)"),
            "{b:?}"
        );
    }

    fn daemon_info(port: u16, token: &str) -> DaemonInfo {
        DaemonInfo {
            port,
            token: token.to_string(),
            pid: 1,
            version: "3.4.8".to_string(),
            vault: Some("/v".to_string()),
        }
    }

    #[test]
    fn plan_serve_routes_to_a_matching_daemon() {
        let info = daemon_info(6789, "sekret");
        match plan_serve(Some(&info), false) {
            ServePlan::OpenDaemon { url } => {
                assert_eq!(url, "http://127.0.0.1:6789/?token=sekret");
            }
            other => panic!("expected OpenDaemon, got {other:?}"),
        }
    }

    #[test]
    fn plan_serve_standalone_when_no_daemon() {
        assert!(matches!(plan_serve(None, false), ServePlan::Standalone));
    }

    #[test]
    fn plan_serve_explicit_listener_flags_win_over_a_daemon() {
        // `--port` / `--host` / `--dir` mean "the user asked for a specific
        // standalone listener" — never silently reroute to the daemon.
        let info = daemon_info(6789, "sekret");
        assert!(matches!(
            plan_serve(Some(&info), true),
            ServePlan::Standalone
        ));
    }

    #[test]
    fn wants_daemon_routing_honours_kill_switch_and_explicit_flags() {
        // Env-locked (crate-wide non-reentrant guard): empty = switch unset.
        {
            let _routing_on = crate::test_env::set_var("ONEBRAIN_NO_DAEMON", "");
            assert!(wants_daemon_routing(false), "default: routing wanted");
            assert!(
                !wants_daemon_routing(true),
                "--port/--host/--dir always means standalone"
            );
        }
        {
            let _killed = crate::test_env::set_var("ONEBRAIN_NO_DAEMON", "1");
            assert!(
                !wants_daemon_routing(false),
                "ONEBRAIN_NO_DAEMON disables serve's daemon detection too"
            );
            assert!(!wants_daemon_routing(true));
        }
    }

    #[test]
    fn daemon_banner_carries_url_and_hint() {
        let b = build_daemon_banner("http://127.0.0.1:6789/?token=abc");
        assert!(b.contains("🌐  Daemon already serving this vault"), "{b:?}");
        assert!(
            b.contains("    URL           http://127.0.0.1:6789/?token=abc"),
            "{b:?}"
        );
        // Points at the dashboard + the standalone escape hatch.
        assert!(b.contains("daemon status"), "{b:?}");
        assert!(b.contains("--port"), "{b:?}");
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
