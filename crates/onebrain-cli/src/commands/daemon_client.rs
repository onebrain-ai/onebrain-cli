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
//! The daemon writes `{ port, token, pid, version, vault }` after it binds (see
//! [`DaemonInfo::write`]); a clean shutdown removes it. Clients read it,
//! liveness-probe the daemon, and connect. A stale file (daemon dead, or a
//! version/vault mismatch) is cleaned up / triggers a restart rather than
//! trusted — except the PASSIVE CLI path ([`discover_matching`]), which on a
//! mismatch routes direct and LEAVES the record intact (never restarts a daemon
//! serving another vault).
//!
//! ## Entry points
//! - [`discover`] — ACTIVE: read + liveness-probe an already-running daemon,
//!   RESTARTING it on a version/vault mismatch (the MCP path owns the daemon's
//!   lifecycle); else `None` (and clean up a stale discovery file).
//! - [`discover_matching`] — PASSIVE (the CLI-search path): return a handle only
//!   for a live daemon matching BOTH version and the caller's vault; on ANY
//!   mismatch route direct (`None`) WITHOUT stopping/restarting — never disrupt
//!   a daemon serving another vault.
//! - [`ensure_running`] — [`discover`], else spawn `daemon __run` detached and
//!   poll until it's live; handles the start race (someone else won → connect).
//! - [`DaemonHandle::search`] / [`reindex`](DaemonHandle::reindex) /
//!   [`status`](DaemonHandle::status) / [`get`](DaemonHandle::get) — typed HTTP
//!   calls carrying the token.
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

/// Best-effort session-token resolution for ledger calls (design §3b). The
/// token identifies which agent session a delivery belongs to so one daemon
/// serves many sessions correctly. `None` when no token is resolvable
/// (headless / odd host) — the caller then sends `""` and the ledger is
/// silently inactive for that call; content inlines normally, never guessed.
fn resolve_session_token_opt() -> Option<String> {
    let inputs = onebrain_cache::ResolveInputs::from_env();
    onebrain_cache::resolve_session_token(&inputs)
        .ok()
        .map(|t| t.to_string())
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

/// Path to the daemon log — surfaced in the `ensure_running` timeout error so
/// the operator knows where to look.
fn log_path() -> Result<PathBuf> {
    Ok(run_dir()?.join("daemon.log"))
}

/// The daemon's discovery record, published to `daemon.json` after it binds.
/// `port` is the ACTUAL bound port (may differ from the requested one when
/// binding `0`); `version` is the daemon CLI's own version, used for skew
/// detection.
///
/// CLEANUP: removed on a clean shutdown (SIGTERM handler + `daemon stop`). A
/// HARD kill (SIGKILL / crash) leaves the record on disk — there is no
/// crash-time cleanup hook — so [`discover`] treats it as UNTRUSTED: it
/// liveness-probes and removes any stale record whose daemon no longer answers.
/// So a stale `daemon.json` is reclaimed LAZILY on the next `discover`, not
/// eagerly on death.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub version: String,
    /// The CANONICAL path of the vault this daemon bound at boot, or `None` if
    /// it bound vault-less (`$ONEBRAIN_VAULT` unset/invalid — see
    /// `daemon::resolve_daemon_vault`). The daemon is a MACHINE SINGLETON keyed
    /// on `daemon.json`, so a client that resolved a DIFFERENT vault must not
    /// silently reuse this daemon's engine — [`vault_decision`] compares this to
    /// the caller's vault and triggers a restart on mismatch, exactly like a
    /// version skew. Absent in `daemon.json` written by a pre-vault-field CLI →
    /// deserializes to `None` (treated as "unknown vault" → restart when a
    /// caller expects a specific one), so an old daemon is never trusted to be
    /// serving the right vault.
    #[serde(default)]
    pub vault: Option<String>,
}

/// Canonical identity string for a vault path — the value stored in
/// `daemon.json.vault` and compared by [`vault_decision`]. Canonicalizing both
/// the daemon's bound vault and the caller's resolved vault means a symlinked,
/// `..`-laden, or trailing-slash path variant of the SAME vault compares equal
/// (no spurious restart), while genuinely different vaults compare unequal.
/// Returns `None` when the path can't be canonicalized (e.g. it no longer
/// exists) — callers treat an uncanonicalizable expected vault as "unknown", so
/// they don't force a restart on a transient stat failure.
pub fn canonical_vault_id(path: &Path) -> Option<String> {
    path.canonicalize()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
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
            // Re-assert 0600 (the `.mode` above only applies when this call
            // CREATES the temp file; an existing one keeps its old bits). This
            // is a CREDENTIAL file — a failed chmod means it could be left
            // group/world-readable, so warn loudly rather than swallow with
            // `.ok()`; the write still succeeds (the create-time mode usually
            // held), but the operator gets a signal to investigate.
            if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!(error = %e, path = %tmp.display(),
                    "could not re-assert 0600 on daemon.json (token file may be readable)");
            }
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
        // Re-assert 0700 on a pre-existing (possibly looser) dir. It guards a
        // credential file, so warn rather than silently `.ok()` on failure.
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(error = %e, path = %dir.display(),
                "could not re-assert 0700 on daemon run dir");
        }
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

/// Decide what a client should do given a discovered daemon's bound vault vs the
/// vault the caller resolved. Pure (no I/O) so the policy is unit-testable.
///
/// The daemon is a machine singleton keyed on `daemon.json`, so if it's serving
/// vault A and the caller resolved vault B, routing B's `query`/`status` through
/// A's engine would return WRONG-VAULT results with no error. This maps that to
/// a restart, exactly like a version skew — making the v3.4.6 model explicit:
/// **one vault at a time per machine; switching vaults restarts the daemon.**
/// (Multiple concurrent per-vault daemons are deferred to the v3.8 daemon
/// refactor.)
#[derive(Debug, PartialEq, Eq)]
pub enum VaultDecision {
    /// The daemon's bound vault matches the caller's (or the caller expressed no
    /// vault expectation) — use the running daemon as-is.
    Use,
    /// The daemon is bound to a DIFFERENT vault (or bound vault-less while the
    /// caller expects one, or its record predates the vault field) — restart it
    /// for the caller's vault.
    Restart,
}

/// What vault a caller expects a daemon to be serving — the input to
/// [`vault_decision`]. Distinguishing the three cases is a safety property: an
/// [`Unresolvable`](VaultExpectation::Unresolvable) expected vault (the caller
/// HAS a vault, but it wouldn't canonicalize — e.g. the directory vanished
/// mid-operation) must NOT collapse into [`Any`](VaultExpectation::Any), or the
/// caller would silently REUSE/route to a possibly-wrong-vault daemon it can't
/// verify (a narrow wrong-vault TOCTOU). It is instead treated conservatively
/// as a mismatch.
#[derive(Debug, PartialEq, Eq)]
pub enum VaultExpectation {
    /// The caller expressed NO vault expectation (`expected_vault == None`) — it
    /// doesn't care which vault the daemon serves. Never a mismatch.
    Any,
    /// The caller HAS a vault that canonicalized to this id — match against it.
    Vault(String),
    /// The caller HAS a vault, but it wouldn't canonicalize (canonicalize failed
    /// — the dir vanished / a transient stat error). Conservatively a mismatch:
    /// the caller can't verify a daemon serves the right vault, so it must not
    /// adopt one.
    Unresolvable,
}

/// Turn a caller's expected vault path into a [`VaultExpectation`], preserving
/// the "has a vault but it wouldn't canonicalize" case rather than flattening it
/// to "no expectation". This is the seam that closes the wrong-vault TOCTOU:
/// callers previously did `expected_vault.and_then(canonical_vault_id)`, which
/// mapped BOTH `None` (no vault) AND `Some(uncanonicalizable)` to `None`, and
/// `vault_decision(_, None)` said `Use` — so a caller whose vault vanished
/// mid-op would reuse whatever daemon was up.
pub fn vault_expectation(expected_vault: Option<&Path>) -> VaultExpectation {
    match expected_vault {
        None => VaultExpectation::Any,
        Some(p) => match canonical_vault_id(p) {
            Some(id) => VaultExpectation::Vault(id),
            None => VaultExpectation::Unresolvable,
        },
    }
}

/// Compare a discovered daemon's bound vault (`daemon.json.vault`, already
/// canonicalized at write time) against the caller's [`VaultExpectation`].
///
/// - `Any` → the caller doesn't care which vault (never restarts).
/// - `Vault(v)` and the daemon's `vault == Some(v)` → `Use`.
/// - `Vault(_)` with any other daemon vault — different vault, daemon bound
///   vault-less, or an old record with no vault field (`daemon_vault == None`) —
///   → `Restart`. An old daemon is thus never trusted to be serving the right
///   vault.
/// - `Unresolvable` → `Restart`. The caller HAS a vault it can't canonicalize,
///   so it can't confirm any daemon serves the right one — treat conservatively
///   as a mismatch rather than adopt a possibly-wrong-vault daemon.
pub fn vault_decision(daemon_vault: Option<&str>, expected: &VaultExpectation) -> VaultDecision {
    match expected {
        VaultExpectation::Any => VaultDecision::Use,
        VaultExpectation::Vault(want) if daemon_vault == Some(want.as_str()) => VaultDecision::Use,
        VaultExpectation::Vault(_) | VaultExpectation::Unresolvable => VaultDecision::Restart,
    }
}

/// Why the client is still waiting for a daemon — used to turn `ensure_running`'s
/// opaque timeout into an actionable message. Pure classification (no I/O) so it
/// is unit-testable: [`classify_last_state`] maps the last-observed
/// `daemon.json` state to one of these.
#[derive(Debug, PartialEq, Eq)]
pub enum LastState {
    /// No `daemon.json` ever appeared — the spawned `daemon start` never
    /// published discovery (likely failed at bind / vault resolution).
    NotFound,
    /// A `daemon.json` at a DIFFERENT version — a stale daemon we couldn't
    /// supersede in time.
    VersionSkew { found: String, own: String },
    /// A `daemon.json` at OUR version exists, but the daemon didn't answer the
    /// liveness probe (still coming up, or bound-but-wedged).
    AliveNoResponse,
}

impl LastState {
    /// One-line, human-readable summary for the timeout error message.
    pub fn describe(&self) -> String {
        match self {
            LastState::NotFound => "no daemon.json ever appeared (start likely failed)".to_string(),
            LastState::VersionSkew { found, own } => {
                format!(
                    "found a daemon at version {found} (we are {own}) that didn't yield in time"
                )
            }
            LastState::AliveNoResponse => {
                "daemon.json at our version present but the daemon didn't answer the health probe"
                    .to_string()
            }
        }
    }
}

/// Classify the last-observed discovery state for the timeout message. Pure:
/// takes the read result + our version, returns a [`LastState`]. `read` is
/// `Ok(None)` for a missing file, `Ok(Some(info))` for a present record.
pub fn classify_last_state(read: &Option<DaemonInfo>, own: &str) -> LastState {
    match read {
        None => LastState::NotFound,
        Some(info) if version_decision(&info.version, own) == VersionDecision::Restart => {
            LastState::VersionSkew {
                found: info.version.clone(),
                own: own.to_string(),
            }
        }
        // Present + our version, yet we still timed out → it never answered.
        Some(_) => LastState::AliveNoResponse,
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

/// Timeout for `daemon status`'s best-effort dashboard probes
/// ([`DaemonHandle::probe_health`] / [`DaemonHandle::probe_status_no_retry`]).
/// Longer than [`PROBE_TIMEOUT`] because `/api/internal/status` does a
/// synchronous hash-walk of the vault, but far below [`CLIENT_TIMEOUT`] — a
/// wedged daemon must degrade the dashboard quickly, not hang the command.
const STATUS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

impl DaemonHandle {
    /// Build a handle around a discovery record. `pub(crate)` so the mcp track's
    /// tests can construct one pointing at a test-owned live server (the normal
    /// callers go through `discover`/`ensure_running`).
    pub(crate) fn new(info: DaemonInfo) -> Self {
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

    /// Liveness check with a SHORT probe timeout. Probes the ENGINE-INDEPENDENT
    /// `GET /api/health` (200 whenever the process is up) — NOT
    /// `/api/internal/status`, which 503s when the daemon holds no engine. Using
    /// the status route here would make a live-but-engine-less daemon read as
    /// dead, so `discover()` would delete its `daemon.json` and respawn it in a
    /// loop. `true` iff `/api/health` answered 2xx.
    fn is_live(&self) -> bool {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(PROBE_TIMEOUT))
            .build()
            .into();
        let url = format!("{}/api/health", self.info.base_url());
        agent
            .get(&url)
            .header("x-onebrain-token", &self.info.token)
            .call()
            .is_ok()
    }

    /// `GET /api/internal/status` → parsed JSON value. The daemon returns the
    /// `search status` shape (`doc_count`, `pending_*`, `last_indexed`,
    /// `indexed`). Retries ONCE via [`ensure_running`] if the daemon vanished
    /// between discovery and the call (see [`with_retry`]).
    pub fn status(&self) -> Result<serde_json::Value> {
        with_retry(self, |h| {
            let url = format!("{}/api/internal/status", h.info.base_url());
            h.agent
                .get(&url)
                .header("x-onebrain-token", &h.info.token)
                .call()
        })
    }

    /// `POST /api/internal/reindex` with `{ mode, paths? }`. `paths` is required
    /// for `mode = "paths"`. Returns the daemon's reindex-result JSON. Retries
    /// ONCE via [`ensure_running`] on a transport failure.
    pub fn reindex(&self, mode: &str, paths: &[String]) -> Result<serde_json::Value> {
        let body = serde_json::json!({ "mode": mode, "paths": paths });
        let payload = serde_json::to_string(&body).context("serialize reindex body")?;
        with_retry(self, |h| {
            let url = format!("{}/api/internal/reindex", h.info.base_url());
            h.agent
                .post(&url)
                .header("x-onebrain-token", &h.info.token)
                .header("content-type", "application/json")
                .send(payload.as_str())
        })
    }

    /// `GET /api/internal/get?path=` → the doc's full indexed text, or
    /// `Ok(None)` when the daemon answers 404 (doc not indexed) so the CLI can
    /// render the same "not indexed yet" hint the direct path does.
    ///
    /// Uses [`retry_once`] with a status-preserving reconnect (a vanished daemon
    /// respawns + retries; any HTTP status is a real answer) but keeps the
    /// structured `ureq::Error` on the failure arm so a 404 can be told apart
    /// from a genuine error — [`with_retry`] can't, since it stringifies the
    /// status into an opaque anyhow before returning.
    pub fn get(&self, doc_path: &str) -> Result<Option<String>> {
        let p = urlencode(doc_path);
        let op = |h: &DaemonHandle| {
            let url = format!("{}/api/internal/get?path={}", h.info.base_url(), p);
            h.agent
                .get(&url)
                .header("x-onebrain-token", &h.info.token)
                .call()
        };
        let outcome = match op(self) {
            Ok(resp) => Ok(resp),
            Err(e) if is_transport_error(&e) => {
                tracing::warn!(error = %e, "daemon unreachable (get); reconnecting and retrying once");
                // Reconnect to the SAME vault this handle was serving (mirrors
                // `with_retry`), so a respawn after the daemon vanished can't be
                // satisfied by a wrong-vault daemon that started meanwhile.
                let expected = self.info.vault.clone();
                let fresh = ensure_running(expected.as_deref().map(Path::new))
                    .context("respawn daemon after transport failure (get)")?;
                op(&fresh)
            }
            Err(e) => Err(e),
        };
        match outcome {
            Ok(mut resp) => {
                let v = read_json(&mut resp)?;
                Ok(v.get("content").and_then(|c| c.as_str()).map(String::from))
            }
            // A 404 is the daemon's "doc not indexed" — surface it as `None` so
            // the caller renders its hint rather than an error.
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            // A 503 is the daemon holding no engine (another process owns the
            // redb lock) — classify as EngineBusy so the caller reports honest
            // E_ENGINE_BUSY, not an opaque E_INTERNAL. See `classify_daemon_ureq`.
            Err(ureq::Error::StatusCode(503)) => Err(daemon_engine_busy()),
            Err(e) => Err(anyhow::anyhow!("daemon get: {e}")),
        }
    }

    /// `GET /api/vault/search?q=&mode=[&top_k=][&min_candidates=]` → the
    /// webui search response JSON. `top_k`/`min_candidates` are optional
    /// per-query overrides (e.g. the CLI's `--top-k`/`--min-candidates`
    /// flags) — omitted entirely when `None`, so the server falls back to
    /// its own config-derived defaults exactly as before this param was
    /// added. Retries ONCE via [`ensure_running`] on a transport failure.
    pub fn search(
        &self,
        query: &str,
        mode: &str,
        top_k: Option<usize>,
        min_candidates: Option<usize>,
    ) -> Result<serde_json::Value> {
        let q = urlencode(query);
        let m = urlencode(mode);
        with_retry(self, |h| {
            let mut url = format!("{}/api/vault/search?q={}&mode={}", h.info.base_url(), q, m);
            if let Some(top_k) = top_k {
                url.push_str(&format!("&top_k={top_k}"));
            }
            if let Some(min_candidates) = min_candidates {
                url.push_str(&format!("&min_candidates={min_candidates}"));
            }
            h.agent
                .get(&url)
                .header("x-onebrain-token", &h.info.token)
                .call()
        })
    }

    /// `POST /api/internal/rerank` with `{ query, paths, top_k }` — doc-level
    /// Tier-2 rerank of a caller-supplied candidate path set (e.g. the MCP
    /// `query` tool's fused lex+vec result paths) through the daemon's held
    /// engine. Returns the daemon's `{ hits: [..] }` JSON, in the SAME wire
    /// shape `search()`'s response carries (`path`, `score`, `title`,
    /// `snippet`, `rerank_score`?), so callers reuse the same hit parser.
    /// Retries ONCE via [`ensure_running`] on a transport failure, same as
    /// every other call on this handle.
    pub fn rerank(&self, query: &str, paths: &[String], top_k: usize) -> Result<serde_json::Value> {
        let body = serde_json::json!({ "query": query, "paths": paths, "top_k": top_k });
        let payload = serde_json::to_string(&body).context("serialize rerank body")?;
        with_retry(self, |h| {
            let url = format!("{}/api/internal/rerank", h.info.base_url());
            h.agent
                .post(&url)
                .header("x-onebrain-token", &h.info.token)
                .header("content-type", "application/json")
                .send(payload.as_str())
        })
    }

    /// `POST /api/token/ledger/check` (Track 3, design §3b/§5b) with
    /// `{ path, session_token, record }`. The session token is resolved HERE
    /// (best-effort via [`resolve_session_token_opt`]) and attached so one
    /// daemon serves many sessions correctly; an unresolvable token is sent as
    /// `""`, which the daemon reads as "ledger inactive for this call".
    ///
    /// **Version-skew feature-detect (design §8):** a daemon that predates the
    /// token routes answers 404. That is NOT an error — it means "this daemon
    /// can't optimize", so the client gets `Ok(None)` and skips the
    /// optimization (inlines as today). Only a live route answers `Ok(Some)`.
    pub fn token_ledger_check(
        &self,
        path: &str,
        record: bool,
    ) -> Result<Option<serde_json::Value>> {
        let session_token = resolve_session_token_opt().unwrap_or_default();
        let body = serde_json::json!({
            "path": path,
            "session_token": session_token,
            "record": record,
        });
        let payload = serde_json::to_string(&body).context("serialize ledger check body")?;
        let op = |h: &DaemonHandle| {
            let url = format!("{}/api/token/ledger/check", h.info.base_url());
            h.agent
                .post(&url)
                .header("x-onebrain-token", &h.info.token)
                .header("content-type", "application/json")
                .send(payload.as_str())
        };
        match op(self) {
            Ok(mut resp) => Ok(Some(read_json(&mut resp)?)),
            // Route absent on an older daemon → skip optimization (not an error).
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(ureq::Error::StatusCode(503)) => Err(daemon_engine_busy()),
            Err(e) => Err(anyhow::anyhow!("daemon ledger check: {e}")),
        }
    }

    /// `GET /api/token/status` → `{ level, ledger_active, cache_bytes }`, or
    /// `Ok(None)` when the daemon predates the token routes (404). Same
    /// feature-detect contract as [`Self::token_ledger_check`].
    pub fn token_status(&self) -> Result<Option<serde_json::Value>> {
        self.token_get_feature_detected("/api/token/status")
    }

    /// `GET /api/token/gain?by=&since=&history=` → the gain pivot JSON, or
    /// `Ok(None)` on a 404 (route absent). Query params are omitted when
    /// `None`, mirroring [`Self::search`].
    pub fn token_gain(
        &self,
        by: Option<&str>,
        since: Option<&str>,
        history: bool,
    ) -> Result<Option<serde_json::Value>> {
        let mut path = String::from("/api/token/gain?");
        if let Some(by) = by {
            path.push_str(&format!("by={}&", urlencode(by)));
        }
        if let Some(since) = since {
            path.push_str(&format!("since={}&", urlencode(since)));
        }
        path.push_str(&format!("history={history}"));
        self.token_get_feature_detected(&path)
    }

    /// Shared GET helper for the feature-detected token routes: `Ok(Some(json))`
    /// on 2xx, `Ok(None)` on 404 (old daemon), `Err` on any other failure.
    fn token_get_feature_detected(&self, path: &str) -> Result<Option<serde_json::Value>> {
        let op = |h: &DaemonHandle| {
            let url = format!("{}{}", h.info.base_url(), path);
            h.agent
                .get(&url)
                .header("x-onebrain-token", &h.info.token)
                .call()
        };
        match op(self) {
            Ok(mut resp) => Ok(Some(read_json(&mut resp)?)),
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(ureq::Error::StatusCode(503)) => Err(daemon_engine_busy()),
            Err(e) => Err(anyhow::anyhow!("daemon token GET {path}: {e}")),
        }
    }

    /// `GET /api/health` → parsed JSON (`{ ok, engine_held, dist_dir }`), or
    /// `None` on ANY failure. **No retry, no respawn** — the `daemon status`
    /// dashboard is a read-only report and must never start, stop, or restart
    /// a daemon (unlike [`with_retry`]'s `ensure_running` reconnect).
    pub(crate) fn probe_health(&self) -> Option<serde_json::Value> {
        self.probe_get("/api/health")
    }

    /// `GET /api/internal/status` → parsed JSON, or `None` on ANY failure —
    /// including the engine-less daemon's 503, which the dashboard renders as
    /// absent Engine/Models fields. Same no-retry/no-respawn contract as
    /// [`Self::probe_health`]. Named apart from [`Self::status`] (the warm-
    /// client call that DOES reconnect-respawn) so a caller can't reach for
    /// the wrong lifecycle behaviour by accident.
    pub(crate) fn probe_status_no_retry(&self) -> Option<serde_json::Value> {
        self.probe_get("/api/internal/status")
    }

    /// One best-effort GET with the short [`STATUS_PROBE_TIMEOUT`], parsed as
    /// JSON. Any transport error, HTTP status error, or parse failure → `None`.
    /// The URL carries no token (it rides the header), so nothing sensitive
    /// can leak into error text or logs.
    fn probe_get(&self, path: &str) -> Option<serde_json::Value> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(STATUS_PROBE_TIMEOUT))
            .build()
            .into();
        let url = format!("{}{}", self.info.base_url(), path);
        let mut resp = agent
            .get(&url)
            .header("x-onebrain-token", &self.info.token)
            .call()
            .ok()?;
        read_json(&mut resp).ok()
    }
}

/// True when a ureq error means the daemon is UNREACHABLE (transport-level) —
/// as opposed to reachable-but-returned-an-error-status. Only a transport
/// failure justifies a respawn-and-retry; an HTTP status error is a real answer
/// from a live daemon and must propagate unchanged.
fn is_transport_error(e: &ureq::Error) -> bool {
    matches!(
        e,
        ureq::Error::ConnectionFailed
            | ureq::Error::HostNotFound
            | ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
    )
}

/// Run a daemon request `op` against `handle`; if it fails with a TRANSPORT
/// error (daemon gone between discovery and the call), respawn via
/// [`ensure_running`] and retry the op ONCE against the fresh handle. Any other
/// error (HTTP status, body-read/parse) propagates immediately — the warm-client
/// contract is "reconnect to a vanished daemon", not "retry a real error".
///
/// The retry POLICY is factored into the generic, network-free [`retry_once`]
/// so it is unit-testable without a live daemon; this wrapper just supplies the
/// real `is_transport_error` predicate + `ensure_running` reconnect and parses
/// the JSON body.
fn with_retry(
    handle: &DaemonHandle,
    op: impl Fn(&DaemonHandle) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<serde_json::Value> {
    // Reconnect to the SAME vault this handle was serving — pass its bound vault
    // as the expected vault so a respawn (after the daemon vanished) can't be
    // satisfied by a wrong-vault daemon that started meanwhile.
    let expected = handle.info.vault.clone();
    let mut resp = retry_once(
        handle,
        &op,
        is_transport_error,
        || {
            ensure_running(expected.as_deref().map(Path::new))
                .context("respawn daemon after transport failure")
        },
        classify_daemon_ureq,
    )?;
    read_json(&mut resp)
}

/// Turn a non-transport ureq error from a daemon request into an `anyhow` error.
///
/// A vault-bound daemon returns **503** when it holds no engine — another
/// process owns redb's single-writer lock (e.g. an `onebrain mcp` server from
/// before an upgrade, still alive while the new warm daemon runs). That is the
/// SAME contention the direct path reports as `E_ENGINE_BUSY`, so put the typed
/// [`onebrain_search::error::EngineBusy`] in the error chain; the search-verb
/// callers (`map_daemon_error`) and the status probe then map it identically to
/// the direct `Engine::open` lock case. Any other status keeps an opaque wrap.
///
/// **Why every routed 503 means engine-busy (not "no vault bound").** The daemon
/// also 503s from `require_vault_root` when it is running vault-less. But this
/// client is only reached via [`route_to_daemon`](super::search_common::route_to_daemon)
/// → [`discover_matching`], which returns a handle ONLY for a daemon whose
/// `daemon.json.vault` matches the caller's vault; a vault-less daemon fails
/// `vault_decision` (`Restart`) and is never routed to (the CLI opens the engine
/// directly instead). So a 503 on a routed request can only come from the
/// engine-contention paths — `require_engine` on `/api/internal/*` or
/// `map_search_failure` on `/api/vault/search` — never the no-vault guard.
fn classify_daemon_ureq(e: ureq::Error) -> anyhow::Error {
    if matches!(e, ureq::Error::StatusCode(503)) {
        return daemon_engine_busy();
    }
    anyhow::anyhow!("daemon request: {e}")
}

/// The typed engine-busy error for a daemon that holds no engine (503). Mirrors
/// the direct path's `onebrain_search` classification so `is_engine_busy` on the
/// chain is `true` for both the direct lock and the routed-503 case.
fn daemon_engine_busy() -> anyhow::Error {
    anyhow::Error::new(onebrain_search::error::EngineBusy)
        .context("daemon holds no engine — the search index is locked by another process")
}

/// Generic "try, and on a retryable error reconnect + try ONCE more" core.
///
/// Pure of any network/daemon knowledge — it takes the operation, a predicate
/// deciding whether an error is retryable, and a reconnect action producing a
/// fresh handle. Returns the first success, the reconnect error if reconnect
/// fails, or the SECOND attempt's result. A non-retryable error propagates
/// immediately (no reconnect). This is the seam the unit tests drive with fake
/// ops + a fake reconnect, so the branch logic is covered without a daemon.
fn retry_once<H, T, E>(
    handle: &H,
    op: &impl Fn(&H) -> std::result::Result<T, E>,
    is_retryable: impl Fn(&E) -> bool,
    reconnect: impl FnOnce() -> Result<H>,
    classify: impl Fn(E) -> anyhow::Error,
) -> Result<T>
where
    E: std::fmt::Display,
{
    match op(handle) {
        Ok(v) => Ok(v),
        Err(e) if is_retryable(&e) => {
            tracing::warn!(error = %e, "daemon unreachable; reconnecting and retrying once");
            let fresh = reconnect()?;
            op(&fresh).map_err(classify)
        }
        // A non-retryable error is a real answer from a live daemon; `classify`
        // turns a 503 (daemon holds no engine) into the typed EngineBusy, leaving
        // any other status as an opaque wrap.
        Err(e) => Err(classify(e)),
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

/// Discover an already-running daemon serving `expected_vault`, or `None`.
///
/// Reads `daemon.json`; if present and live AND at our version AND bound to the
/// caller's `expected_vault`, returns a handle. A stale record (parse error,
/// dead daemon) is cleaned up and yields `None`. A version OR vault mismatch
/// stops the old daemon, cleans the record, and yields `None` so a caller's
/// [`ensure_running`] starts a fresh one (see [`version_decision`] /
/// [`vault_decision`]).
///
/// `expected_vault` is the caller's resolved vault path (`None` = "any vault,
/// don't check" — e.g. a diagnostic that just wants to reach whatever daemon is
/// up). It is canonicalized here and compared against the daemon's stored
/// canonical vault.
pub fn discover(expected_vault: Option<&Path>) -> Result<Option<DaemonHandle>> {
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

    // A version OR vault mismatch both mean "this running daemon can't serve
    // us": restart it. Compute the vault decision from the caller's expectation
    // (which preserves the "has a vault but it wouldn't canonicalize" case as a
    // conservative mismatch) vs the daemon's stored canonical vault.
    let expected = vault_expectation(expected_vault);
    let version_mismatch =
        version_decision(&info.version, own_version()) == VersionDecision::Restart;
    let vault_mismatch = vault_decision(info.vault.as_deref(), &expected) == VaultDecision::Restart;
    if version_mismatch || vault_mismatch {
        if vault_mismatch && !version_mismatch {
            tracing::info!(
                daemon_vault = ?info.vault,
                expected_vault = ?expected,
                "daemon bound to a different vault; restarting daemon for the caller's vault"
            );
        } else {
            tracing::info!(
                daemon_version = %info.version,
                own_version = own_version(),
                "daemon version skew; restarting daemon"
            );
        }
        // Stop the mismatched daemon, then clear its record. A stop FAILURE is
        // material — the old daemon may still hold the redb lock, so the
        // subsequent start would fail. Capture it: warn now, and (via the
        // caller's `ensure_running`) it surfaces as an "engine still locked"
        // start failure rather than being silently dropped.
        if let Err(e) = stop_daemon() {
            tracing::warn!(error = %e, "failed to stop mismatched daemon; \
                a fresh start may fail while it still holds the engine lock");
        }
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

/// PASSIVE discovery for the CLI search verbs — return a handle ONLY when a
/// daemon is live, at our version, AND bound to `expected_vault`; otherwise
/// `Ok(None)`. **Never stops, restarts, or spawns a daemon.**
///
/// This is the deliberate counterpart to [`discover`]: `discover` is the MCP
/// path's ACTIVE lifecycle owner and, on a version- OR vault-mismatch, STOPS +
/// restarts the daemon for the caller's vault (one daemon per machine; the MCP
/// session owns switching it). The CLI, by contrast, is a PASSIVE reader —
/// `onebrain search …` must NEVER kill a daemon that is serving another vault
/// (a live MCP session), so on ANY mismatch it simply declines to route
/// (`Ok(None)`) and the caller opens the engine directly.
///
/// Decisions (all from `daemon.json`, no HTTP until the vault/version check
/// passes):
/// - version mismatch (`version_decision == Restart`) → `Ok(None)`, record
///   LEFT INTACT (a foreign daemon; not ours to reclaim).
/// - vault mismatch (`vault_decision == Restart` — wrong vault, vault-less
///   daemon while we expect one, or a pre-vault-field record) → `Ok(None)`,
///   record LEFT INTACT.
/// - match, but the daemon fails the `/api/health` liveness probe (via
///   [`DaemonHandle::is_live`] — the engine-INDEPENDENT route, so a
///   startup-window engine-less daemon still reads live) → a genuinely-dead
///   SAME-vault record is stale, so it is reclaimed (`remove`) and `Ok(None)`
///   returned.
/// - match + live → `Ok(Some(handle))`.
///
/// `expected_vault = None` means "any vault" and skips the vault check (kept for
/// signature symmetry; the CLI always passes `Some`).
pub fn discover_matching(expected_vault: Option<&Path>) -> Result<Option<DaemonHandle>> {
    let path = discovery_path()?;
    let info = match DaemonInfo::read(&path) {
        Ok(Some(info)) => info,
        Ok(None) => return Ok(None),
        // A corrupt record isn't tied to any specific vault, so cleaning it is
        // safe and matches `discover`'s handling.
        Err(_) => {
            let _ = DaemonInfo::remove(&path);
            return Ok(None);
        }
    };

    // Version or vault mismatch → decline to route, WITHOUT touching the record.
    // A mismatched daemon may be a live MCP session on another vault; the CLI
    // must never stop/restart/remove it. An expected vault that won't
    // canonicalize is treated as a mismatch here too (via `vault_expectation`):
    // the CLI can't verify the daemon serves the right vault, so it routes
    // direct rather than adopt a possibly-wrong-vault daemon.
    let expected = vault_expectation(expected_vault);
    if version_decision(&info.version, own_version()) == VersionDecision::Restart {
        tracing::debug!(
            daemon_version = %info.version, own_version = own_version(),
            "daemon at a different version; CLI routes direct (no restart)"
        );
        return Ok(None);
    }
    if vault_decision(info.vault.as_deref(), &expected) == VaultDecision::Restart {
        tracing::debug!(
            daemon_vault = ?info.vault, expected_vault = ?expected,
            "daemon bound to a different vault; CLI routes direct (no restart)"
        );
        return Ok(None);
    }

    // Same vault + our version. Probe liveness (engine-independent /api/health).
    let handle = DaemonHandle::new(info);
    if handle.is_live() {
        Ok(Some(handle))
    } else {
        // A dead SAME-vault record is genuinely stale (it's ours to reclaim);
        // clean it so the next lookup doesn't re-probe a corpse.
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
pub fn ensure_running(expected_vault: Option<&Path>) -> Result<DaemonHandle> {
    if let Some(handle) = discover(expected_vault)? {
        return Ok(handle);
    }

    // If the caller HAS a vault but it won't canonicalize (dir vanished
    // mid-op), refuse rather than spawn a daemon we couldn't verify — a
    // `--vault <vanished-path>` daemon would bind vault-less and we'd never
    // adopt it anyway (the poll below requires a vault MATCH). Bail early with a
    // clear message instead of spinning to the start timeout.
    let expected = vault_expectation(expected_vault);
    if expected == VaultExpectation::Unresolvable {
        anyhow::bail!(
            "cannot start a daemon: the expected vault could not be resolved \
             (canonicalize failed — did the directory move or get removed?)"
        );
    }

    // Pass the resolved vault through `daemon start --vault` so the spawned
    // daemon binds it EXPLICITLY (no `$ONEBRAIN_VAULT` mutation).
    spawn_daemon_start(expected_vault).context("spawn daemon start")?;

    let path = discovery_path()?;
    let deadline = Instant::now() + START_TIMEOUT;
    // Remember the last-observed discovery record so a timeout can say WHY.
    let mut last_read: Option<DaemonInfo> = None;
    loop {
        // A daemon.json at our version + our vault + live probe → connected.
        // The vault check matters even here: a concurrent starter for a
        // DIFFERENT vault could win the race and publish first, and connecting
        // to it would silently serve the wrong vault. `spawn_daemon_start` now
        // passes `--vault`, so the daemon WE spawn binds our vault; requiring
        // the match just refuses to adopt a wrong-vault winner (we keep polling
        // until the deadline instead).
        if let Ok(read) = DaemonInfo::read(&path) {
            last_read = read.clone();
            if let Some(info) = read {
                let version_ok =
                    version_decision(&info.version, own_version()) == VersionDecision::Use;
                let vault_ok =
                    vault_decision(info.vault.as_deref(), &expected) == VaultDecision::Use;
                if version_ok && vault_ok {
                    let handle = DaemonHandle::new(info);
                    if handle.is_live() {
                        return Ok(handle);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            // Classify the last state + point at the log, instead of one opaque
            // "did not become ready" line.
            let state = classify_last_state(&last_read, own_version());
            let log = log_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "~/.onebrain/run/daemon.log".to_string());
            anyhow::bail!(
                "daemon did not become ready within {}s; last state: {}. See {log}",
                START_TIMEOUT.as_secs(),
                state.describe(),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Spawn `onebrain daemon start` (which self-respawns the detached `__run`
/// child + writes the PID file). Runs the CURRENT executable so a client always
/// starts a daemon at its own version. Inherits no stdio (the child redirects
/// its own to the daemon log).
///
/// `expected_vault` is passed through as `daemon start --vault <path>` so the
/// spawned daemon binds the SAME vault the caller resolved, WITHOUT the caller
/// having to export `$ONEBRAIN_VAULT` (a process-env mutation that is unsound
/// under concurrent reads and deprecated since Rust 1.81). When `None`, no
/// `--vault` is passed and the daemon falls back to `$ONEBRAIN_VAULT`.
fn spawn_daemon_start(expected_vault: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "start"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(v) = expected_vault {
        cmd.arg("--vault").arg(v);
    }
    let status = cmd.status().context("run `onebrain daemon start`")?;
    if !status.success() {
        anyhow::bail!("`onebrain daemon start` exited with {status}");
    }
    Ok(())
}

/// Stop a running daemon via `onebrain daemon stop` (SIGTERM + PID cleanup).
/// Used by the version-skew restart path. Returns an error on a non-zero exit
/// so the caller can react (the old daemon may still hold the engine lock).
fn stop_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let status = std::process::Command::new(exe)
        .args(["daemon", "stop"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("run `onebrain daemon stop`")?;
    if !status.success() {
        anyhow::bail!("`onebrain daemon stop` exited with {status}");
    }
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
            vault: None,
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

    // ── vault_decision: the wrong-vault-daemon restart policy (C1) ─────────
    #[test]
    fn vault_decision_no_expectation_always_uses() {
        // A caller with no vault expectation never forces a restart.
        assert_eq!(
            vault_decision(Some("/vaults/a"), &VaultExpectation::Any),
            VaultDecision::Use
        );
        assert_eq!(
            vault_decision(None, &VaultExpectation::Any),
            VaultDecision::Use
        );
    }

    #[test]
    fn vault_decision_same_vault_uses() {
        assert_eq!(
            vault_decision(
                Some("/vaults/a"),
                &VaultExpectation::Vault("/vaults/a".into())
            ),
            VaultDecision::Use
        );
    }

    #[test]
    fn vault_decision_different_vault_restarts() {
        // Daemon on A, caller wants B → restart (the core wrong-vault guard).
        assert_eq!(
            vault_decision(
                Some("/vaults/a"),
                &VaultExpectation::Vault("/vaults/b".into())
            ),
            VaultDecision::Restart
        );
    }

    #[test]
    fn vault_decision_vaultless_or_old_record_restarts_when_vault_expected() {
        // Daemon bound vault-less, OR an old daemon.json with no vault field
        // (deserializes to None) → restart when a caller expects a vault. An
        // old daemon is never trusted to be serving the right vault.
        assert_eq!(
            vault_decision(None, &VaultExpectation::Vault("/vaults/b".into())),
            VaultDecision::Restart
        );
    }

    // ── vault_decision + vault_expectation: the uncanonicalizable-expected-vault
    // hardening (the narrow wrong-vault TOCTOU). A caller that HAS a vault which
    // won't canonicalize must be a MISMATCH, not silently "no expectation".
    #[test]
    fn vault_decision_unresolvable_expected_restarts_even_against_matching_daemon() {
        // Even if a daemon is up on `/vaults/a`, an Unresolvable expectation
        // can't confirm it — treat as a mismatch (don't adopt it).
        assert_eq!(
            vault_decision(Some("/vaults/a"), &VaultExpectation::Unresolvable),
            VaultDecision::Restart
        );
        assert_eq!(
            vault_decision(None, &VaultExpectation::Unresolvable),
            VaultDecision::Restart
        );
    }

    #[test]
    fn vault_expectation_none_is_any() {
        assert_eq!(vault_expectation(None), VaultExpectation::Any);
    }

    #[test]
    fn vault_expectation_real_dir_canonicalizes_to_vault() {
        // An existing directory canonicalizes → `Vault(id)` matching the daemon
        // record a co-located daemon would write for the same path.
        let dir = tempdir().unwrap();
        match vault_expectation(Some(dir.path())) {
            VaultExpectation::Vault(id) => {
                assert_eq!(Some(id), canonical_vault_id(dir.path()));
            }
            other => panic!("expected Vault(_), got {other:?}"),
        }
    }

    #[test]
    fn vault_expectation_missing_dir_is_unresolvable() {
        // A path that can't be canonicalized (never existed / vanished mid-op)
        // → `Unresolvable`, NOT `Any` — this is the TOCTOU the hardening closes.
        let dir = tempdir().unwrap();
        let gone = dir.path().join("does-not-exist");
        assert_eq!(
            vault_expectation(Some(&gone)),
            VaultExpectation::Unresolvable
        );
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
        assert!(discover(None).unwrap().is_none());
    }

    #[test]
    fn base_url_targets_localhost_and_port() {
        assert_eq!(sample("3.4.6").base_url(), "http://127.0.0.1:6789");
    }

    // ── is_transport_error: only unreachable errors are retryable ──────────
    #[test]
    fn transport_errors_are_retryable_status_errors_are_not() {
        assert!(is_transport_error(&ureq::Error::ConnectionFailed));
        assert!(is_transport_error(&ureq::Error::HostNotFound));
        assert!(is_transport_error(&ureq::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ))));
        // An HTTP status is a real answer from a live daemon — NOT retryable.
        assert!(!is_transport_error(&ureq::Error::StatusCode(503)));
        assert!(!is_transport_error(&ureq::Error::StatusCode(400)));
    }

    // ── retry_once: the reconnect-and-retry policy, network-free ───────────
    #[test]
    fn retry_once_returns_first_success_without_reconnect() {
        let reconnected = std::cell::Cell::new(false);
        let out: Result<u32> = retry_once(
            &(),
            &|_| Ok::<u32, String>(7),
            |_| true,
            || {
                reconnected.set(true);
                Ok(())
            },
            |e| anyhow::anyhow!("{e}"),
        );
        assert_eq!(out.unwrap(), 7);
        assert!(!reconnected.get(), "no reconnect on first-try success");
    }

    #[test]
    fn retry_once_reconnects_and_retries_on_retryable_error() {
        // First op call fails retryably; after reconnect the second call wins.
        let calls = std::cell::Cell::new(0u32);
        let reconnected = std::cell::Cell::new(false);
        let out: Result<u32> = retry_once(
            &(),
            &|_| {
                let n = calls.get();
                calls.set(n + 1);
                if n == 0 {
                    Err("gone".to_string())
                } else {
                    Ok(42)
                }
            },
            |_| true, // retryable
            || {
                reconnected.set(true);
                Ok(())
            },
            |e| anyhow::anyhow!("{e}"),
        );
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.get(), 2, "op runs exactly twice (retry ONCE)");
        assert!(reconnected.get(), "reconnect happened");
    }

    #[test]
    fn retry_once_does_not_reconnect_on_nonretryable_error() {
        let reconnected = std::cell::Cell::new(false);
        let out: Result<u32> = retry_once(
            &(),
            &|_| Err::<u32, String>("HTTP 400".to_string()),
            |_| false, // NOT retryable (e.g. a status error)
            || {
                reconnected.set(true);
                Ok(())
            },
            |e| anyhow::anyhow!("{e}"),
        );
        assert!(out.is_err());
        assert!(
            !reconnected.get(),
            "a non-retryable error must not reconnect"
        );
    }

    #[test]
    fn retry_once_propagates_reconnect_failure() {
        let out: Result<u32> = retry_once(
            &(),
            &|_| Err::<u32, String>("gone".to_string()),
            |_| true,
            || anyhow::bail!("reconnect failed"),
            |e| anyhow::anyhow!("{e}"),
        );
        let msg = format!("{:#}", out.unwrap_err());
        assert!(msg.contains("reconnect failed"), "got: {msg}");
    }

    // ── classify_daemon_ureq: 503 → typed EngineBusy, else opaque ──────────
    #[test]
    fn classify_daemon_ureq_maps_503_to_engine_busy() {
        let err = classify_daemon_ureq(ureq::Error::StatusCode(503));
        assert!(
            onebrain_search::error::is_engine_busy(&err),
            "a daemon 503 (engine-less) must classify as EngineBusy, got: {err:#}"
        );
    }

    #[test]
    fn classify_daemon_ureq_leaves_other_status_opaque() {
        let err = classify_daemon_ureq(ureq::Error::StatusCode(500));
        assert!(
            !onebrain_search::error::is_engine_busy(&err),
            "a non-503 status must NOT be EngineBusy, got: {err:#}"
        );
    }

    // ── classify_last_state + LastState::describe (timeout diagnostics) ────
    #[test]
    fn classify_last_state_maps_each_case() {
        assert_eq!(classify_last_state(&None, "3.4.6"), LastState::NotFound);

        let skew = classify_last_state(&Some(sample("3.4.5")), "3.4.6");
        assert_eq!(
            skew,
            LastState::VersionSkew {
                found: "3.4.5".to_string(),
                own: "3.4.6".to_string(),
            }
        );

        let alive = classify_last_state(&Some(sample("3.4.6")), "3.4.6");
        assert_eq!(alive, LastState::AliveNoResponse);
    }

    #[test]
    fn last_state_describe_is_actionable() {
        assert!(LastState::NotFound.describe().contains("no daemon.json"));
        assert!(LastState::AliveNoResponse
            .describe()
            .contains("didn't answer"));
        let msg = LastState::VersionSkew {
            found: "3.4.5".to_string(),
            own: "3.4.6".to_string(),
        }
        .describe();
        assert!(msg.contains("3.4.5") && msg.contains("3.4.6"), "got: {msg}");
    }

    // ── wire-contract: a real internal.rs status body parses cleanly ───────
    // Feeds the EXACT JSON shape `GET /api/internal/status` emits through the
    // same `serde_json` parse `read_json` uses, so the client + server agree on
    // the wire format without a live socket.
    #[test]
    fn status_wire_shape_parses() {
        let body = r#"{"doc_count":3,"pending_new":1,"pending_changed":0,
            "pending_removed":2,"pending_total":3,"last_indexed":42,"indexed":true}"#;
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["doc_count"], 3);
        assert_eq!(v["pending_total"], 3);
        assert_eq!(v["indexed"], true);
    }

    // Reindex non-2xx mapping: a ureq StatusCode error is classified
    // non-retryable, so `with_retry` would propagate it (see the retry_once
    // non-retryable test). This locks that a 500-class reindex response is NOT
    // treated as "daemon gone".
    #[test]
    fn reindex_error_status_is_not_transport_error() {
        assert!(!is_transport_error(&ureq::Error::StatusCode(500)));
    }

    // ── Live-server tests: exercise the HTTP client methods (is_live, status,
    // search, discover) against a REAL ephemeral server holding an engine over a
    // lex-seeded vault. Download-free (lex / empty engine). This covers the
    // ureq client paths without spawning a full daemon process. ───────────────

    /// Stand up a real server on an ephemeral port, holding an engine over a
    /// lex-seeded vault, and return a LiveServer with a matching DaemonInfo.
    ///
    /// The CALLER must already hold the `ONEBRAIN_CACHE_DIR` (+ `HOME`) env
    /// under `test_env`'s lock — the server thread must NOT call
    /// `test_env::set_var` (the lock is non-reentrant → it would deadlock
    /// against the caller's guard). The server thread only reads the process env
    /// the caller set. Lex is seeded here (under the held env) so the held
    /// engine has data; the seed path is derived from `collection_cache_dir`, so
    /// it honours the caller's `ONEBRAIN_CACHE_DIR`.
    fn start_live_server(vault: &Path, token: &str) -> LiveServerHandle {
        use onebrain_search::chunk::Chunk;
        use onebrain_search::lex::LexIndex;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        std::fs::write(
            vault.join("onebrain.yml"),
            "search:\n  collection: dc-live\n",
        )
        .unwrap();
        {
            let cache_dir = crate::commands::search_common::collection_cache_dir("dc-live");
            let tantivy =
                crate::commands::search_common::index_artifact_path(&cache_dir, "tantivy");
            let mut lex = LexIndex::open(&tantivy).unwrap();
            lex.add(&Chunk {
                chunk_id: "alpha.md#0".to_string(),
                doc_path: "alpha.md".to_string(),
                heading_path: String::new(),
                text: "the quick brown fox".to_string(),
                chunk_index: 0,
            })
            .unwrap();
            lex.commit().unwrap();
        }

        let stop = Arc::new(AtomicBool::new(false));
        let port = Arc::new(std::sync::atomic::AtomicU16::new(0));
        let vault_path = vault.to_path_buf();
        let token_thread = token.to_string();
        let stop_thread = stop.clone();
        let port_thread = port.clone();

        let join = std::thread::spawn(move || {
            let mut cfg =
                crate::server::ServeConfig::localhost(Some(vault_path), 0, token_thread, None);
            cfg.hold_engine = true;
            let (router, _state) = crate::server::build_router_with_state(cfg);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
                let on_bind =
                    |a: std::net::SocketAddr| port_thread.store(a.port(), Ordering::SeqCst);
                let shutdown = async move {
                    while !stop_thread.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                };
                let _ =
                    crate::server::run_server_from_router(router, addr, on_bind, shutdown).await;
            });
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let bound = loop {
            let p = port.load(Ordering::SeqCst);
            if p != 0 {
                break p;
            }
            assert!(Instant::now() < deadline, "server never bound");
            std::thread::sleep(Duration::from_millis(10));
        };

        LiveServerHandle {
            info: DaemonInfo {
                port: bound,
                token: token.to_string(),
                pid: std::process::id(),
                version: own_version().to_string(),
                // Stamp the canonical vault so discover/ensure_running vault
                // checks match a caller resolving this same vault.
                vault: canonical_vault_id(vault),
            },
            stop,
            join: Some(join),
        }
    }

    /// The running server's info + shutdown handle (no temp dirs — the caller
    /// owns those + the env guard, keeping the non-reentrant lock uncontended).
    struct LiveServerHandle {
        info: DaemonInfo,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for LiveServerHandle {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    #[test]
    fn handle_is_live_and_status_search_reindex_round_trip() {
        // One test drives all four HTTP client methods against one live server,
        // holding the env lock for the whole test (so the server thread — which
        // must NOT take the lock — reads a stable ONEBRAIN_CACHE_DIR).
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let srv = start_live_server(vault.path(), "dc-live-token-1234567890");
        let handle = DaemonHandle::new(srv.info.clone());

        assert!(handle.is_live(), "health probe should see the live server");

        let status = handle.status().unwrap();
        assert!(status["doc_count"].is_number(), "status: {status}");

        let search = handle.search("quick", "lex", None, None).unwrap();
        let hits = search["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "search: {search}");
        assert_eq!(hits[0]["path"], "alpha.md");

        // pending reindex: no on-disk md → empty worklist → accepted, no embed.
        let reindex = handle.reindex("pending", &[]).unwrap();
        assert!(reindex["doc_count"].is_number(), "reindex: {reindex}");

        // get: nothing embedded (only lex was seeded), so the meta table has no
        // record → the daemon 404s → the client maps that to Ok(None) (Track 2c).
        let got = handle.get("alpha.md").unwrap();
        assert!(got.is_none(), "unindexed doc → Ok(None): {got:?}");

        // The no-retry status probes (the `daemon status` dashboard, #197):
        // health carries engine_held + a PRESENT-but-null dist_dir (embedded
        // UI), and the internal status carries the live index + model fields.
        let health = handle.probe_health().expect("health probe");
        assert_eq!(health["ok"], true, "{health}");
        assert!(health["engine_held"].is_boolean(), "{health}");
        assert!(health.get("dist_dir").is_some(), "{health}");
        assert!(health["dist_dir"].is_null(), "{health}");
        let internal = handle.probe_status_no_retry().expect("status probe");
        assert!(internal["doc_count"].is_number(), "{internal}");
        assert!(internal["embed_model"].is_string(), "{internal}");
        assert!(internal["reranker_model"].is_string(), "{internal}");

        // A dead server degrades both probes to None — never an error, never a
        // respawn (drop stops the server; the handle still points at the port).
        drop(srv);
        assert!(handle.probe_health().is_none());
        assert!(handle.probe_status_no_retry().is_none());
    }

    /// PASSIVE vault-match (Track 2c): `discover_matching` routes to a live
    /// daemon that serves the caller's vault, but returns `None` (→ direct open)
    /// for a DIFFERENT vault — WITHOUT stopping/removing the daemon.json record
    /// (the CLI never disturbs a daemon serving another vault). Live server so
    /// the same-vault arm exercises the real `/api/health` liveness probe.
    #[cfg(unix)]
    #[test]
    fn discover_matching_routes_same_vault_and_declines_other_untouched() {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("HOME", home.path().as_os_str()),
        ]);
        let srv = start_live_server(vault.path(), "dc-live-token-1234567890");
        let path = discovery_path().unwrap();
        srv.info.write(&path).unwrap();

        // Same vault → routes (Some, live).
        let matched = discover_matching(Some(vault.path())).unwrap();
        assert!(matched.is_some(), "same-vault daemon should route");

        // A DIFFERENT vault → declines (None, direct open) …
        let other = tempdir().unwrap();
        assert!(
            discover_matching(Some(other.path())).unwrap().is_none(),
            "wrong-vault daemon must NOT route"
        );
        // … and the daemon.json record is UNTOUCHED (no stop/remove — the CLI
        // must never disturb a daemon serving another vault).
        let still = DaemonInfo::read(&path).unwrap().expect("record preserved");
        assert_eq!(still.vault, srv.info.vault, "record must be left intact");
    }

    /// HEADLINE acceptance (Track 2c): with a daemon holding the engine, a
    /// client can write a note, reindex it THROUGH the daemon, and then `get` /
    /// `status` see it — i.e. in-session indexing works and CLI-during-session
    /// reads succeed without ever opening a second engine (no `E_ENGINE_BUSY`).
    ///
    /// Lex-only tier only: there, `mode:"paths"` records the doc's chunk
    /// metadata + hash WITHOUT embedding (`embed_passages_if_available` returns
    /// `None`), so `get` returns text and `doc_count` rises with NO model
    /// download. Under `semantic` the same path would embed → a multi-GB model
    /// download, which the no-download test rule forbids; that tier's daemon
    /// round-trip is covered by `handle_is_live_and_status_search_reindex_round_trip`
    /// (empty-`pending` reindex + `get`-miss, all embed-free) plus
    /// `discover_matching_routes_same_vault_and_declines_other_untouched`.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn in_session_write_reindex_get_round_trip() {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let srv = start_live_server(vault.path(), "dc-live-token-1234567890");
        let handle = DaemonHandle::new(srv.info.clone());

        // A real on-disk note the daemon can index via mode:"paths".
        std::fs::write(
            vault.path().join("note.md"),
            "in-session indexed body text\n",
        )
        .unwrap();

        // Before indexing: get is a miss (Ok(None)).
        assert!(
            handle.get("note.md").unwrap().is_none(),
            "unindexed note → Ok(None) before reindex"
        );

        // Reindex the note THROUGH the daemon (the sole engine owner).
        let reindex = handle.reindex("paths", &["note.md".to_string()]).unwrap();
        assert_eq!(reindex["added"], 1, "note newly indexed: {reindex}");
        assert_eq!(reindex["doc_count"], 1, "doc_count rose: {reindex}");

        // After: get returns the indexed text (CLI-during-session read works).
        let content = handle.get("note.md").unwrap().expect("note now indexed");
        assert!(
            content.contains("in-session indexed body text"),
            "get returns indexed text: {content}"
        );

        // And status reflects the in-session index (never busy — routed).
        let status = handle.status().unwrap();
        assert_eq!(status["doc_count"], 1, "status sees the doc: {status}");

        // Rerank the now-indexed doc THROUGH the daemon: one candidate path,
        // one hit back, mapped to the SAME wire shape `search()` uses. No
        // reranker model is downloaded in this fresh test cache, so
        // `rerank_score` is omitted — proving the route + held engine + field
        // mapping are wired end-to-end without a model download.
        let rerank = handle
            .rerank("indexed", &["note.md".to_string()], 5)
            .unwrap();
        let hits = rerank["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "rerank: {rerank}");
        assert_eq!(hits[0]["path"], "note.md", "rerank: {rerank}");
        assert_eq!(hits[0]["title"], "note", "rerank: {rerank}");
        assert!(
            hits[0].get("rerank_score").is_none(),
            "no model downloaded -> rerank_score omitted: {rerank}"
        );
    }

    #[cfg(not(feature = "semantic"))]
    #[test]
    fn rerank_503_when_no_engine_held() {
        // Like search/reindex, rerank 503s when the daemon holds no engine —
        // exercised at the client layer via a live server started WITHOUT
        // hold_engine, mirroring the internal.rs route-level 503 test.
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: dc-rerank-no-engine\n",
        )
        .unwrap();

        let token = "dc-rerank-no-engine-token-123";
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let port = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
        let vault_path = vault.path().to_path_buf();
        let token_thread = token.to_string();
        let stop_thread = stop.clone();
        let port_thread = port.clone();
        let join = std::thread::spawn(move || {
            // hold_engine defaults to false — this daemon holds no engine.
            let cfg =
                crate::server::ServeConfig::localhost(Some(vault_path), 0, token_thread, None);
            let (router, _state) = crate::server::build_router_with_state(cfg);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                port_thread.store(
                    listener.local_addr().unwrap().port(),
                    std::sync::atomic::Ordering::SeqCst,
                );
                let server = axum::serve(listener, router);
                let graceful = server.with_graceful_shutdown(async move {
                    while !stop_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                });
                let _ = graceful.await;
            });
        });
        // Wait for the port to be published, same pattern as start_live_server.
        let bound = loop {
            let p = port.load(std::sync::atomic::Ordering::SeqCst);
            if p != 0 {
                break p;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let info = DaemonInfo {
            port: bound,
            token: token.to_string(),
            pid: std::process::id(),
            version: own_version().to_string(),
            vault: canonical_vault_id(vault.path()),
        };
        let handle = DaemonHandle::new(info);

        let err = handle
            .rerank("hello", &["note.md".to_string()], 5)
            .unwrap_err();
        assert!(
            onebrain_search::error::is_engine_busy(&err),
            "no-engine-held rerank must classify as EngineBusy: {err}"
        );

        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = join.join();
    }

    #[cfg(unix)]
    #[test]
    fn discover_finds_a_live_server_via_daemon_json() {
        // HOME + ONEBRAIN_CACHE_DIR under ONE guard (set_vars is atomic; two
        // set_var calls would deadlock on the non-reentrant lock).
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("HOME", home.path().as_os_str()),
        ]);
        let srv = start_live_server(vault.path(), "dc-live-token-1234567890");

        let path = discovery_path().unwrap();
        srv.info.write(&path).unwrap();

        // Discover with the SAME vault the server bound → vault check passes.
        let found = discover(Some(vault.path())).unwrap();
        assert!(found.is_some(), "discover should find the live server");
        let v = found.unwrap().status().unwrap();
        assert!(v["doc_count"].is_number());
    }

    // C1: a daemon bound to vault A must NOT be adopted by a client resolving a
    // DIFFERENT vault B — `discover(Some(B))` refuses it (restarts instead), so
    // B's queries never silently route through A's engine.
    //
    // We assert the REFUSAL at the fast-path level with a live server bound to A
    // and `daemon.json` stamped with A. To avoid the `stop_daemon` subprocess
    // spawn on the mismatch path (which, under `cargo test`, would re-exec the
    // test binary — the same reason `ensure_running`'s spawn is a documented
    // coverage residual), we test the REUSE side directly: a same-vault client
    // (A) adopts it, and a cross-vault client's decision is `Restart`. The
    // actual respawn is the version-skew-equivalent OS shell (see
    // docs/coverage.md). The pure `vault_decision`/`version_decision` policy and
    // the wire round-trips below pin the logic without a spawn.
    #[cfg(unix)]
    #[test]
    fn discover_reuses_only_when_vault_matches() {
        let vault_a = tempdir().unwrap();
        let vault_b = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("HOME", home.path().as_os_str()),
        ]);
        // Live server bound to vault A, and a daemon.json stamped with A.
        let srv = start_live_server(vault_a.path(), "dc-live-token-1234567890");
        srv.info.write(&discovery_path().unwrap()).unwrap();
        let a_id = canonical_vault_id(vault_a.path());
        assert_eq!(srv.info.vault, a_id, "daemon.json must record vault A");

        // Same-vault client (A) → the daemon is REUSED (fast path, no restart).
        let found = discover(Some(vault_a.path())).unwrap();
        assert!(found.is_some(), "a same-vault client must reuse the daemon");
        assert_eq!(found.unwrap().info().vault, a_id);

        // Cross-vault client (B) → the DECISION is Restart (the refusal), which
        // is what triggers `discover` to stop + respawn for B. Asserting the
        // pure decision keeps this spawn-free; the live restart is OS shell.
        assert_eq!(
            vault_decision(
                srv.info.vault.as_deref(),
                &vault_expectation(Some(vault_b.path()))
            ),
            VaultDecision::Restart,
            "a daemon bound to A must be refused (restarted) for vault B"
        );
    }

    // Back-compat: a daemon.json written by a pre-vault-field CLI has no `vault`
    // key; it must deserialize with `vault: None` (serde default), so an old
    // daemon reads as "unknown vault" and a vault-expecting caller restarts it.
    #[test]
    fn old_daemon_json_without_vault_field_deserializes_to_none() {
        let body = r#"{"port":6789,"token":"tok-abcdefghijklmnop","pid":42,"version":"3.4.5"}"#;
        let info: DaemonInfo = serde_json::from_str(body).unwrap();
        assert_eq!(info.vault, None);
        assert_eq!(
            vault_decision(
                info.vault.as_deref(),
                &VaultExpectation::Vault("/vaults/b".into())
            ),
            VaultDecision::Restart
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_running_fast_path_returns_discovered_daemon() {
        // With a live server + a matching daemon.json, ensure_running must take
        // the fast path (discover succeeds) and NOT spawn anything.
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("HOME", home.path().as_os_str()),
        ]);
        let srv = start_live_server(vault.path(), "dc-live-token-1234567890");
        srv.info.write(&discovery_path().unwrap()).unwrap();

        let handle = ensure_running(Some(vault.path())).expect("fast path should connect");
        assert_eq!(handle.info().port, srv.info.port);
    }

    #[cfg(unix)]
    #[test]
    fn discover_removes_stale_record_for_dead_server() {
        // A daemon.json pointing at a port nothing listens on → discover cleans
        // it up and returns None (stale record, no live daemon).
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_var("HOME", home.path());
        let path = discovery_path().unwrap();
        DaemonInfo {
            port: 1, // unused — connection refused
            token: "x".repeat(20),
            pid: std::process::id(),
            version: own_version().to_string(),
            vault: None,
        }
        .write(&path)
        .unwrap();

        // Vault-agnostic (None) so this exercises ONLY the dead-server cleanup,
        // not the vault check — a dead daemon is cleaned up regardless of vault.
        assert!(discover(None).unwrap().is_none(), "dead server → None");
        assert!(
            DaemonInfo::read(&path).unwrap().is_none(),
            "stale daemon.json should be removed"
        );
    }
}
