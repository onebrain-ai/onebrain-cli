//! Warm-daemon discovery + client library.
//!
//! redb is single-process, so exactly ONE process (the daemon) opens the
//! search engine; the CLI `onebrain search …` verbs and the native MCP server
//! become *clients* that talk to it over the existing localhost HTTP surface
//! (`/api/vault/search`, `/api/internal/*`) with the daemon's token. This
//! module is that reusable client — the mcp + CLI tracks call it; nothing here
//! opens an engine itself.
//!
//! ## Discovery file — `~/.onebrain/run/daemon.json`
//! The daemon writes `{ port, token, pid, version }` after it binds (see
//! [`DaemonInfo::write`]); a clean shutdown removes it. Clients read it,
//! liveness-probe the daemon, and connect. A stale file (daemon dead, or a
//! version mismatch) is cleaned up / triggers a restart rather than trusted.
//!
//! ## Entry points
//! - [`discover`] — read + liveness-probe an already-running daemon, else
//!   `None` (and clean up a stale discovery file).
//! - [`ensure_running`] — [`discover`], else spawn `daemon __run` detached and
//!   poll until it's live; handles the start race (someone else won → connect).
//! - [`DaemonHandle::search`] / [`reindex`](DaemonHandle::reindex) /
//!   [`status`](DaemonHandle::status) — typed HTTP calls carrying the token.
//!
//! ## Version skew
//! A daemon from an older/newer CLI may speak a different wire shape, so
//! [`discover`] treats a `version` mismatch as "must restart": it stops the old
//! daemon and (via [`ensure_running`]) starts one at our version before use.
//!
//! ## Dead-code allow
//! This is a REUSABLE client library: the daemon side (this PR) only calls
//! [`DaemonInfo::write`]/[`remove`](DaemonInfo::remove); the *consumers* of
//! `discover`/`ensure_running`/[`DaemonHandle`] are the mcp + CLI-search tracks
//! that land separately. Until they wire in, most of this module reads as dead
//! code, so we allow it here rather than sprinkle per-item `#[allow]`s or gate
//! the API behind a feature — the unit tests below exercise the core paths so
//! it isn't untested dead code.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The CLI's own version — the daemon stamps it into `daemon.json`, and a
/// client compares against it to detect version skew.
fn own_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Directory holding the daemon's runtime files: `~/.onebrain/run/`.
///
/// Kept in lockstep with `commands::daemon`'s private `run_dir()` (both key off
/// `dirs::home_dir`), so the client reads the same `daemon.json` the daemon
/// writes. Honours `$HOME` on Unix / `%USERPROFILE%` on Windows.
fn run_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("resolve home directory for daemon run dir")?;
    Ok(home.join(".onebrain").join("run"))
}

/// Path to the discovery file: `~/.onebrain/run/daemon.json`.
pub fn discovery_path() -> Result<PathBuf> {
    Ok(run_dir()?.join("daemon.json"))
}

/// The daemon's discovery record, published to `daemon.json` after it binds and
/// removed on clean shutdown. `port` is the ACTUAL bound port (may differ from
/// the requested one when binding `0`); `version` is the daemon CLI's own
/// version, used for skew detection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub version: String,
}

impl DaemonInfo {
    /// Serialize + atomically write `daemon.json` (write to a temp sibling then
    /// rename) with owner-only (0600) perms on Unix — the token is a
    /// credential, so the file must not be world-readable. Creates the run dir
    /// (0700) first. Called by the daemon right after it binds.
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            ensure_private_run_dir(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).context("serialize daemon.json")?;
        // Temp sibling + rename → readers never observe a half-written file.
        let tmp = path.with_extension("json.tmp");
        {
            use std::fs::OpenOptions;
            let mut opts = OpenOptions::new();
            opts.create(true).write(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts
                .open(&tmp)
                .with_context(|| format!("create {}", tmp.display()))?;
            use std::io::Write;
            f.write_all(&json)
                .with_context(|| format!("write {}", tmp.display()))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).ok();
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Read + parse `daemon.json`. Returns `Ok(None)` for a missing file (no
    /// daemon has run) and an `Err` for a present-but-corrupt file so callers
    /// can distinguish "nothing there" from "there's junk to clean up".
    pub fn read(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let info: DaemonInfo =
                    serde_json::from_slice(&bytes).context("parse daemon.json")?;
                Ok(Some(info))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("read daemon.json"),
        }
    }

    /// Remove `daemon.json` if present (idempotent — a missing file is success).
    /// The daemon calls this on clean shutdown; clients call it to clear a
    /// stale record.
    pub fn remove(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("remove daemon.json"),
        }
    }

    /// The base URL a client hits: `http://127.0.0.1:<port>`.
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Create the daemon run dir with private (0700) perms on Unix — same policy as
/// `commands::daemon`'s `ensure_private_run_dir`, duplicated here so the client
/// doesn't reach into that module's private fns. On non-Unix, a plain mkdir.
fn ensure_private_run_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create daemon run dir {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok();
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create daemon run dir {}", dir.display()))
    }
}

/// Decide what a client should do given a discovered record's version vs ours.
/// Pure (no I/O) so the skew policy is unit-testable in isolation.
#[derive(Debug, PartialEq, Eq)]
pub enum VersionDecision {
    /// Versions match — use the running daemon as-is.
    Use,
    /// Versions differ — stop the old daemon and start one at our version.
    Restart,
}

/// Compare a discovered daemon's `version` against ours.
pub fn version_decision(daemon_version: &str, own: &str) -> VersionDecision {
    if daemon_version == own {
        VersionDecision::Use
    } else {
        VersionDecision::Restart
    }
}

/// A live, verified connection to the daemon: the discovery record plus a
/// pre-built HTTP agent. All calls carry the token and target `127.0.0.1`.
#[derive(Debug, Clone)]
pub struct DaemonHandle {
    info: DaemonInfo,
    agent: ureq::Agent,
}

/// How long a client waits on any single daemon HTTP call before giving up.
/// Generous: a cold hybrid embed on the daemon side can take seconds.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`ensure_running`] polls for a freshly-spawned daemon to publish
/// `daemon.json` + answer a liveness probe before failing.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Liveness-probe timeout — a health check must be quick or the daemon is
/// wedged and should be treated as dead.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

impl DaemonHandle {
    fn new(info: DaemonInfo) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(CLIENT_TIMEOUT))
            .build()
            .into();
        Self { info, agent }
    }

    /// The discovery record this handle wraps.
    pub fn info(&self) -> &DaemonInfo {
        &self.info
    }

    /// `GET /api/internal/status` with a SHORT probe timeout — the liveness
    /// check. `true` iff the daemon answered 2xx.
    fn is_live(&self) -> bool {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(PROBE_TIMEOUT))
            .build()
            .into();
        let url = format!("{}/api/internal/status", self.info.base_url());
        agent
            .get(&url)
            .header("x-onebrain-token", &self.info.token)
            .call()
            .is_ok()
    }

    /// `GET /api/internal/status` → parsed JSON value. The daemon returns the
    /// `search status` shape (`doc_count`, `pending_*`, `last_indexed`,
    /// `indexed`).
    pub fn status(&self) -> Result<serde_json::Value> {
        let url = format!("{}/api/internal/status", self.info.base_url());
        let mut resp = self
            .agent
            .get(&url)
            .header("x-onebrain-token", &self.info.token)
            .call()
            .context("daemon status request")?;
        read_json(&mut resp)
    }

    /// `POST /api/internal/reindex` with `{ mode, paths? }`. `paths` is required
    /// for `mode = "paths"`. Returns the daemon's reindex-result JSON.
    pub fn reindex(&self, mode: &str, paths: &[String]) -> Result<serde_json::Value> {
        let url = format!("{}/api/internal/reindex", self.info.base_url());
        let body = serde_json::json!({ "mode": mode, "paths": paths });
        let payload = serde_json::to_string(&body).context("serialize reindex body")?;
        let mut resp = self
            .agent
            .post(&url)
            .header("x-onebrain-token", &self.info.token)
            .header("content-type", "application/json")
            .send(payload)
            .context("daemon reindex request")?;
        read_json(&mut resp)
    }

    /// `GET /api/vault/search?q=&mode=` → the webui search response JSON.
    pub fn search(&self, query: &str, mode: &str) -> Result<serde_json::Value> {
        let url = format!(
            "{}/api/vault/search?q={}&mode={}",
            self.info.base_url(),
            urlencode(query),
            urlencode(mode),
        );
        let mut resp = self
            .agent
            .get(&url)
            .header("x-onebrain-token", &self.info.token)
            .call()
            .context("daemon search request")?;
        read_json(&mut resp)
    }
}

/// Read a ureq response body as JSON. ureq's `json` feature is off (the crate
/// parses via serde_json directly), so read the body to a string and parse it.
fn read_json(resp: &mut ureq::http::Response<ureq::Body>) -> Result<serde_json::Value> {
    let body = resp
        .body_mut()
        .read_to_string()
        .context("read daemon response body")?;
    serde_json::from_str(&body).context("parse daemon response JSON")
}

/// Minimal percent-encoding for a query-string value: encodes everything that
/// isn't an unreserved char. Kept local so the client pulls in no url crate.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Discover an already-running daemon, or `None`.
///
/// Reads `daemon.json`; if present and live AND at our version, returns a
/// handle. A stale record (parse error, dead daemon) is cleaned up and yields
/// `None`. A version mismatch stops the old daemon, cleans the record, and
/// yields `None` so a caller's [`ensure_running`] starts a fresh one at our
/// version (see [`version_decision`]).
pub fn discover() -> Result<Option<DaemonHandle>> {
    let path = discovery_path()?;
    let info = match DaemonInfo::read(&path) {
        Ok(Some(info)) => info,
        // No file → no daemon. A corrupt file → clean it and treat as none.
        Ok(None) => return Ok(None),
        Err(_) => {
            let _ = DaemonInfo::remove(&path);
            return Ok(None);
        }
    };

    if version_decision(&info.version, own_version()) == VersionDecision::Restart {
        tracing::info!(
            daemon_version = %info.version,
            own_version = own_version(),
            "daemon version skew; restarting daemon"
        );
        // Best-effort stop of the mismatched daemon, then clear its record.
        let _ = stop_daemon();
        let _ = DaemonInfo::remove(&path);
        return Ok(None);
    }

    let handle = DaemonHandle::new(info);
    if handle.is_live() {
        Ok(Some(handle))
    } else {
        // Stale record: daemon named in the file is gone. Clean up.
        let _ = DaemonInfo::remove(&path);
        Ok(None)
    }
}

/// Ensure a daemon is running at our version and return a connected handle.
///
/// Fast path: [`discover`] returns an existing live daemon. Otherwise spawn
/// `onebrain daemon start` (detached; the existing self-respawn path) and poll
/// for `daemon.json` + liveness up to [`START_TIMEOUT`]. The start race is
/// handled implicitly: if a concurrent starter won, its `daemon.json` appears
/// and we connect to it — we don't require that *we* spawned the winner.
pub fn ensure_running() -> Result<DaemonHandle> {
    if let Some(handle) = discover()? {
        return Ok(handle);
    }

    spawn_daemon_start().context("spawn daemon start")?;

    let path = discovery_path()?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        // A daemon.json at our version + live probe → connected.
        if let Ok(Some(info)) = DaemonInfo::read(&path) {
            if version_decision(&info.version, own_version()) == VersionDecision::Use {
                let handle = DaemonHandle::new(info);
                if handle.is_live() {
                    return Ok(handle);
                }
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "daemon did not become ready within {}s",
                START_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Spawn `onebrain daemon start` (which self-respawns the detached `__run`
/// child + writes the PID file). Runs the CURRENT executable so a client always
/// starts a daemon at its own version. Inherits no stdio (the child redirects
/// its own to the daemon log).
fn spawn_daemon_start() -> Result<()> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let status = std::process::Command::new(exe)
        .args(["daemon", "start"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("run `onebrain daemon start`")?;
    if !status.success() {
        anyhow::bail!("`onebrain daemon start` exited with {status}");
    }
    Ok(())
}

/// Stop a running daemon via `onebrain daemon stop` (SIGTERM + PID cleanup).
/// Used by the version-skew restart path.
fn stop_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let _ = std::process::Command::new(exe)
        .args(["daemon", "stop"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("run `onebrain daemon stop`")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample(version: &str) -> DaemonInfo {
        DaemonInfo {
            port: 6789,
            token: "tok-abcdefghijklmnop".to_string(),
            pid: 4242,
            version: version.to_string(),
        }
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        let info = sample("3.4.6");
        info.write(&path).unwrap();
        let back = DaemonInfo::read(&path).unwrap().unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn read_missing_file_is_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        assert!(DaemonInfo::read(&path).unwrap().is_none());
    }

    #[test]
    fn read_corrupt_file_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        std::fs::write(&path, "not json {{{").unwrap();
        assert!(DaemonInfo::read(&path).is_err());
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        DaemonInfo::remove(&path).unwrap(); // missing → ok
        sample("3.4.6").write(&path).unwrap();
        DaemonInfo::remove(&path).unwrap();
        assert!(DaemonInfo::read(&path).unwrap().is_none());
    }

    #[test]
    fn write_is_atomic_no_tmp_left_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        sample("3.4.6").write(&path).unwrap();
        // The temp sibling must be gone after a successful write.
        assert!(!dir.path().join("daemon.json.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn discovery_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        sample("3.4.6").write(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "daemon.json must be 0600 (holds the token)");
    }

    #[test]
    fn version_match_uses_running_daemon() {
        assert_eq!(version_decision("3.4.6", "3.4.6"), VersionDecision::Use);
    }

    #[test]
    fn version_mismatch_requests_restart() {
        assert_eq!(version_decision("3.4.5", "3.4.6"), VersionDecision::Restart);
        assert_eq!(version_decision("3.5.0", "3.4.6"), VersionDecision::Restart);
    }

    #[test]
    fn urlencode_escapes_reserved_and_spaces() {
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
        assert_eq!(urlencode("plain-Text_1.0~"), "plain-Text_1.0~");
        assert_eq!(urlencode("คำ"), "%E0%B8%84%E0%B8%B3");
    }

    // Client fallback: with no daemon.json present, discover() returns None
    // (no daemon), so a caller falls back to opening the engine directly. We
    // point HOME at an empty tempdir so `discovery_path()` resolves under it and
    // finds nothing — never touching the real ~/.onebrain.
    #[cfg(unix)]
    #[test]
    fn discover_returns_none_when_no_daemon() {
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_var("HOME", home.path());
        assert!(discover().unwrap().is_none());
    }
}
