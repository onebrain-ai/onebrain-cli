//! `cloudflared` driver for `onebrain gateway setup` (v3.5.2, design D3/D4).
//!
//! Two jobs:
//! - **CLI provisioning** of a locally managed tunnel on the user's own
//!   domain (`tunnel login` → `tunnel list`/`create` → `route dns` →
//!   `tunnel token`). Partially supersedes ADR 0042 D3 (which avoided
//!   `cert.pem`) — ADR 0043.
//! - **Quick Tunnel** (`cloudflared tunnel --url …`): a foreground,
//!   testing-only `*.trycloudflare.com` URL. Nothing is written to disk.
//!
//! Everything that decides something from `cloudflared`'s output is a pure
//! function ([`parse_quick_url`], [`classify_route_dns`],
//! [`parse_tunnel_list`], [`parse_created_tunnel`], [`parse_token`]) tested
//! against the fixtures in [`fixtures`]; [`SystemCloudflared`] is a thin
//! process shell over them. A tunnel token is never printed, logged, or put
//! in an error message: `token`'s stdout is only ever parsed.

// Consumed by `gateway setup` (v3.5.2 T4), which lands next and drops this.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TunnelRef {
    pub id: String,
    pub name: String,
}

/// What `cloudflared tunnel route dns <tunnel> <host>` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RouteDns {
    /// A CNAME for exactly `host` now points at the tunnel.
    Created,
    /// `host` already pointed at this tunnel (re-run).
    AlreadyRouted,
    /// Another A/AAAA/CNAME record holds `host`; only `--overwrite-dns`
    /// (an explicit "yes" from the user) replaces it.
    ConflictingRecord,
    /// `host` is not in a zone this `cert.pem` login can manage.
    /// `stray: Some(fqdn)` (red-team B2): cloudflared appended the login's
    /// zone and exited 0, CREATING a record at `fqdn` — the user must delete
    /// it in the dashboard (cloudflared cannot delete DNS records).
    /// `stray: None`: cloudflared refused and created nothing.
    WrongZone { stray: Option<String> },
    /// Anything else; the last non-empty line of cloudflared's output.
    Failed(String),
}

pub(crate) trait Cloudflared {
    /// The binary, when it exists. `None` = not installed.
    fn path(&self) -> Option<PathBuf>;
    /// An origin certificate (`cert.pem`) from `cloudflared tunnel login`
    /// exists (see [`origin_cert_candidates`]).
    fn has_login(&self) -> bool;
    /// `cloudflared tunnel login`: inherits the TTY, opens the browser.
    /// Errors if it fails or leaves no `cert.pem` behind.
    fn login(&self) -> anyhow::Result<()>;
    /// `tunnel list --name <name> -o json` → the live tunnel of that name.
    fn find_tunnel(&self, name: &str) -> anyhow::Result<Option<TunnelRef>>;
    /// `tunnel create <name>`.
    fn create_tunnel(&self, name: &str) -> anyhow::Result<TunnelRef>;
    /// `tunnel route dns [--overwrite-dns] <tunnel> <host>`, classified by
    /// [`classify_route_dns`]. Only process-level failures are `Err`;
    /// `cloudflared` refusing is `Ok(RouteDns::Failed(..))` or another
    /// variant the caller must handle.
    fn route_dns(&self, tunnel: &str, host: &str, overwrite: bool) -> anyhow::Result<RouteDns>;
    /// `tunnel token <tunnel>` → the run token. A secret: never print it.
    fn token(&self, tunnel: &str) -> anyhow::Result<String>;
    /// Start a Quick Tunnel to `http://127.0.0.1:<port>` and wait up to
    /// `timeout` for its URL. The child keeps running inside the returned
    /// [`QuickTunnel`] and is killed when that is dropped.
    fn spawn_quick(&self, port: u16, timeout: Duration) -> anyhow::Result<QuickTunnel>;
}

/// A running Quick Tunnel. Dropping it kills and reaps `cloudflared`.
pub(crate) struct QuickTunnel {
    pub url: String,
    pub child: Child,
}

impl Drop for QuickTunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        deregister_quick(self.child.id());
    }
}

/// Pids of live Quick Tunnel children. The release profile is
/// `panic = "abort"`, so a panic never runs [`QuickTunnel`]'s `Drop`; the
/// panic hook installed by [`install_quick_panic_hook`] kills these
/// instead, so a crashed wizard never leaves `cloudflared` forwarding a
/// public URL to a dead port (or, worse, to whatever binds it next).
static LIVE_QUICK: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

fn live_quick() -> std::sync::MutexGuard<'static, Vec<u32>> {
    LIVE_QUICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn register_quick(pid: u32) {
    live_quick().push(pid);
}

fn deregister_quick(pid: u32) {
    live_quick().retain(|p| *p != pid);
}

/// Best-effort kill of `pids` from inside a panic hook: no allocation-heavy
/// work, errors ignored.
/// - unix: `SIGTERM` (cloudflared shuts its connections down on it).
/// - Windows: `taskkill /PID <pid> /F` — ASSUMED to work, untested here
///   (no Windows leg runs this; a missing `taskkill` just leaves the gap
///   the panic would have left anyway).
fn kill_pids(pids: &[u32]) {
    for &pid in pids {
        #[cfg(unix)]
        // SAFETY: plain kill(2) on a pid we spawned; no memory involved.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Install (once per process) a panic hook that kills every registered
/// Quick Tunnel child, then chains to the previous hook.
fn install_quick_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // try_lock: the panic may have happened while this thread held
            // the registry lock; never deadlock the hook.
            let pids = match LIVE_QUICK.try_lock() {
                Ok(g) => g.clone(),
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner().clone(),
                Err(std::sync::TryLockError::WouldBlock) => Vec::new(),
            };
            kill_pids(&pids);
            previous(info);
        }));
    });
}

/// The tunnel name the wizard creates / reuses for `host`:
/// `onebrain-<host with dots as dashes>`, at most 64 characters. A longer
/// one is cut to 55 and suffixed `-<first 8 hex of sha256(host)>` so two
/// long hostnames sharing a prefix still get distinct tunnels.
pub(crate) fn tunnel_name_for(host: &str) -> String {
    use sha2::Digest;
    const MAX: usize = 64;
    const CUT: usize = 55;
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let full = format!("onebrain-{}", host.replace('.', "-"));
    if full.len() <= MAX {
        return full;
    }
    let digest = sha2::Sha256::digest(host.as_bytes());
    let hex: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    // `full` is ASCII (a normalised hostname), so byte 55 is a char boundary.
    format!("{}-{hex}", &full[..CUT])
}

/// `https://<label>.trycloudflare.com` — exactly one lowercase label, not
/// the `api` host (red-team M5: anchored, so `x.trycloudflare.com.evil.net`
/// or `a.b.trycloudflare.com` never match).
fn is_quick_tunnel_url(token: &str) -> bool {
    let Some(label) = token
        .strip_prefix("https://")
        .and_then(|rest| rest.strip_suffix(".trycloudflare.com"))
    else {
        return false;
    };
    !label.is_empty()
        && label != "api"
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The Quick Tunnel URL in one line of `cloudflared`'s output, if any.
/// cloudflared 2026.10.0 prints it on stderr inside a banner line
/// (`… INF |  https://<x>.trycloudflare.com   |`, probe T0a).
pub(crate) fn parse_quick_url(line: &str) -> Option<String> {
    line.split(|c: char| c.is_whitespace() || c == '|')
        .find(|t| is_quick_tunnel_url(t))
        .map(str::to_string)
}

/// Lowercase, no trailing dot — DNS names compare case-insensitively.
fn fqdn_eq(a: &str, b: &str) -> bool {
    a.trim_end_matches('.')
        .eq_ignore_ascii_case(b.trim_end_matches('.'))
}

/// The whitespace-delimited word right after `marker`.
fn word_after<'a>(output: &'a str, marker: &str) -> Option<&'a str> {
    let at = output.find(marker)? + marker.len();
    output[at..].split_whitespace().next()
}

/// The whitespace-delimited word right before `marker`.
fn word_before<'a>(output: &'a str, marker: &str) -> Option<&'a str> {
    let at = output.find(marker)?;
    output[..at].split_whitespace().next_back()
}

fn last_line(output: &str) -> String {
    let line = output
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("no output");
    line.chars().take(300).collect()
}

/// Classify `route dns` from its exit status and combined output.
/// Success is only trusted when the FQDN cloudflared names equals `host`:
/// for a hostname outside the login's zone cloudflared appends that zone
/// and exits 0 (`Added CNAME brain.other.com.example.com`, red-team B2).
pub(crate) fn classify_route_dns(exit_ok: bool, output: &str, host: &str) -> RouteDns {
    const ADDED: &str = "Added CNAME ";
    const ALREADY: &str = " is already configured to route to your tunnel";
    if exit_ok {
        let named = word_after(output, ADDED)
            .map(|f| (f, RouteDns::Created))
            .or_else(|| word_before(output, ALREADY).map(|f| (f, RouteDns::AlreadyRouted)));
        return match named {
            Some((fqdn, _)) if !fqdn_eq(fqdn, host) => RouteDns::WrongZone {
                stray: Some(fqdn.trim_end_matches('.').to_ascii_lowercase()),
            },
            Some((_, outcome)) => outcome,
            None => RouteDns::Failed(format!(
                "unrecognised `cloudflared tunnel route dns` output: {}",
                last_line(output)
            )),
        };
    }
    let lower = output.to_ascii_lowercase();
    if lower.contains("record with that host already exists") {
        RouteDns::ConflictingRecord
    } else if lower.contains("zone") {
        RouteDns::WrongZone { stray: None }
    } else {
        RouteDns::Failed(last_line(output))
    }
}

/// `tunnel list --name <name> -o json` stdout → the live tunnel named
/// exactly `name`. Empty output, `null`, `[]`, and cloudflared's "No
/// tunnels were found" sentence all mean none (red-team M8).
pub(crate) fn parse_tunnel_list(stdout: &str, name: &str) -> anyhow::Result<Option<TunnelRef>> {
    let t = stdout.trim();
    if t.is_empty() || t == "null" || t.starts_with("No tunnels") {
        return Ok(None);
    }
    let list: Vec<serde_json::Value> = serde_json::from_str(t).map_err(|_| {
        anyhow::anyhow!(
            "unexpected `cloudflared tunnel list -o json` output: {}",
            last_line(t)
        )
    })?;
    let deleted = |v: &serde_json::Value| {
        v.get("deleted_at")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|d| !d.is_empty() && !d.starts_with("0001-01-01"))
    };
    Ok(list
        .iter()
        .filter(|v| v.get("name").and_then(serde_json::Value::as_str) == Some(name))
        .filter(|v| !deleted(v))
        .find_map(|v| {
            Some(TunnelRef {
                id: v.get("id")?.as_str()?.to_string(),
                name: name.to_string(),
            })
        }))
}

/// `tunnel create <name>` output → the new tunnel, from its
/// `Created tunnel <name> with id <uuid>` line.
pub(crate) fn parse_created_tunnel(output: &str, name: &str) -> Option<TunnelRef> {
    let id = word_after(output, &format!("Created tunnel {name} with id "))?;
    let is_uuid = id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    is_uuid.then(|| TunnelRef {
        id: id.to_string(),
        name: name.to_string(),
    })
}

/// `tunnel token` stdout → the token: one non-empty word. The error never
/// echoes stdout (it is, or holds, the secret).
pub(crate) fn parse_token(stdout: &str) -> anyhow::Result<String> {
    let mut words = stdout.split_whitespace();
    match (words.next(), words.next()) {
        (Some(token), None) => Ok(token.to_string()),
        _ => anyhow::bail!("`cloudflared tunnel token` did not print exactly one token"),
    }
}

/// Where `cloudflared` looks for its origin certificate, in order:
/// `TUNNEL_ORIGIN_CERT` when set (then ONLY that path, as cloudflared
/// itself does), else `~/.cloudflared/cert.pem` and, on unix,
/// `/etc/cloudflared/cert.pem` (red-team M7).
pub(crate) fn origin_cert_candidates(
    env_cert: Option<std::ffi::OsString>,
    home: &Path,
) -> Vec<PathBuf> {
    if let Some(p) = env_cert.filter(|p| !p.is_empty()) {
        return vec![PathBuf::from(p)];
    }
    let mut v = vec![home.join(".cloudflared").join("cert.pem")];
    if cfg!(unix) {
        v.push(PathBuf::from("/etc/cloudflared/cert.pem"));
    }
    v
}

/// The real `cloudflared`, run as `bin` (explicit path: no PATH lookup or
/// mutation in tests, red-team I6).
pub(crate) struct SystemCloudflared {
    pub bin: PathBuf,
    pub home: PathBuf,
    /// Bound on each one-shot call (`tunnel list` / `create` / `route dns`
    /// / `token`), which talk to the Cloudflare API and could otherwise hang
    /// on a stalled network. [`ONE_SHOT_TIMEOUT`] in production; tests
    /// shorten it. Not applied to `login` (it waits for the browser) or to
    /// `spawn_quick` (its own `timeout` argument).
    pub timeout: Duration,
}

/// Default bound on a one-shot `cloudflared` call.
pub(crate) const ONE_SHOT_TIMEOUT: Duration = Duration::from_secs(60);

/// Read `stream` to EOF on a thread (so a chatty child never blocks on a
/// full pipe while we wait for it).
fn collect(stream: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stream {
            let _ = s.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

impl SystemCloudflared {
    /// `cloudflared` from PATH; when absent, [`Cloudflared::path`] is `None`.
    pub(crate) fn discover(home: PathBuf) -> Self {
        Self::from_lookup(which::which("cloudflared").ok(), home)
    }

    /// Not found → an EMPTY `bin` (never a relative `cloudflared`, which
    /// `path()` could resolve against a stray `./cloudflared` in the cwd),
    /// so [`Cloudflared::path`] is `None`.
    fn from_lookup(found: Option<PathBuf>, home: PathBuf) -> Self {
        Self {
            bin: found.unwrap_or_default(),
            home,
            timeout: ONE_SHOT_TIMEOUT,
        }
    }

    /// Run one `cloudflared` call to completion, at most `self.timeout`.
    /// On timeout the child is killed and reaped; its output readers are
    /// left to finish on their own (a grandchild may still hold the pipe).
    fn output(&self, what: &str, args: &[&str]) -> anyhow::Result<Output> {
        use wait_timeout::ChildExt;
        let mut child = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("run `cloudflared {what}` ({})", self.bin.display()))?;
        let stdout = collect(child.stdout.take());
        let stderr = collect(child.stderr.take());
        let status = match child
            .wait_timeout(self.timeout)
            .with_context(|| format!("wait for `cloudflared {what}`"))?
        {
            Some(status) => status,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "`cloudflared {what}` did not finish within {:?} — check the network connection and retry",
                    self.timeout
                );
            }
        };
        // The child has exited, so its pipes are closed (barring a
        // grandchild); bound the wait anyway.
        let read = |rx: mpsc::Receiver<Vec<u8>>| rx.recv_timeout(self.timeout).unwrap_or_default();
        Ok(Output {
            status,
            stdout: read(stdout),
            stderr: read(stderr),
        })
    }
}

/// `cloudflared <what>` failed: say so with the last stderr line (never
/// stdout — for `tunnel token` that is the secret).
fn failed(what: &str, out: &Output) -> anyhow::Error {
    anyhow::anyhow!(
        "`cloudflared {what}` failed ({}): {}",
        out.status,
        last_line(&String::from_utf8_lossy(&out.stderr))
    )
}

/// Forward each line of `stream` that carries a Quick Tunnel URL to `tx`,
/// then keep draining to EOF (red-team I5): a reader that stopped would
/// fill the pipe and block cloudflared mid-session.
fn drain_lines(stream: impl Read, tx: mpsc::Sender<String>) {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    let mut sent = false;
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if !sent {
            if let Some(url) = parse_quick_url(&String::from_utf8_lossy(&buf)) {
                // A closed receiver (spawn_quick already returned) is fine.
                let _ = tx.send(url);
                sent = true;
            }
        }
    }
}

impl Cloudflared for SystemCloudflared {
    fn path(&self) -> Option<PathBuf> {
        (!self.bin.as_os_str().is_empty() && self.bin.is_file()).then(|| self.bin.clone())
    }

    fn has_login(&self) -> bool {
        origin_cert_candidates(std::env::var_os("TUNNEL_ORIGIN_CERT"), &self.home)
            .iter()
            .any(|p| p.is_file())
    }

    /// Deliberately unbounded (no [`SystemCloudflared::timeout`]): it waits
    /// for the user to finish the browser step, which takes as long as it
    /// takes; Ctrl-C ends it.
    fn login(&self) -> anyhow::Result<()> {
        let status = Command::new(&self.bin)
            .args(["tunnel", "login"])
            .status()
            .with_context(|| format!("run `cloudflared tunnel login` ({})", self.bin.display()))?;
        if !status.success() {
            anyhow::bail!("`cloudflared tunnel login` failed ({status})");
        }
        if !self.has_login() {
            anyhow::bail!(
                "`cloudflared tunnel login` finished but left no cert.pem (was the browser step completed?)"
            );
        }
        Ok(())
    }

    fn find_tunnel(&self, name: &str) -> anyhow::Result<Option<TunnelRef>> {
        let what = "tunnel list";
        let out = self.output(what, &["tunnel", "list", "--name", name, "-o", "json"])?;
        if !out.status.success() {
            return Err(failed(what, &out));
        }
        parse_tunnel_list(&String::from_utf8_lossy(&out.stdout), name)
    }

    fn create_tunnel(&self, name: &str) -> anyhow::Result<TunnelRef> {
        let what = "tunnel create";
        let out = self.output(what, &["tunnel", "create", name])?;
        if !out.status.success() {
            return Err(failed(what, &out));
        }
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        parse_created_tunnel(&text, name).ok_or_else(|| {
            anyhow::anyhow!(
                "`cloudflared tunnel create {name}` succeeded but printed no tunnel id: {}",
                last_line(&text)
            )
        })
    }

    fn route_dns(&self, tunnel: &str, host: &str, overwrite: bool) -> anyhow::Result<RouteDns> {
        let mut args = vec!["tunnel", "route", "dns"];
        if overwrite {
            args.push("--overwrite-dns");
        }
        args.extend([tunnel, host]);
        let out = self.output("tunnel route dns", &args)?;
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(classify_route_dns(out.status.success(), &text, host))
    }

    fn token(&self, tunnel: &str) -> anyhow::Result<String> {
        let what = "tunnel token";
        let out = self.output(what, &["tunnel", "token", tunnel])?;
        if !out.status.success() {
            return Err(failed(what, &out));
        }
        parse_token(&String::from_utf8_lossy(&out.stdout))
    }

    fn spawn_quick(&self, port: u16, timeout: Duration) -> anyhow::Result<QuickTunnel> {
        let origin = format!("http://127.0.0.1:{port}");
        install_quick_panic_hook();
        let mut child = Command::new(&self.bin)
            .args([
                "tunnel",
                "--no-autoupdate",
                "--protocol",
                "http2",
                "--url",
                &origin,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start `cloudflared tunnel` ({})", self.bin.display()))?;
        let (tx, rx) = mpsc::channel();
        if let Some(s) = child.stdout.take() {
            let tx = tx.clone();
            std::thread::spawn(move || drain_lines(s, tx));
        }
        if let Some(s) = child.stderr.take() {
            let tx = tx.clone();
            std::thread::spawn(move || drain_lines(s, tx));
        }
        drop(tx);
        // Owned by the guard from here on: every early return kills it
        // (and deregisters it).
        register_quick(child.id());
        let mut quick = QuickTunnel {
            url: String::new(),
            child,
        };
        match rx.recv_timeout(timeout) {
            Ok(url) => {
                quick.url = url;
                Ok(quick)
            }
            // `quick` drops here: cloudflared is killed and reaped.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                anyhow::bail!("cloudflared did not report a Quick Tunnel URL within {timeout:?}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("cloudflared exited before reporting a Quick Tunnel URL")
            }
        }
    }
}

/// `cloudflared` output samples the parsers are tested against. Shared with
/// the wizard's tests (T4) so a corrected sample fixes both.
///
/// Provenance: `QUICK_*` are VERBATIM from the T0a live probe (2026-10-11,
/// cloudflared 2026.10.0). Everything else is `ASSUMED until T0b` — written
/// from knowledge of cloudflared's output, not captured; the T0b probe
/// (owner's Cloudflare login) replaces them verbatim.
#[cfg(test)]
pub(crate) mod fixtures {
    /// T0a verbatim (stderr): the banner line carrying the URL.
    pub(crate) const QUICK_URL_LINE: &str = "2026-10-11T10:17:29Z INF |  https://ross-amanda-fisher-champion.trycloudflare.com                                     |";
    pub(crate) const QUICK_URL: &str = "https://ross-amanda-fisher-champion.trycloudflare.com";
    /// T0a: the line before the URL (must NOT parse). Message text verbatim;
    /// the timestamp prefix is illustrative.
    pub(crate) const QUICK_REQUESTING_LINE: &str =
        "2026-10-11T10:17:25Z INF Requesting new quick Tunnel on trycloudflare.com...";

    // ASSUMED until T0b — `tunnel create onebrain-brain-example-com`.
    pub(crate) const CREATE_OUTPUT: &str = "Tunnel credentials written to /Users/x/.cloudflared/6ff42ae2-765d-4adf-8112-31c55c1551ef.json. cloudflared chose this file based on where your origin certificate was found. Keep this file secret. To revoke these credentials, delete the tunnel.\n\nCreated tunnel onebrain-brain-example-com with id 6ff42ae2-765d-4adf-8112-31c55c1551ef\n";
    pub(crate) const CREATED_ID: &str = "6ff42ae2-765d-4adf-8112-31c55c1551ef";

    // ASSUMED until T0b — `tunnel list --name onebrain-brain-example-com -o json` (match).
    pub(crate) const LIST_JSON_MATCH: &str = r#"[{"id":"6ff42ae2-765d-4adf-8112-31c55c1551ef","name":"onebrain-brain-example-com","created_at":"2026-10-11T10:30:00Z","deleted_at":"0001-01-01T00:00:00Z","connections":[]}]"#;
    // ASSUMED until T0b — `tunnel list --name does-not-exist -o json` (no match).
    pub(crate) const LIST_JSON_EMPTY: &str = "[]\n";
    // ASSUMED until T0b — the human-readable "nothing found" sentence.
    pub(crate) const LIST_NONE_TEXT: &str = "No tunnels were found for the given filter flags. You can use 'cloudflared tunnel create' to create a tunnel.\n";

    // ASSUMED until T0b — `tunnel route dns <t> brain.example.com`, created (exit 0).
    pub(crate) const ROUTE_ADDED: &str = "2026-10-11T10:31:00Z INF Added CNAME brain.example.com which will route to this tunnel tunnelID=6ff42ae2-765d-4adf-8112-31c55c1551ef\n";
    // ASSUMED until T0b — same command re-run (exit 0).
    pub(crate) const ROUTE_ALREADY: &str = "2026-10-11T10:31:30Z INF brain.example.com is already configured to route to your tunnel tunnelID=6ff42ae2-765d-4adf-8112-31c55c1551ef\n";
    // ASSUMED until T0b — another record holds the name (exit 1).
    pub(crate) const ROUTE_CONFLICT: &str = "Failed to add route: code: 1003, reason: Failed to create record brain.example.com with err An A, AAAA, or CNAME record with that host already exists. For more details, refer to <https://developers.cloudflare.com/dns/manage-dns-records/troubleshooting/records-with-same-name/>.\n";
    // ASSUMED until T0b — `route dns <t> brain.other.com` with a cert for
    // example.com: zone appended, record created, exit 0 (red-team B2).
    pub(crate) const ROUTE_WRONG_ZONE_APPENDED: &str = "2026-10-11T10:32:00Z INF Added CNAME brain.other.com.example.com which will route to this tunnel tunnelID=6ff42ae2-765d-4adf-8112-31c55c1551ef\n";
    // ASSUMED until T0b — a zone refusal (exit 1).
    pub(crate) const ROUTE_ZONE_REFUSED: &str =
        "Failed to add route: code: 1003, reason: zone not found for brain.other.com\n";

    /// ASSUMED until T0b — `tunnel token` stdout: one base64 line. This is
    /// the repo's existing fake token (base64 JSON with a non-secret `s`).
    pub(crate) const TOKEN_STDOUT: &str = "eyJhIjoiMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYiLCJ0IjoiNmZmNDJhZTItNzY1ZC00YWRmLTgxMTItMzFjNTVjMTU1MWVmIiwicyI6ImJtOTBMV0V0Y21WaGJDMXpaV055WlhRPSJ9\n";
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn tunnel_name_for_dashes_the_host() {
        assert_eq!(
            tunnel_name_for("brain.example.com"),
            "onebrain-brain-example-com"
        );
        assert_eq!(
            tunnel_name_for("Brain.Example.COM."),
            "onebrain-brain-example-com"
        );
    }

    /// Red-team M8: ≤ 64 chars; a cut name keeps a hash of the full host so
    /// two long hosts sharing the first 46 characters still differ.
    #[test]
    fn tunnel_name_for_a_long_host_is_cut_to_64_with_a_hash() {
        let exact = format!("{}.com", "a".repeat(51)); // onebrain- + 55 = 64
        assert_eq!(tunnel_name_for(&exact).len(), 64);
        assert!(!tunnel_name_for(&exact).contains("--"));

        let a = format!("{}.one.example.com", "x".repeat(60));
        let b = format!("{}.two.example.com", "x".repeat(60));
        let (na, nb) = (tunnel_name_for(&a), tunnel_name_for(&b));
        assert_eq!(na.len(), 64, "{na}");
        assert_ne!(na, nb);
        assert!(na.starts_with("onebrain-xxxx"), "{na}");
        let suffix = &na[56..];
        assert_eq!(na.as_bytes()[55], b'-');
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()), "{na}");
        assert_eq!(tunnel_name_for(&a), na, "deterministic");
    }

    #[test]
    fn parse_quick_url_reads_the_t0a_banner_line() {
        assert_eq!(parse_quick_url(QUICK_URL_LINE).as_deref(), Some(QUICK_URL));
    }

    #[test]
    fn parse_quick_url_ignores_other_lines_and_hosts() {
        for line in [
            QUICK_REQUESTING_LINE,
            "",
            "INF Starting tunnel tunnelID=",
            "INF |  https://api.trycloudflare.com  |",
            "INF |  https://x.trycloudflare.com.evil.net  |",
            "INF |  https://a.b.trycloudflare.com  |",
            "INF |  http://x.trycloudflare.com  |",
            "INF |  https://.trycloudflare.com  |",
            "INF |  https://X.trycloudflare.com  |",
            "INF |  https://evil.example/x.trycloudflare.com  |",
        ] {
            assert_eq!(parse_quick_url(line), None, "{line:?}");
        }
    }

    #[test]
    fn classify_route_dns_table() {
        let h = "brain.example.com";
        assert_eq!(classify_route_dns(true, ROUTE_ADDED, h), RouteDns::Created);
        assert_eq!(
            classify_route_dns(true, ROUTE_ALREADY, h),
            RouteDns::AlreadyRouted
        );
        assert_eq!(
            classify_route_dns(false, ROUTE_CONFLICT, h),
            RouteDns::ConflictingRecord
        );
        assert_eq!(
            classify_route_dns(false, ROUTE_ZONE_REFUSED, "brain.other.com"),
            RouteDns::WrongZone { stray: None }
        );
        // FQDNs compare case-insensitively, trailing dot ignored.
        assert_eq!(
            classify_route_dns(true, ROUTE_ADDED, "Brain.Example.com."),
            RouteDns::Created
        );
    }

    /// Red-team B2: exit 0 is NOT success when the record cloudflared made
    /// is for a different FQDN — that record is a stray to delete.
    #[test]
    fn classify_route_dns_flags_a_zone_appended_record_as_a_stray() {
        assert_eq!(
            classify_route_dns(true, ROUTE_WRONG_ZONE_APPENDED, "brain.other.com"),
            RouteDns::WrongZone {
                stray: Some("brain.other.com.example.com".into())
            }
        );
        // Same for an "already routed" stray from an earlier run.
        let already = ROUTE_ALREADY.replace("brain.example.com", "brain.other.com.example.com");
        assert_eq!(
            classify_route_dns(true, &already, "brain.other.com"),
            RouteDns::WrongZone {
                stray: Some("brain.other.com.example.com".into())
            }
        );
    }

    #[test]
    fn classify_route_dns_never_trusts_unrecognised_output() {
        assert_eq!(
            classify_route_dns(true, "INF something new\n", "brain.example.com"),
            RouteDns::Failed(
                "unrecognised `cloudflared tunnel route dns` output: INF something new".into()
            )
        );
        assert_eq!(
            classify_route_dns(false, "error A\n\nerror B\n", "brain.example.com"),
            RouteDns::Failed("error B".into())
        );
        assert_eq!(
            classify_route_dns(false, "", "brain.example.com"),
            RouteDns::Failed("no output".into())
        );
        // A non-zero exit with the success sentence is still a failure.
        assert!(matches!(
            classify_route_dns(false, ROUTE_ADDED, "brain.example.com"),
            RouteDns::Failed(_)
        ));
    }

    #[test]
    fn parse_tunnel_list_match_and_every_empty_shape() {
        let name = "onebrain-brain-example-com";
        assert_eq!(
            parse_tunnel_list(LIST_JSON_MATCH, name).unwrap(),
            Some(TunnelRef {
                id: CREATED_ID.into(),
                name: name.into()
            })
        );
        for none in [LIST_JSON_EMPTY, "", "  \n", "null\n", LIST_NONE_TEXT] {
            assert_eq!(parse_tunnel_list(none, name).unwrap(), None, "{none:?}");
        }
        // `--name` is a filter, not a promise: other names never match.
        assert_eq!(
            parse_tunnel_list(LIST_JSON_MATCH, "onebrain-x").unwrap(),
            None
        );
    }

    #[test]
    fn parse_tunnel_list_skips_deleted_tunnels_and_rejects_garbage() {
        let name = "t";
        let deleted = r#"[{"id":"a","name":"t","deleted_at":"2026-10-01T00:00:00Z"},{"id":"b","name":"t","deleted_at":"0001-01-01T00:00:00Z"}]"#;
        assert_eq!(parse_tunnel_list(deleted, name).unwrap().unwrap().id, "b");
        let no_field = r#"[{"id":"c","name":"t"}]"#;
        assert_eq!(parse_tunnel_list(no_field, name).unwrap().unwrap().id, "c");
        let err = parse_tunnel_list("ERR bad things", name)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ERR bad things"), "{err}");
    }

    #[test]
    fn parse_created_tunnel_reads_the_id_line() {
        let name = "onebrain-brain-example-com";
        assert_eq!(
            parse_created_tunnel(CREATE_OUTPUT, name),
            Some(TunnelRef {
                id: CREATED_ID.into(),
                name: name.into()
            })
        );
        assert_eq!(parse_created_tunnel(CREATE_OUTPUT, "other"), None);
        assert_eq!(
            parse_created_tunnel("Created tunnel t with id not-a-uuid\n", "t"),
            None
        );
    }

    #[test]
    fn parse_token_takes_one_word_and_never_echoes_stdout() {
        assert_eq!(parse_token(TOKEN_STDOUT).unwrap(), TOKEN_STDOUT.trim());
        for bad in ["", "\n", "two words-SECRETISH\n"] {
            let err = parse_token(bad).unwrap_err().to_string();
            assert!(!err.contains("SECRETISH"), "{err}");
        }
    }

    #[test]
    fn origin_cert_candidates_honour_the_env_override_then_the_defaults() {
        let home = Path::new("/h");
        assert_eq!(
            origin_cert_candidates(Some("/custom/cert.pem".into()), home),
            [PathBuf::from("/custom/cert.pem")]
        );
        let defaults = origin_cert_candidates(None, home);
        assert_eq!(defaults[0], home.join(".cloudflared").join("cert.pem"));
        // An empty env value is unset, as for every env switch here.
        assert_eq!(origin_cert_candidates(Some("".into()), home), defaults);
        #[cfg(unix)]
        assert_eq!(defaults[1], PathBuf::from("/etc/cloudflared/cert.pem"));
        #[cfg(not(unix))]
        assert_eq!(defaults.len(), 1);
    }

    #[test]
    fn system_cloudflared_path_is_none_when_the_binary_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cf = SystemCloudflared {
            bin: dir.path().join("nope"),
            home: dir.path().to_path_buf(),
            timeout: ONE_SHOT_TIMEOUT,
        };
        assert_eq!(cf.path(), None);
        let err = cf.find_tunnel("x").unwrap_err().to_string();
        assert!(err.contains("run `cloudflared tunnel list`"), "{err}");
        assert!(cf.spawn_quick(1, Duration::from_millis(10)).is_err());
    }

    #[test]
    fn discover_without_cloudflared_has_no_path_even_with_a_stray_relative_file() {
        let cf = SystemCloudflared::from_lookup(None, PathBuf::from("/h"));
        assert_eq!(cf.bin, PathBuf::new(), "never a relative `cloudflared`");
        assert_eq!(cf.path(), None);
        assert_eq!(cf.timeout, ONE_SHOT_TIMEOUT);
        let found = SystemCloudflared::from_lookup(Some("/x/cloudflared".into()), "/h".into());
        assert_eq!(found.bin, PathBuf::from("/x/cloudflared"));
    }

    #[test]
    fn quick_registry_registers_and_deregisters() {
        let pid = u32::MAX - 7; // never a real child of this test process
        register_quick(pid);
        assert!(live_quick().contains(&pid));
        deregister_quick(pid);
        assert!(!live_quick().contains(&pid));
    }

    #[test]
    fn has_login_sees_a_cert_in_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let cf = SystemCloudflared {
            bin: dir.path().join("nope"),
            home: dir.path().to_path_buf(),
            timeout: ONE_SHOT_TIMEOUT,
        };
        std::fs::create_dir_all(dir.path().join(".cloudflared")).unwrap();
        std::fs::write(dir.path().join(".cloudflared/cert.pem"), "x").unwrap();
        // Holds whatever TUNNEL_ORIGIN_CERT / /etc say only when the env
        // var is unset — the pure candidate test above covers the env path.
        if std::env::var_os("TUNNEL_ORIGIN_CERT").is_none() {
            assert!(cf.has_login());
        }
    }
}

/// [`SystemCloudflared`] against fake `cloudflared` shell scripts, run by
/// explicit path (red-team I6). Unix-only (`#!/bin/sh`): exercised on the
/// macOS and Linux CI legs, and measured by the Linux coverage gate.
/// Windows is covered by the pure tests above.
#[cfg(all(test, unix))]
mod system_tests {
    use super::fixtures::*;
    use super::*;
    use std::time::Instant;

    /// A `cloudflared` that appends its argv to `<dir>/argv.log`, then runs
    /// `body`. Returns the fake and the dir.
    fn fake(body: &str) -> (tempfile::TempDir, SystemCloudflared) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("cloudflared");
        let log = dir.path().join("argv.log");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho \"$*\" >> '{}'\n{body}\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        (
            dir,
            SystemCloudflared {
                bin,
                home,
                timeout: ONE_SHOT_TIMEOUT,
            },
        )
    }

    fn argv(dir: &tempfile::TempDir) -> Vec<String> {
        std::fs::read_to_string(dir.path().join("argv.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn sh_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only probes for existence.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[test]
    fn path_is_the_explicit_binary() {
        let (_d, cf) = fake("exit 0");
        assert_eq!(cf.path(), Some(cf.bin.clone()));
    }

    #[test]
    fn find_tunnel_runs_list_with_json_output_and_parses_it() {
        let (d, cf) = fake(&format!("printf '%s' {}", sh_quote(LIST_JSON_MATCH)));
        let t = cf.find_tunnel("onebrain-brain-example-com").unwrap();
        assert_eq!(t.unwrap().id, CREATED_ID);
        assert_eq!(
            argv(&d),
            ["tunnel list --name onebrain-brain-example-com -o json"]
        );
        let (_d, cf) = fake("echo '[]'");
        assert_eq!(cf.find_tunnel("x").unwrap(), None);
    }

    #[test]
    fn a_failing_command_names_the_stderr_line_not_stdout() {
        let (_d, cf) = fake("echo STDOUT-SECRET; echo 'ERR cert expired' >&2; exit 1");
        for err in [
            cf.find_tunnel("x").unwrap_err().to_string(),
            cf.create_tunnel("x").unwrap_err().to_string(),
            cf.token("x").unwrap_err().to_string(),
        ] {
            assert!(err.contains("ERR cert expired"), "{err}");
            assert!(!err.contains("STDOUT-SECRET"), "{err}");
        }
        // Silent stderr: the error still never falls back to stdout (for
        // `tunnel token`, stdout is the secret).
        let (_d, cf) = fake("echo STDOUT-SECRET; exit 1");
        let err = cf.token("x").unwrap_err().to_string();
        assert!(!err.contains("STDOUT-SECRET"), "{err}");
        assert!(err.contains("no output"), "{err}");
    }

    #[test]
    fn create_tunnel_parses_the_id_from_stdout_or_stderr() {
        let (d, cf) = fake(&format!("printf '%s' {} >&2", sh_quote(CREATE_OUTPUT)));
        let t = cf.create_tunnel("onebrain-brain-example-com").unwrap();
        assert_eq!(t.id, CREATED_ID);
        assert_eq!(argv(&d), ["tunnel create onebrain-brain-example-com"]);

        let (_d, cf) = fake("echo ok");
        let err = cf.create_tunnel("t").unwrap_err().to_string();
        assert!(err.contains("printed no tunnel id"), "{err}");
    }

    /// `--overwrite-dns` is passed only when asked for, before the operands.
    #[test]
    fn route_dns_passes_overwrite_only_on_request_and_classifies() {
        let (d, cf) = fake(&format!("printf '%s' {} >&2", sh_quote(ROUTE_ADDED)));
        let h = "brain.example.com";
        assert_eq!(cf.route_dns("t1", h, false).unwrap(), RouteDns::Created);
        assert_eq!(cf.route_dns("t1", h, true).unwrap(), RouteDns::Created);
        assert_eq!(
            argv(&d),
            [
                "tunnel route dns t1 brain.example.com",
                "tunnel route dns --overwrite-dns t1 brain.example.com",
            ]
        );

        let (_d, cf) = fake(&format!(
            "printf '%s' {} >&2; exit 1",
            sh_quote(ROUTE_CONFLICT)
        ));
        assert_eq!(
            cf.route_dns("t1", h, false).unwrap(),
            RouteDns::ConflictingRecord
        );
    }

    #[test]
    fn token_returns_stdout_trimmed() {
        let (d, cf) = fake(&format!("printf '%s' {}", sh_quote(TOKEN_STDOUT)));
        assert_eq!(cf.token("t1").unwrap(), TOKEN_STDOUT.trim());
        assert_eq!(argv(&d), ["tunnel token t1"]);
    }

    /// Guarded on the host having no other origin cert (TUNNEL_ORIGIN_CERT
    /// unset, no /etc/cloudflared/cert.pem) — true on CI runners.
    #[test]
    fn login_succeeds_only_when_a_cert_appears() {
        if std::env::var_os("TUNNEL_ORIGIN_CERT").is_some()
            || Path::new("/etc/cloudflared/cert.pem").exists()
        {
            return;
        }
        let (d, cf) = fake("exit 0");
        let err = cf.login().unwrap_err().to_string();
        assert!(err.contains("no cert.pem"), "{err}");
        assert_eq!(argv(&d), ["tunnel login"]);

        let (_d, cf) = fake("exit 3");
        let err = cf.login().unwrap_err().to_string();
        assert!(err.contains("login` failed"), "{err}");

        let (d, mut cf) = fake("");
        let cert = d.path().join("home/.cloudflared/cert.pem");
        std::fs::write(
            &cf.bin,
            format!(
                "#!/bin/sh\nmkdir -p {} && touch {}\n",
                sh_quote(&cert.parent().unwrap().display().to_string()),
                sh_quote(&cert.display().to_string())
            ),
        )
        .unwrap();
        cf.home = d.path().join("home");
        assert!(!cf.has_login());
        cf.login().unwrap();
        assert!(cf.has_login());
    }

    /// T0a: the URL arrives on stderr after the "Requesting" line; the
    /// child keeps running; Drop kills and reaps it.
    #[test]
    fn spawn_quick_returns_the_stderr_url_and_drop_kills_the_child() {
        let (d, cf) = fake(&format!(
            "echo {} >&2; sleep 1; echo {} >&2; exec sleep 30",
            sh_quote(QUICK_REQUESTING_LINE),
            sh_quote(QUICK_URL_LINE)
        ));
        let q = cf.spawn_quick(7717, Duration::from_secs(10)).unwrap();
        assert_eq!(q.url, QUICK_URL);
        assert_eq!(
            argv(&d),
            ["tunnel --no-autoupdate --protocol http2 --url http://127.0.0.1:7717"]
        );
        let pid = q.child.id();
        assert!(alive(pid));
        assert!(live_quick().contains(&pid), "spawn_quick must register");
        drop(q);
        assert!(!alive(pid), "Drop must kill and reap cloudflared");
        assert!(!live_quick().contains(&pid), "Drop must deregister");
    }

    /// Red-team I5: after the URL the readers keep draining, so a chatty
    /// cloudflared never blocks on a full pipe. The fake writes ~1 MB after
    /// the URL, then a marker file; the marker proves it was not blocked.
    #[test]
    fn spawn_quick_keeps_draining_output_after_the_url() {
        let marker_dir = tempfile::tempdir().unwrap();
        let marker = marker_dir.path().join("done");
        let (_d, cf) = fake(&format!(
            "echo {url} >&2\ni=0\nwhile [ $i -lt 10000 ]; do echo \"INF line $i ................................................................................\" >&2; echo \"out $i ................................................................................\"; i=$((i+1)); done\ntouch {m}\nexec sleep 30",
            url = sh_quote(QUICK_URL_LINE),
            m = sh_quote(&marker.display().to_string())
        ));
        let mut q = cf.spawn_quick(7717, Duration::from_secs(10)).unwrap();
        let start = Instant::now();
        while !marker.exists() && start.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(marker.exists(), "cloudflared blocked: output not drained");
        assert!(
            q.child.try_wait().unwrap().is_none(),
            "child must still run"
        );
    }

    #[test]
    fn spawn_quick_times_out_and_kills_a_silent_child() {
        let (d, cf) = fake("echo $$ > \"$(dirname \"$0\")/pid\"; exec sleep 30");
        let start = Instant::now();
        let err = cf
            .spawn_quick(7717, Duration::from_secs(1))
            .err()
            .unwrap()
            .to_string();
        // Plan: the timeout error arrives in < timeout + 1 s.
        let took = start.elapsed();
        assert!(took < Duration::from_secs(2), "{took:?}");
        assert!(
            err.contains("did not report a Quick Tunnel URL within 1s"),
            "{err}"
        );
        // Under load the shell may not have written its pid before the
        // kill; then it never will. Either way it must not be running.
        std::thread::sleep(Duration::from_millis(300));
        if let Ok(pid) = std::fs::read_to_string(d.path().join("pid")) {
            let pid: u32 = pid.trim().parse().unwrap();
            assert!(!alive(pid), "a timed-out cloudflared must be killed");
        }
    }

    /// Concern 2: a one-shot call that hangs (stalled network) is killed
    /// after `timeout` and named in the error. The call runs on a thread
    /// under a 10 s guard so a missing timeout fails red instead of
    /// hanging the suite.
    #[test]
    fn one_shot_calls_time_out_kill_the_child_and_name_the_command() {
        let (d, mut cf) = fake("echo $$ > \"$(dirname \"$0\")/pid\"; exec sleep 30");
        cf.timeout = Duration::from_millis(500);
        type Call = fn(&SystemCloudflared) -> anyhow::Result<()>;
        let calls: [(&str, Call); 4] = [
            ("tunnel list", |c| c.find_tunnel("t").map(drop)),
            ("tunnel create", |c| c.create_tunnel("t").map(drop)),
            ("tunnel route dns", |c| {
                c.route_dns("t", "brain.example.com", false).map(drop)
            }),
            ("tunnel token", |c| c.token("t").map(drop)),
        ];
        let cf = std::sync::Arc::new(cf);
        for (what, call) in calls {
            let _ = std::fs::remove_file(d.path().join("pid"));
            let (tx, rx) = mpsc::channel();
            let c = cf.clone();
            let start = Instant::now();
            std::thread::spawn(move || {
                let _ = tx.send(call(&c).unwrap_err().to_string());
            });
            let err = rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("`cloudflared {what}` hung: no timeout"));
            assert!(start.elapsed() < Duration::from_secs(3), "{what}");
            assert!(err.contains(&format!("`cloudflared {what}`")), "{err}");
            assert!(err.contains("did not finish within 500ms"), "{err}");
            assert!(err.contains("check the network"), "{err}");
            std::thread::sleep(Duration::from_millis(300));
            if let Ok(pid) = std::fs::read_to_string(d.path().join("pid")) {
                assert!(
                    !alive(pid.trim().parse().unwrap()),
                    "{what}: child not killed"
                );
            }
        }
    }

    /// Review fix 1: what the panic hook does — `kill_pids` on the
    /// registry snapshot — terminates a live child. (The hook itself is not
    /// triggered: a real panic would abort the test process in release.)
    #[test]
    fn panic_hook_kill_terminates_a_registered_child() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        register_quick(pid);
        let snapshot: Vec<u32> = live_quick().iter().copied().filter(|p| *p == pid).collect();
        kill_pids(&snapshot);
        let start = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break Some(s);
            }
            if start.elapsed() > Duration::from_secs(5) {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        deregister_quick(pid);
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let status = status.expect("kill_pids must terminate the child");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
    }

    #[test]
    fn install_quick_panic_hook_is_idempotent() {
        install_quick_panic_hook();
        install_quick_panic_hook();
    }

    #[test]
    fn spawn_quick_reports_a_child_that_exits_without_a_url() {
        let (_d, cf) = fake("echo 'ERR failed to request quick Tunnel' >&2; exit 1");
        let err = cf
            .spawn_quick(7717, Duration::from_secs(10))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("exited before reporting"), "{err}");
    }
}
