//! `onebrain serve [--dir <dist>] [--port N] [--open]`
//!
//! Foreground, ephemeral HTTP surface for the current session. Brings up ONE
//! local listener that serves a static web dist (SPA) + the read-only vault
//! JSON API, then blocks until Ctrl-C.
//!
//! Shares the entire server with `daemon __run` via [`crate::server`] — the
//! only difference is the shutdown trigger (Ctrl-C here, SIGTERM there). See
//! the build-level design `2026-06-04-daemon-serve-design.md` §2–4.
//!
//! **Daemon-aware since v3.4.8 (#197); reuse-or-start since v3.4.12 (#258);
//! per-vault slots since v3.4.13 (#230):**
//! `serve` reuses the daemon already serving THIS vault, and — when none is
//! running — STARTS one (restarting a stale/version-mismatched one) via
//! [`daemon_client::ensure_running`], then prints THAT daemon's webui URL (read
//! from its slot — the daemon binds an ephemeral port, never a fixed 6789),
//! honours `--open`, and exits. It never binds a second listener next to the
//! same vault's daemon (both would want that vault's engine). Two DIFFERENT
//! vaults' daemons coexisting on their own ephemeral ports is now CORRECT, not a
//! collision. Explicit `--port` / `--dir` flags (or a set `$ONEBRAIN_BIND`) —
//! and `$ONEBRAIN_NO_DAEMON` — always mean a foreground standalone server (see
//! [`plan_serve`]).
//!
//! **Localhost-only since v3.4.8 (#205):** the `--host` flag was REMOVED —
//! every listener binds `127.0.0.1`, same as the daemon. Remote access goes
//! through an encrypted tunnel (see docs/daemon.md § Remote access). The one
//! escape hatch is the `$ONEBRAIN_BIND` env var (containers: loopback inside
//! Docker is unreachable from the host), which prints a loud plaintext-HTTP
//! warning when it names a non-loopback address.

use crate::cli::ServeArgs;
use crate::commands::daemon_client::{self, DaemonInfo};
use crate::output::{item, section, OutputMode};
use crate::server::{self, resolve_token, ServeConfig};
use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr};

/// Default port for a STANDALONE foreground `serve` when `--port` is omitted.
/// `6789` is memorable (the 6-7-8-9 run) and avoids the busy "round" ports —
/// 8888 (Jupyter), 5555 (Android ADB), 6666 (IRC), 3000/5173/8080 (dev servers)
/// — as well as 4317/4318 (OpenTelemetry OTLP gRPC/HTTP). This is ONLY the
/// standalone escape hatch's default: since v3.4.13 (#230) the warm daemon binds
/// an EPHEMERAL port (multiple per-vault daemons can't share a fixed one), so the
/// common reuse-or-start path never touches 6789.
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
    /// The user asked for a specific listener (`--port`/`--dir`/`$ONEBRAIN_BIND`),
    /// the daemon kill switch is set, or starting a daemon failed — run the
    /// foreground server directly.
    Standalone,
}

/// Decide between routing to a daemon and standalone serving.
///
/// `daemon` is the record of the daemon `serve` reuses or just started for this
/// vault (`daemon_client::ensure_running` — `None` only when routing is disabled,
/// the user forced a listener, or the start failed). `explicit_listener` is
/// `true` when the
/// user passed `--port` or `--dir`, or set `$ONEBRAIN_BIND`: they asked for a
/// SPECIFIC standalone listener, so a daemon never hijacks that (the
/// standalone bind will fail loudly on a port conflict rather than silently
/// rerouting).
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
/// user asked for a specific standalone listener (`--port`/`--dir`/`$ONEBRAIN_BIND`)
/// or the CLI-wide `$ONEBRAIN_NO_DAEMON` kill switch is set — the same switch
/// the search verbs honour ([`crate::commands::search_common::daemon_routing_disabled`]).
/// Extracted from [`run`] so the routing gate is unit-testable (the composition
/// with `discover_matching` is exercised live by the release-binary audit).
fn wants_daemon_routing(explicit_listener: bool) -> bool {
    !explicit_listener && !crate::commands::search_common::daemon_routing_disabled()
}

/// Read `$ONEBRAIN_BIND` — the container-only bind override (#205). A set-but-
/// empty/whitespace value counts as unset (mirrors `$ONEBRAIN_NO_DAEMON`'s
/// convention for hook-managed env blocks).
fn bind_env() -> Option<String> {
    std::env::var("ONEBRAIN_BIND")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Resolve the bind address: `127.0.0.1` unless a `$ONEBRAIN_BIND` value is
/// supplied (containers: loopback inside Docker is unreachable from the host,
/// and 0.0.0.0-in-container behind Docker's published-port NAT is the standard
/// pattern). An INVALID value is a hard error, never a silent loopback
/// fallback — a container operator must not believe they bound `0.0.0.0` when
/// they didn't. Pure (env injected) so every arm is unit-testable.
fn resolve_bind(bind: Option<&str>) -> Result<IpAddr> {
    match bind {
        None => Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        Some(raw) => raw
            .trim()
            .parse()
            .with_context(|| format!("invalid ONEBRAIN_BIND address: {raw:?}")),
    }
}

/// The plaintext-HTTP warning printed when `$ONEBRAIN_BIND` names a
/// non-loopback address. Relocated from the removed `--host` path (#205) —
/// the guard survives, only its trigger changed. Returned as a String (not
/// printed here) so the wording is unit-testable.
fn non_loopback_warning(host: &IpAddr) -> String {
    format!(
        "\n  ⚠️  WARNING: ONEBRAIN_BIND={host} exposes OneBrain beyond this machine over PLAIN HTTP.\n\
         \x20    The auth token and all vault content would travel UNENCRYPTED.\n\
         \x20    Do NOT expose this port directly. Put a TLS tunnel/proxy in front:\n\
         \x20      • Cloudflare Tunnel + Access   • Tailscale Serve   • Caddy + Let's Encrypt\n\
         \x20    Unset ONEBRAIN_BIND (loopback-only) unless you've set one up.\n\n"
    )
}

/// The banner printed when `serve` routes to an already-running daemon instead
/// of binding its own listener. Grouped-status convention, plus a hint at the
/// dashboard and the standalone escape hatch.
fn build_daemon_banner(url: &str) -> String {
    let mut out = String::new();
    // "serving" (not "already serving") — `serve` may have just STARTED this
    // daemon via ensure_running, so the banner must read correctly on both the
    // reuse and the just-started paths.
    out.push_str(&section("🌐", "Daemon serving this vault"));
    out.push('\n');
    out.push_str(&item("URL", url));
    out.push('\n');
    out.push_str(&item(
        "Hint",
        "`onebrain daemon status` for the dashboard · pass --port/--dir for a standalone server",
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

    // Daemon-aware routing (#197, self-healing since v3.4.12): `serve` reuses a
    // running daemon for THIS vault, and — when none is running — STARTS one
    // (or restarts a stale/version-mismatched one) via `ensure_running`, then
    // hands the user that daemon's webui URL. This makes `serve` "always run:
    // reuse-or-start", and the started daemon holds the engine + token cache, so
    // the Token-Gain dashboard is populated (fixes #257 for the default path)
    // instead of the old engine-less foreground standalone.
    //
    // Two engine owners would collide on redb's single-writer lock, so we never
    // bind a second listener next to a daemon. If starting a daemon genuinely
    // fails (e.g. the engine is still locked by an exiting process), we fall
    // back to a standalone foreground server rather than erroring.
    //
    // `$ONEBRAIN_NO_DAEMON` (the CLI-wide routing kill switch) skips this
    // entirely → standalone. Explicit listener flags — and a set
    // `$ONEBRAIN_BIND` (a container operator NEEDS a reachable listener, not a
    // redirect to a loopback-bound daemon) — skip it too.
    let bind_override = bind_env();
    let explicit_listener = args.port.is_some() || args.dir.is_some() || bind_override.is_some();
    let daemon_info = if wants_daemon_routing(explicit_listener) {
        match daemon_client::ensure_running(Some(&vault_root)) {
            Ok(handle) => Some(handle.info().clone()),
            Err(e) => {
                tracing::warn!(error = %e, "could not reuse or start a daemon; serving standalone");
                None
            }
        }
    } else {
        None
    };
    if let ServePlan::OpenDaemon { url } = plan_serve(daemon_info.as_ref(), explicit_listener) {
        print!("{}", build_daemon_banner(&url));
        if args.open {
            // Best-effort, same as the standalone path below.
            if let Err(e) = open_browser(&url) {
                eprintln!("⚠ Could not open your browser automatically — {e}\n💡 open the URL above manually.");
            }
        }
        return Ok(());
    }

    // Bind address: always 127.0.0.1 (#205 — the `--host` flag is gone);
    // `$ONEBRAIN_BIND` is the container-only escape hatch. Invalid values are
    // a HARD error — an operator must never believe they bound an address
    // they didn't.
    let host: IpAddr = resolve_bind(bind_override.as_deref())?;
    let port = args.port.unwrap_or(DEFAULT_PORT);
    // Honours $ONEBRAIN_TOKEN (≥32 chars, [A-Za-z0-9_-]) for a stable,
    // bookmarkable URL behind a tunnel; otherwise a fresh random per-process
    // token. A malformed pinned token is a hard error (see `resolve_token`).
    let token = resolve_token()?;

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
        // …but DO open the token cache so this standalone escape-hatch server
        // (reached only via --port/--dir/$ONEBRAIN_BIND now that the default
        // path starts a daemon) still populates the Token-Gain dashboard (#257)
        // rather than 503-ing. Best-effort: a daemon-held token.redb → the
        // routes degrade to "unavailable".
        open_token_cache: true,
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
    let vault_display = resolved.root.as_path().display().to_string();
    let open_flag = args.open;

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
        // `run_server_with`'s `on_bind` callback only fires AFTER the listener
        // has bound successfully (see `server::run_server_from_router`) — a
        // bind failure (e.g. the port is already in use) returns `Err` from
        // this call WITHOUT ever invoking `on_bind`. Printing the banner (URL
        // + auth token + "Ctrl-C to stop") from inside `on_bind`, instead of
        // before this call, is what fixes #278: a failed bind now surfaces
        // only the bind error, never a banner that implies the server came up.
        let on_bind = move |_bound: std::net::SocketAddr| {
            // Grouped-convention banner (matches `search status` / `doctor`): a
            // `🌐  Serving` section header, then indented `Label  value` rows, a
            // blank line, and the stop hint.
            print!("{}", build_banner(&url, &vault_display, &ui_line));

            // Binding beyond loopback exposes the server on the network over
            // PLAIN HTTP — the token + vault content would travel unencrypted.
            // Only `$ONEBRAIN_BIND` can get here (#205: no `--host` flag
            // anymore), so the warning guards the env path. Warn loudly and
            // point at the safe way to do it (a TLS tunnel/proxy in front).
            if !host.is_loopback() {
                eprint!("{}", non_loopback_warning(&host));
            }

            if open_flag {
                // Best-effort: a failed browser launch must not stop the server.
                if let Err(e) = open_browser(&url) {
                    eprintln!("⚠ Could not open your browser automatically — {e}\n💡 open the URL above manually.");
                }
            }
        };
        server::run_server_with(cfg, shutdown, on_bind).await
    })
    .map_err(contract_bind_error)?;

    Ok(())
}

/// Rewrite a standalone-bind failure into contract style: what happened, and
/// a concrete next step. Recognised by the stable `"bind HTTP listener"`
/// context `server::run_server_from_router` attaches right before the TCP
/// bind (#278) — every other error `run`'s `block_on` can return (a
/// mid-flight HTTP fault, a runtime build failure, ...) passes through
/// unchanged, since those already carry their own explanation and this
/// serve-specific hint wouldn't fit them.
fn contract_bind_error(e: anyhow::Error) -> anyhow::Error {
    let detail = format!("{e:#}");
    if !detail.contains("bind HTTP listener") {
        return e;
    }
    anyhow::anyhow!(
        "✗ Could not start the server — {detail}\n\
         💡 something else is already using that address — pick a different port with \
         `--port <N>`, or drop --port/--dir (and $ONEBRAIN_BIND) so `onebrain serve` reuses \
         or starts the per-vault daemon instead"
    )
}

/// Open `url` in the platform default browser (best-effort).
///
/// macOS → `open`; other Unix → `xdg-open`; Windows → `cmd /C start "" "<url>"`.
///
/// **Windows quoting (security).** `cmd.exe` re-parses its ENTIRE raw command
/// line as cmd grammar, not argv — `&`, `^`, `|` split/chain commands there,
/// and `%VAR%` expands even *inside* double quotes. `std::process::Command`
/// only auto-quotes an arg when it contains a space, so a plain `.args([...])`
/// call (the previous shape) passed the URL through unquoted for any other
/// cmd metacharacter — `&calc&` in the URL became `& calc &`, a second command
/// cmd happily ran. We now build the raw command line ourselves via
/// [`windows_start_cmdline`] and hand it to cmd with
/// [`std::os::windows::process::CommandExt::raw_arg`] so the literal quotes
/// around the URL reach cmd verbatim (`Command`'s normal arg quoting would
/// re-escape or drop them).
///
/// That still leaves three characters that defeat the quoting itself: a literal
/// `"` inside the URL closes the quote early (whatever follows becomes
/// unquoted cmd grammar again), `%NAME%` expands even *inside* quotes (cmd
/// environment-variable substitution ignores quoting), and `!` triggers
/// history expansion in certain cmd modes. [`url_safe_for_cmd`]
/// rejects all three before we ever spawn — belt-and-suspenders on top of the
/// token-charset validation in [`crate::server::resolve_token`], which already
/// keeps the only free-form component of `url` (the auth token) restricted to
/// `[A-Za-z0-9_-]`. That makes this branch unreachable in practice; the check
/// exists so "unreachable" is enforced, not assumed.
fn open_browser(url: &str) -> Result<()> {
    // Checked unconditionally (not just under `#[cfg(windows)]`) so the guard
    // is exercised by calling `open_browser` directly on any host, including
    // in CI on macOS/Linux where the Windows spawn path never compiles. It's
    // a no-op restriction on Unix (`open`/`xdg-open` take the URL as a plain
    // argv element, not through a shell), so this costs nothing there.
    if !url_safe_for_cmd(url) {
        anyhow::bail!(
            "refusing to open browser: URL contains `\"`, `%`, or `!`, which can \
             escape or expand inside a Windows `cmd /C start` command line: {url}"
        );
    }

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
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let cmdline = windows_start_cmdline(url);
        std::process::Command::new("cmd")
            .arg("/C")
            .raw_arg(&cmdline)
            .spawn()
            .with_context(|| format!("spawn `cmd /C {cmdline}`"))?;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = url;
        anyhow::bail!(
            "✗ Can't open a browser automatically — this platform isn't one of the ones \
             OneBrain knows how to launch a browser on (macOS/Linux/Windows)\n\
             💡 copy the URL above into your browser."
        )
    }
}

/// `true` when `url` is safe to embed inside a double-quoted Windows `cmd.exe`
/// command line: no `"` (would close the quote early, turning whatever
/// follows back into live cmd grammar), no `%` (cmd expands `%VAR%` even
/// *inside* double quotes, unlike `&`/`^`/`|`/etc. which quoting alone stops),
/// and no `!` (which expands under cmd delayed expansion when `DelayedExpansion=1`).
///
/// Always compiled (not `#[cfg(windows)]`) so it's unit-testable on any host;
/// only the Windows [`open_browser`] branch calls it at runtime.
fn url_safe_for_cmd(url: &str) -> bool {
    !url.contains('"') && !url.contains('%') && !url.contains('!')
}

/// `start "" "<url>"` — the raw cmd.exe command line (after `cmd /C`) to open
/// `url` in the default browser. The first quoted arg to `start` is a window
/// title; the empty `""` slot fills that so `start` doesn't mistake the URL
/// for the title. The URL itself is double-quoted so `cmd.exe`'s line parser
/// (which re-tokenizes on spaces, `&`, `^`, `|`, ...) treats it as one atomic
/// token rather than re-parsing it as further cmd syntax — see [`open_browser`]
/// for why plain argv-quoting isn't enough on Windows, and [`url_safe_for_cmd`]
/// for the three characters (`"`, `%`, `!`) that defeat quoting outright.
///
/// On non-windows the `#[cfg(windows)]` caller above is compiled out, so the
/// function is only reachable from unit tests there. `dead_code` would
/// otherwise fire on non-windows prod builds.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_start_cmdline(url: &str) -> String {
    format!("start \"\" \"{url}\"")
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

    #[test]
    fn windows_start_cmdline_keeps_empty_title_slot_and_quotes_url() {
        // `start` treats its first quoted arg as a window title; the empty
        // `""` slot prevents it from eating the URL, and the URL itself is
        // double-quoted so cmd.exe's line parser treats it as one token.
        let cmdline = windows_start_cmdline("http://127.0.0.1:7777/?token=abc");
        assert_eq!(cmdline, "start \"\" \"http://127.0.0.1:7777/?token=abc\"");
    }

    #[test]
    fn windows_start_cmdline_does_not_escape_embedded_metacharacters() {
        // The cmdline builder itself does no filtering — that's the job of
        // `url_safe_for_cmd` / the token-charset validation upstream. This
        // test documents that a `&`-bearing URL is quoted (not rejected) at
        // this layer, matching the "quoting is layer 2, charset is layer 1 +
        // the `"`/`%` bail is the belt-and-suspenders check" split.
        let cmdline = windows_start_cmdline("http://x/?token=aaaa&calc&");
        assert_eq!(cmdline, "start \"\" \"http://x/?token=aaaa&calc&\"");
    }

    #[test]
    fn url_safe_for_cmd_accepts_a_normal_token_url() {
        assert!(url_safe_for_cmd(
            "http://127.0.0.1:6789/?token=0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn url_safe_for_cmd_rejects_double_quote() {
        // A literal `"` closes the quoted arg early — whatever follows
        // becomes live, unquoted cmd grammar again.
        assert!(!url_safe_for_cmd("http://x/?token=a\"&calc&\"a"));
    }

    #[test]
    fn url_safe_for_cmd_rejects_percent() {
        // `%VAR%` expands even inside double quotes — quoting alone can't
        // neutralize it.
        assert!(!url_safe_for_cmd("http://x/?token=%PATH%"));
    }

    #[test]
    fn url_safe_for_cmd_rejects_exclamation() {
        // `!VAR!` expands under cmd delayed expansion (when `DelayedExpansion=1`)
        // even inside double quotes — quoting alone can't neutralize it.
        assert!(!url_safe_for_cmd("http://x/?token=abc!def!"));
    }

    #[test]
    fn open_browser_rejects_quote_and_percent_bearing_urls() {
        // The `"` / `%` guard runs unconditionally at the top of
        // `open_browser` (not just under `#[cfg(windows)]`), so calling it
        // directly exercises the rejection on any host, including macOS CI.
        for bad in ["http://x/?token=a\"b", "http://x/?token=%PATH%"] {
            let err = open_browser(bad).unwrap_err().to_string();
            assert!(err.contains(bad), "{bad}: {err}");
        }
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
        // `--port` / `--dir` / `$ONEBRAIN_BIND` mean "the user asked for a
        // specific standalone listener" — never silently reroute to the daemon.
        let info = daemon_info(6789, "sekret");
        assert!(matches!(
            plan_serve(Some(&info), true),
            ServePlan::Standalone
        ));
    }

    #[test]
    fn resolve_bind_defaults_to_loopback() {
        // No env → 127.0.0.1, same as the daemon. Empty/whitespace values are
        // filtered out by `bind_env` before they reach here.
        assert_eq!(resolve_bind(None).unwrap(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn resolve_bind_accepts_explicit_addresses() {
        // The container pattern: 0.0.0.0 inside Docker.
        let all = resolve_bind(Some("0.0.0.0")).unwrap();
        assert_eq!(all, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert!(!all.is_loopback());
        // IPv6 + whitespace-tolerant.
        assert!(resolve_bind(Some(" ::1 ")).unwrap().is_loopback());
    }

    #[test]
    fn resolve_bind_invalid_is_a_hard_error() {
        // Never a silent loopback fallback — the operator asked for a bind.
        for bad in ["not-an-ip", "0.0.0.0:6789", "localhost"] {
            let err = resolve_bind(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("ONEBRAIN_BIND"), "{bad}: {err}");
        }
    }

    #[test]
    fn bind_env_reads_and_filters_empty() {
        // Env-locked (crate convention): a set-but-empty value counts as unset.
        {
            let _empty = crate::test_env::set_var("ONEBRAIN_BIND", "");
            assert_eq!(bind_env(), None);
        }
        {
            let _set = crate::test_env::set_var("ONEBRAIN_BIND", "0.0.0.0");
            assert_eq!(bind_env().as_deref(), Some("0.0.0.0"));
        }
    }

    #[test]
    fn non_loopback_warning_names_env_and_risks() {
        let w = non_loopback_warning(&"0.0.0.0".parse::<IpAddr>().unwrap());
        assert!(w.contains("ONEBRAIN_BIND=0.0.0.0"), "{w}");
        assert!(w.contains("PLAIN HTTP"), "{w}");
        assert!(w.contains("UNENCRYPTED"), "{w}");
        assert!(w.contains("Tailscale Serve"), "{w}");
        // The removed flag must not resurface in the wording.
        assert!(!w.contains("--host"), "{w}");
    }

    #[test]
    fn wants_daemon_routing_honours_kill_switch_and_explicit_flags() {
        // Env-locked (crate-wide non-reentrant guard): empty = switch unset.
        {
            let _routing_on = crate::test_env::set_var("ONEBRAIN_NO_DAEMON", "");
            assert!(wants_daemon_routing(false), "default: routing wanted");
            assert!(
                !wants_daemon_routing(true),
                "--port/--dir/ONEBRAIN_BIND always means standalone"
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
        assert!(b.contains("🌐  Daemon serving this vault"), "{b:?}");
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
