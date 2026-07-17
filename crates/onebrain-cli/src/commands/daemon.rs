//! `onebrain daemon start|stop|status` (+ the hidden `daemon __run`).
//!
//! **Process model — self-respawn, detached.** `daemon start` does NOT fork.
//! It re-spawns the *current executable* running the hidden `daemon __run`
//! subcommand as a detached background process (its own session via
//! [`nix::unistd::setsid`], stdio redirected to a log file), records the
//! child's PID in a PID file, and returns. `daemon __run` is the long-lived
//! process: it initialises file logging, writes its own PID, then (v3.3 step 2)
//! spins up a tokio runtime and runs the shared HTTP surface ([`crate::server`])
//! until SIGTERM. The server + its graceful shutdown are shared verbatim with
//! the foreground `onebrain serve` command — only the shutdown trigger differs
//! (SIGTERM here, Ctrl-C there).
//!
//! ## Layout (mirrors `session_init.rs`)
//! - The status/PID logic is a **pure, dependency-injected core**
//!   ([`compute_status`]) so it is unit-testable without spawning a real
//!   process: the liveness probe (`is_alive: impl Fn(u32) -> bool`) and the
//!   PID-file path are both injected.
//! - The thin public wiring ([`run_start`] / [`run_stop`] / [`run_status`] /
//!   [`run_internal`]) resolves the real paths + probe and does the I/O.
//!
//! ## Warm-engine ownership (v3.4.6)
//! `daemon __run` opens the native-search [`Engine`](onebrain_search::engine::Engine)
//! ONCE at boot (`hold_engine`) and holds it for the process lifetime — the
//! single redb owner, so mcp + CLI search route through the daemon instead of
//! each opening their own. It also publishes a `daemon.json` discovery file,
//! guards concurrent starts, and idle-shuts-down. See
//! `docs/decisions/0023-warm-daemon-mcp-search.md` + `docs/daemon.md`.
//!
//! ## Files (under `~/.onebrain/run/`) — PER-VAULT slots (v3.4.13, #230)
//! Every runtime file is keyed on the bound vault: `daemon-<hash>.*` for a vault
//! (hash = `short_path_hash` of the canonical path), `daemon-none.*` for a
//! vault-less daemon. The slot paths are resolved through the ONE shared resolver
//! in [`crate::commands::daemon_client`] ([`daemon_client::slot_paths_for_id`]),
//! so the daemon (writer) and clients (readers) can't drift.
//! - `daemon-<hash>.pid` — the running daemon's PID (one line, just the integer).
//! - `daemon-<hash>.log` — the detached child's stdout+stderr and `tracing` output.
//! - `daemon-<hash>.json` — discovery record (`port`/`token`/`pid`/`version`/`vault`),
//!   written after bind, removed on clean shutdown.
//! - `daemon-<hash>.lock` — transient per-slot exclusive start lock (see
//!   [`acquire_start_lock`]). Per-slot, so two DIFFERENT vaults starting
//!   concurrently never serialize; same-vault concurrent starts still exclude.
//!
//! ## Concurrent-start guard (v3.4.6)
//! `daemon start` takes an exclusive `O_EXCL`-create lock around the
//! check-then-spawn window (see [`acquire_start_lock`]). The lock records the
//! creating PID and is reclaimed only if that PID is provably dead; the winner
//! holds it until the daemon publishes `daemon.json` (fully bound), so N
//! parallel starts → exactly one daemon. Cross-platform (`create_new` =
//! `O_EXCL`/`CREATE_NEW`).
//!
//! ## Known limitation (residual)
//! - **Recycled session-leader false positive.** The interim liveness/identity
//!   check (`is_alive`) requires the pid to be a live session leader, which
//!   kills the common recycled-PID false positive — but a recycled pid that
//!   *also* happens to be a session leader (another daemon) still slips
//!   through. A full start-time identity check (pid + process start-time in
//!   `daemon.json`) is deferred to the v3.8 daemon refactor.

use crate::commands::daemon_client::{self, DaemonInfo, SlotPaths};
use crate::output::{emit, Envelope, OutputMode};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

// ─────────────────────────────────────────────────────────────────────────
// Envelope data payloads — one `#[derive(Serialize)]` struct per verb, so
// JSON/YAML auto-serialise and the text closures read typed fields.
// ─────────────────────────────────────────────────────────────────────────

/// `daemon status` payload. `pid` is `Some` only when a live daemon was found.
///
/// Beyond the core `running`/`pid` pair, every field is a **best-effort
/// dashboard enrichment** (v3.4.8, #197) resolved from `daemon.json` + the
/// daemon's `/api/health` and `/api/internal/status` probes. All of them are
/// `Option` + `skip_serializing_if` so:
/// - the not-running JSON shape stays the minimal `{running, pid}` it always
///   was, and
/// - a probe failure degrades to ABSENT fields — `daemon status` never exits
///   non-zero because an HTTP probe failed.
///
/// `url` carries the token-bearing webui URL (`http://127.0.0.1:PORT/?token=…`).
/// Printing it to the user's own terminal is fine — the token already lives
/// user-readable in `daemon.json` — but it must NEVER be written to tracing
/// logs (`~/.onebrain/run/daemon.log` is long-lived).
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct DaemonStatusData {
    pub running: bool,
    pub pid: Option<u32>,
    /// CLI version stamped in `daemon.json` by the running daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Epoch seconds the daemon came up (the `daemon.json` mtime — written
    /// once, right after bind).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    /// Idle-shutdown TTL in seconds (`$ONEBRAIN_DAEMON_IDLE_SECS` resolution;
    /// `0` = disabled). Resolved in THIS process's environment — normally the
    /// same user env the daemon started under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_ttl_secs: Option<u64>,
    /// The daemon's actual bound port (from `daemon.json`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Canonical path of the bound vault (`daemon.json.vault`); absent when
    /// the daemon bound vault-less.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault: Option<String>,
    /// Where the served web UI comes from: `"embedded"`, or the
    /// `$ONEBRAIN_DIST` override path. Absent when the daemon didn't report it
    /// (pre-3.4.8 daemon, or the health probe failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webui_source: Option<String>,
    /// The clickable token-bearing webui URL. See the type-level note on token
    /// handling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Whether the daemon holds the search engine (`/api/health.engine_held`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_held: Option<bool>,
    /// Live index stats from `/api/internal/status` (absent when the daemon
    /// holds no engine — that route 503s — or the probe failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_indexed: Option<u64>,
    /// Model line from `/api/internal/status`: configured embed model +
    /// Tier-2 reranker name/readiness/download state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embed_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reranker_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reranker_ready: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reranker_downloaded: Option<bool>,
}

/// `daemon status` payload — the LIST of running daemons, one per per-vault slot
/// (v3.4.13, #230). Multiple warm daemons (one per vault) now coexist, so
/// `daemon status` enumerates all `daemon-*` slots and reports each running one.
/// An empty list = no daemon running. Each entry is the same [`DaemonStatusData`]
/// dashboard the single-daemon status used to emit.
#[derive(Debug, Serialize)]
pub struct DaemonStatusListData {
    pub daemons: Vec<DaemonStatusData>,
}

/// `daemon start` payload. `already_running` distinguishes a fresh spawn from
/// a no-op start; `pid` is the daemon's PID either way.
#[derive(Debug, Serialize)]
pub struct DaemonStartData {
    pub started: bool,
    pub already_running: bool,
    pub pid: u32,
}

/// `daemon stop` payload. `stopped` is `false` when there was nothing to stop
/// (no PID file, or the recorded process was already dead).
#[derive(Debug, Serialize)]
pub struct DaemonStopData {
    pub stopped: bool,
    pub pid: Option<u32>,
}

// ─────────────────────────────────────────────────────────────────────────
// Pure core — no process spawning, no real syscalls. The probe + PID path are
// injected so the four status cases below are exercised by unit tests with a
// `tempdir()` PID file and a stub probe (exactly like `session_init` injects
// `qmd_count`).
// ─────────────────────────────────────────────────────────────────────────

/// Resolve the daemon's status from a PID file, given a liveness probe.
///
/// Four cases, all covered by tests:
/// 1. No PID file → not running.
/// 2. PID file with a live PID → running (carry the PID through).
/// 3. PID file with a dead PID (stale) → not running.
/// 4. PID file with garbage (non-numeric / empty) → not running.
///
/// `is_alive` is the platform liveness probe (production: signal-0 `kill`).
/// Keeping it injected means this function never touches a real process.
fn compute_status(pid_path: &Path, is_alive: impl Fn(u32) -> bool) -> DaemonStatusData {
    match read_pid(pid_path) {
        Some(pid) if is_alive(pid) => DaemonStatusData {
            running: true,
            pid: Some(pid),
            ..Default::default()
        },
        // No file, garbage file, or a recorded-but-dead PID all collapse to
        // "not running". A stale PID file is left on disk here — `run_start`
        // overwrites it and `run_stop` removes it; status is read-only.
        _ => DaemonStatusData {
            running: false,
            pid: None,
            ..Default::default()
        },
    }
}

/// Read + parse the PID file. Returns `None` for a missing file, an unreadable
/// file, or non-numeric contents — every "no usable PID" case folds into
/// `None` so callers treat them uniformly as "not running".
fn read_pid(pid_path: &Path) -> Option<u32> {
    let raw = fs::read_to_string(pid_path).ok()?;
    let pid = raw.trim().parse::<u32>().ok()?;
    // Reject implausible PIDs from a corrupt/crafted file. `0` is not a real
    // process PID, and any value > i32::MAX would wrap *negative* when cast to
    // `i32` for `kill` — and `kill(negative, …)` targets a whole PROCESS GROUP,
    // not a single process. Real PIDs never reach this range, so we drop it.
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    Some(pid)
}

/// Write `pid` to the PID file, creating the parent `run/` directory (0700) first.
fn write_pid(pid_path: &Path, pid: u32) -> Result<()> {
    if let Some(parent) = pid_path.parent() {
        ensure_private_run_dir(parent)?;
    }
    fs::write(pid_path, format!("{pid}\n"))
        .with_context(|| format!("write PID file {}", pid_path.display()))
}

/// Remove the PID file if present. A missing file is success (idempotent
/// cleanup) — only a real I/O error (EACCES, etc.) propagates.
fn remove_pid(pid_path: &Path) -> Result<()> {
    match fs::remove_file(pid_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove PID file {}", pid_path.display())),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Slot-path resolution (v3.4.13, #230). Every runtime file lives in a per-vault
// slot — `daemon-<hash>.{json,pid,lock,log}` — resolved through the ONE shared
// resolver in `daemon_client` ([`daemon_client::slot_paths_for_id`] /
// [`resolve_slot`]) so the daemon (writer) and clients (readers) can't drift.
// ─────────────────────────────────────────────────────────────────────────

/// The slot the management verbs operate on for a given `--vault`-style arg.
///
/// Resolves the bound vault EXACTLY as [`run_internal`] will ([`resolve_daemon_vault`]:
/// a validated real vault, else vault-less), then keys the slot on its canonical
/// id. This is why `run_start` (which pre-creates `slot.log`/`slot.pid`/`slot.lock`)
/// and the detached `__run` child (which writes `slot.json`/`slot.pid`) always
/// agree on the slot: both derive it from the SAME resolved-and-canonicalized
/// bound vault.
fn slot_for_arg(vault_arg: Option<&Path>) -> Result<SlotPaths> {
    let bound = resolve_daemon_vault(vault_arg);
    let vault_id = bound.as_deref().and_then(daemon_client::canonical_vault_id);
    daemon_client::slot_paths_for_id(vault_id.as_deref())
}

/// Create the daemon run dir (`~/.onebrain/run/`) if missing, with private
/// (0700 owner-only) permissions on Unix (fix B).
///
/// The run dir holds the PID file + the (0600) log; both are user-private, so
/// the directory itself should not be group/world-traversable. `DirBuilder`
/// with `.mode(0o700)` applies the mode atomically at creation. On a
/// pre-existing dir we re-assert 0700 so an older, looser dir is tightened.
/// On non-Unix this is a plain `create_dir_all` (mode bits don't apply).
fn ensure_private_run_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create daemon run dir {}", dir.display()))?;
        // `.mode` only applies to dirs this call creates; re-assert 0700 so an
        // existing looser dir is tightened on the next daemon start.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("tighten daemon run dir perms {}", dir.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir).with_context(|| format!("create daemon run dir {}", dir.display()))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Liveness probe — signal 0 on Unix, unsupported elsewhere.
// ─────────────────────────────────────────────────────────────────────────

/// True if `pid` is a live process that *looks like our detached daemon* —
/// i.e. a process that exists AND is a session leader.
///
/// We start the daemon via [`setsid`], which makes it a session leader, so its
/// process-group id equals its pid (`getpgid(pid) == pid`). Requiring that on
/// top of the bare existence check closes the common PID-recycling false
/// positive: after our daemon dies, the kernel may hand its pid to an unrelated
/// process, but a *random* recycled process is almost never a session leader,
/// so it fails the `getpgid` test and we correctly report "not our daemon".
///
/// Two-part probe:
/// - `kill(pid, 0)` — signal 0 performs the permission/existence check without
///   delivering a signal. `Ok` ⇒ exists; `Err(EPERM)` ⇒ exists but owned by
///   someone else (still counts as alive); `Err(ESRCH)`/other ⇒ gone.
/// - `getpgid(pid) == pid` — the recycled-PID guard described above. A
///   `getpgid` error (ESRCH if it raced to exit, etc.) means "not our daemon".
///
/// NOTE: this is an INTERIM identity check. A recycled pid that *also* happens
/// to be a session leader (e.g. some other daemon) is a residual false
/// positive we accept for now. Full start-time identity — a `daemon.json`
/// sidecar recording pid + process start-time — lands in step 2 with the
/// RPC/server infra.
#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::{getpgid, Pid};

    let target = Pid::from_raw(pid as i32);

    // Step 1: does *a* process with this pid exist and is it signalable?
    let exists = match kill(target, None) {
        Ok(()) => true,
        // EPERM: the process exists but we can't signal it — still alive.
        Err(Errno::EPERM) => true,
        // ESRCH (and anything else) → treat as not running.
        Err(_) => false,
    };
    if !exists {
        return false;
    }

    // Step 2: is it a session leader (pgid == pid)? Our daemon always is,
    // because `daemon start` spawns it under `setsid()`. Any `getpgid` error
    // (e.g. ESRCH from a race) means "not our daemon".
    match getpgid(Some(target)) {
        Ok(pgid) => pgid == target,
        Err(_) => false,
    }
}

/// Non-Unix stub: the daemon isn't supported yet, so nothing is ever "alive".
#[cfg(not(unix))]
fn is_alive(_pid: u32) -> bool {
    false
}

// ─────────────────────────────────────────────────────────────────────────
// Concurrent-start guard — cross-platform, no new deps.
//
// `daemon start` used to check "already running?" then spawn in two steps, so
// two parallel calls could both pass the check and both spawn (TOCTOU). The
// guard closes that race with an exclusive `O_EXCL` create of a lock file
// (`daemon.lock`): the FIRST caller creates it and proceeds; a concurrent caller
// gets `AlreadyExists` and backs off ("already running"). This is portable
// (`create_new` maps to `O_EXCL` on Unix and `CREATE_NEW` on Windows) so the
// 3-OS CI matrix passes without nix's `fs` feature / a `flock` dep.
//
// Staleness: a crashed `daemon start` can leave `daemon.lock` behind. The lock
// is SELF-DESCRIBING — it records the PID of the `daemon start` process that
// created it. On `AlreadyExists` we read that PID and probe it:
//   - lock PID ALIVE            → a concurrent starter (or a held lock) → contended.
//   - lock PID DEAD             → the creator crashed → stale → reclaim once.
//   - lock PID unreadable/empty → a concurrent creator that hasn't written its
//                                 PID yet → assume live → contended (conservative;
//                                 never reclaim a lock we can't prove is stale).
// This is why the guard keys off the LOCK's PID, not the daemon's PID file:
// during a fresh concurrent start the winner hasn't written the daemon PID yet,
// so a daemon-PID probe would wrongly read "not running" and every loser would
// reclaim + spawn. The lock-creating process, by contrast, is demonstrably alive
// for the whole critical section.
// ─────────────────────────────────────────────────────────────────────────

/// RAII holder of the exclusive start lock. Dropping it removes the lock file
/// so the next `daemon start` isn't blocked. The lock only needs to survive the
/// brief check-then-spawn window in [`run_start`]; the long-lived daemon is
/// tracked by the PID file + `daemon.json`, not this lock.
struct StartGuard {
    path: PathBuf,
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Outcome of trying to take the start lock.
enum StartLock {
    /// We won the lock — proceed to spawn. Hold the guard for the critical section.
    Acquired(StartGuard),
    /// Another live `daemon start`/daemon holds it — the caller reports
    /// "already running" using the recorded PID (from `daemon.json` / PID file).
    Contended,
}

/// Try to take the exclusive start lock, clearing a lock left by a crashed
/// `daemon start` exactly once. `pid_is_live(pid)` probes whether the process
/// that created the lock is still alive — the reclaim decision keys off the
/// LOCK's recorded PID, never the daemon PID file (see the module comment for
/// why: during a fresh concurrent start the daemon PID isn't written yet).
///
/// Pure of process spawning + injected `pid_is_live` so the three outcomes
/// (fresh acquire / live-holder contended / stale-reclaim) are unit-testable.
fn acquire_start_lock(lock_path: &Path, pid_is_live: impl Fn(u32) -> bool) -> Result<StartLock> {
    if let Some(parent) = lock_path.parent() {
        ensure_private_run_dir(parent)?;
    }
    match create_lock(lock_path) {
        Ok(()) => Ok(StartLock::Acquired(StartGuard {
            path: lock_path.to_path_buf(),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            match lock_owner_state(lock_path, &pid_is_live) {
                // The creator is alive (or we can't prove it's dead) → respect it.
                LockOwner::Live | LockOwner::Unknown => Ok(StartLock::Contended),
                // The creator is provably dead → reclaim once and retry.
                LockOwner::Dead => {
                    remove_pid_lock_stale(lock_path)?;
                    match create_lock(lock_path) {
                        Ok(()) => Ok(StartLock::Acquired(StartGuard {
                            path: lock_path.to_path_buf(),
                        })),
                        // Someone grabbed it in the retry window → contended.
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            Ok(StartLock::Contended)
                        }
                        Err(e) => Err(e)
                            .with_context(|| format!("create start lock {}", lock_path.display())),
                    }
                }
            }
        }
        Err(e) => Err(e).with_context(|| format!("create start lock {}", lock_path.display())),
    }
}

/// State of the process that created an existing lock file.
#[derive(Debug, PartialEq, Eq)]
enum LockOwner {
    /// The recorded PID is a live process → a real concurrent holder.
    Live,
    /// The recorded PID is dead → the creator crashed; the lock is stale.
    Dead,
    /// The lock's PID couldn't be read (empty/partial write by a concurrent
    /// creator, or unreadable) → treat as live (never reclaim on a guess).
    Unknown,
}

/// Classify an existing lock by probing its recorded PID with `pid_is_live`.
/// Pure (no I/O beyond reading the lock) so the reclaim decision is testable.
fn lock_owner_state(lock_path: &Path, pid_is_live: &impl Fn(u32) -> bool) -> LockOwner {
    match read_lock_pid(lock_path) {
        Some(pid) if pid_is_live(pid) => LockOwner::Live,
        Some(_) => LockOwner::Dead,
        None => LockOwner::Unknown,
    }
}

/// Read the PID recorded in a lock file, or `None` if missing / empty / garbage.
fn read_lock_pid(lock_path: &Path) -> Option<u32> {
    let raw = fs::read_to_string(lock_path).ok()?;
    raw.trim().parse::<u32>().ok().filter(|&p| p != 0)
}

/// Raw process-existence probe: `kill(pid, 0)` with no session-leader check.
/// Distinct from [`is_alive`] (which requires the pid to be a *daemon* session
/// leader): the lock's PID is the `daemon start` PROCESS, not the setsid daemon,
/// so it must not be gated on session-leadership.
#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true, // exists, owned by someone else
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn pid_exists(_pid: u32) -> bool {
    // Non-unix: no cheap raw probe wired yet; assume live so we never reclaim a
    // lock we can't verify is stale (conservative — matches `LockOwner::Unknown`).
    true
}

/// Exclusive-create the lock file (`O_EXCL` / `CREATE_NEW`) and record OUR PID
/// (the `daemon start` process) so a later contender can probe whether the
/// creator is still alive. The create itself is the lock; the PID makes stale
/// detection possible.
fn create_lock(lock_path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)?;
    write!(f, "{}", std::process::id())?;
    f.flush()?;
    Ok(())
}

/// Remove a stale lock file (best-effort; a missing file is fine).
fn remove_pid_lock_stale(lock_path: &Path) -> Result<()> {
    match fs::remove_file(lock_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("remove stale start lock {}", lock_path.display()))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Public wiring — resolve real paths/probe, do I/O, emit the envelope.
// ─────────────────────────────────────────────────────────────────────────

/// `onebrain daemon status` — read the PID file, probe liveness, report.
///
/// When the daemon is running, the report is a full dashboard: `daemon.json`
/// supplies port/token/version/vault, `GET /api/health` supplies
/// `engine_held` + the webui source, and `GET /api/internal/status` supplies
/// the live index + model fields. Every probe is best-effort — a failure
/// leaves its fields absent, and `status` still exits 0 (see
/// [`DaemonStatusData`]). Probes NEVER start, stop, or restart a daemon.
pub fn run_status(mode: &OutputMode) -> Result<()> {
    // Enumerate every per-vault slot and report each RUNNING daemon (v3.4.13,
    // #230). A slot whose json is present but whose daemon is dead (stale) is
    // skipped here — status stays read-only; doctor's daemon check flags stale
    // slots. A vault-less daemon (`daemon-none.*`) is enumerated like any other.
    // The legacy pre-v3.4.13 machine-wide slot is checked too, so a live
    // pre-upgrade daemon (invisible to `all_slots`) still shows up here (LOW-2)
    // instead of the command reporting "not running" while it holds the lock.
    let mut daemons = Vec::new();
    let slots = daemon_client::all_slots()?
        .into_iter()
        .chain(std::iter::once(daemon_client::legacy_slot()?));
    for slot in slots {
        let mut data = compute_status(&slot.pid, is_alive);
        if !data.running {
            continue;
        }
        if let Ok(Some(info)) = DaemonInfo::read(&slot.json) {
            let handle = daemon_client::DaemonHandle::new(info.clone());
            let health = handle.probe_health();
            let internal = handle.probe_status_no_retry();
            enrich_status(
                &mut data,
                &info,
                file_mtime_secs(&slot.json),
                resolve_idle_secs(),
                health.as_ref(),
                internal.as_ref(),
            );
        }
        daemons.push(data);
    }
    let env = Envelope::ok("daemon.status", None, DaemonStatusListData { daemons });
    emit(
        &env,
        mode,
        std::io::stdout().lock(),
        render_status_list_text,
    )?;
    Ok(())
}

/// Render the multi-daemon `daemon status` report: one grouped block per running
/// daemon (reusing [`render_status_text`] via a per-daemon envelope), or the
/// plain "daemon not running" when the list is empty. A count header precedes
/// the blocks when more than one daemon runs.
fn render_status_list_text(env: &Envelope<DaemonStatusListData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.daemons.is_empty() {
        return "daemon not running".to_string();
    }
    let mut out = String::new();
    if d.daemons.len() > 1 {
        out.push_str(&format!("{} daemons running\n\n", d.daemons.len()));
    }
    for (i, daemon) in d.daemons.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        let one = Envelope::ok("daemon.status", None, daemon.clone());
        out.push_str(&render_status_text(&one));
    }
    out
}

/// Fill the dashboard fields of `data` from the discovery record + the two
/// HTTP probes. Pure (no I/O — the probes are passed in already-resolved) so
/// the field mapping, including the degrade-to-absent policy, is unit-testable
/// without a live daemon.
///
/// - `health` is the parsed `GET /api/health` body (`None` = probe failed).
/// - `internal` is the parsed `GET /api/internal/status` body (`None` = probe
///   failed OR the daemon holds no engine — that route 503s).
/// - `started_epoch` is the `daemon.json` mtime (written once, after bind).
fn enrich_status(
    data: &mut DaemonStatusData,
    info: &crate::commands::daemon_client::DaemonInfo,
    started_epoch: Option<u64>,
    idle_ttl_secs: u64,
    health: Option<&serde_json::Value>,
    internal: Option<&serde_json::Value>,
) {
    data.version = Some(info.version.clone());
    data.port = Some(info.port);
    data.vault = info.vault.clone();
    // The one token-bearing line (stdout only — never tracing; see the
    // DaemonStatusData doc).
    data.url = Some(format!(
        "http://127.0.0.1:{}/?token={}",
        info.port, info.token
    ));
    data.started = started_epoch;
    data.idle_ttl_secs = Some(idle_ttl_secs);

    if let Some(h) = health {
        data.engine_held = h.get("engine_held").and_then(serde_json::Value::as_bool);
        // `dist_dir` (v3.4.8): present-and-null = the binary's own UI, string
        // = the $ONEBRAIN_DIST override. A MISSING key (pre-3.4.8 daemon)
        // stays `None` — unknown, not "embedded". With no override, the
        // sibling `embedded_ui` flag distinguishes a real bundled UI from the
        // asset-less build's placeholder page — `false` must not report a
        // dishonest "embedded". The flag ships in the same daemon version as
        // `dist_dir`, so on the null arm an absent flag can only be a skewed
        // (newer-client/older-daemon) probe — default to "embedded", the
        // release-binary truth.
        data.webui_source = match h.get("dist_dir") {
            Some(serde_json::Value::Null) => {
                match h.get("embedded_ui").and_then(serde_json::Value::as_bool) {
                    Some(false) => Some("placeholder".to_string()),
                    _ => Some("embedded".to_string()),
                }
            }
            Some(serde_json::Value::String(dist)) => Some(dist.clone()),
            _ => None,
        };
    }

    if let Some(s) = internal {
        let num = |k: &str| s.get(k).and_then(serde_json::Value::as_u64);
        let text = |k: &str| {
            s.get(k)
                .and_then(serde_json::Value::as_str)
                .map(String::from)
        };
        let flag = |k: &str| s.get(k).and_then(serde_json::Value::as_bool);
        data.doc_count = num("doc_count");
        data.pending_total = num("pending_total");
        data.last_indexed = num("last_indexed");
        data.embed_model = text("embed_model");
        data.reranker_model = text("reranker_model");
        data.reranker_ready = flag("reranker_ready");
        data.reranker_downloaded = flag("reranker_downloaded");
    }
}

/// `path`'s mtime as epoch seconds, or `None` if unreadable.
fn file_mtime_secs(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Humanize an idle-TTL for the status dashboard: `0` is the documented
/// "disabled" sentinel; whole hours/minutes render as such; anything else
/// falls back to raw seconds.
fn format_ttl(secs: u64) -> String {
    match secs {
        0 => "disabled (runs until stopped)".to_string(),
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

fn render_status_text(env: &Envelope<DaemonStatusData>) -> String {
    use crate::commands::search_status::format_local;
    use crate::output::{item, section};

    let d = env.data.as_ref().expect("ok envelope always has data");
    let pid = match d.pid {
        Some(pid) if d.running => pid,
        _ => return "daemon not running".to_string(),
    };

    // Grouped-status convention (matches `search status` / `serve`'s banner):
    // emoji only on section headers, plain-space-indented label rows. Sections
    // whose fields are all absent (a failed probe, an engine-less daemon) are
    // omitted entirely — the dashboard degrades, it never errors.
    let mut lines = Vec::new();

    lines.push(section("🟢", "Process"));
    lines.push(item("Running", &format!("yes (pid {pid})")));
    if let Some(v) = &d.version {
        lines.push(item("Version", v));
    }
    if let Some(started) = d.started.and_then(format_local) {
        lines.push(item("Started", &started));
    }
    if let Some(ttl) = d.idle_ttl_secs {
        lines.push(item("Idle TTL", &format_ttl(ttl)));
    }

    if d.port.is_some() || d.vault.is_some() || d.webui_source.is_some() {
        lines.push(String::new());
        lines.push(section("🔌", "Bind"));
        if let Some(port) = d.port {
            lines.push(item("Port", &port.to_string()));
        }
        match &d.vault {
            Some(vault) => lines.push(item("Vault", vault)),
            // We read daemon.json (port present) and it carried no vault →
            // the daemon genuinely bound vault-less; say so rather than hide it.
            None if d.port.is_some() => {
                lines.push(item("Vault", "none bound (vault endpoints 503)"))
            }
            None => {}
        }
        if let Some(source) = &d.webui_source {
            // "placeholder" is the machine token (JSON); spell it out for humans.
            let shown = if source == "placeholder" {
                "placeholder page (no bundled web UI in this binary)"
            } else {
                source.as_str()
            };
            lines.push(item("Web UI", shown));
        }
    }

    if let Some(url) = &d.url {
        lines.push(String::new());
        lines.push(section("🌐", "Webui"));
        lines.push(item("URL", url));
    }

    if d.engine_held.is_some() || d.doc_count.is_some() {
        lines.push(String::new());
        lines.push(section("🧠", "Engine"));
        if let Some(held) = d.engine_held {
            lines.push(item("Held", if held { "✅  yes" } else { "—  no" }));
        }
        if let Some(docs) = d.doc_count {
            lines.push(item("Docs", &docs.to_string()));
            lines.push(item(
                "Pending",
                &d.pending_total.unwrap_or_default().to_string(),
            ));
            match d.last_indexed.and_then(format_local) {
                Some(when) => lines.push(item("Last indexed", &when)),
                None => lines.push(item("Last indexed", "never")),
            }
        }
    }

    if d.embed_model.is_some() || d.reranker_model.is_some() {
        lines.push(String::new());
        lines.push(section("🎯", "Models"));
        if let Some(model) = &d.embed_model {
            lines.push(item("Embed", model));
        }
        if let Some(model) = &d.reranker_model {
            let readiness = match (d.reranker_ready, d.reranker_downloaded) {
                (Some(true), _) => " (ready)",
                (Some(false), Some(false)) => " (not downloaded)",
                (Some(false), _) => " (not ready)",
                (None, _) => "",
            };
            lines.push(item("Reranker", &format!("{model}{readiness}")));
        }
    }

    lines.join("\n")
}

/// Resolve the vault to convey to the detached `__run` child at `start` time
/// (#262). Every sibling verb (`serve`, `search`, `token`, `doctor`) walks up
/// from cwd to find a vault; bare `daemon start` didn't, because the ONLY
/// place that ever resolved a vault was [`resolve_daemon_vault`] inside the
/// detached child — but by the time that runs, `spawn_detached_run`'s
/// `pre_exec` has already `chdir("/")`'d the child (a long-lived daemon must
/// not pin whatever directory `start` happened to run from). A walk-up
/// inside the child is therefore structurally useless; it has to happen here,
/// in the parent, while cwd still means something.
///
/// Precedence, UNCHANGED from before this function existed for the first two
/// rungs, plus one new rung inserted below them:
/// 1. Explicit `--vault <arg>` — returned untouched (even if it isn't a real
///    vault). Validation still happens exactly once, downstream, in the
///    child's [`resolve_daemon_vault`] (soft-fail: warn + serve vault-less).
///    Routing this through the stricter `vault_ctx::resolve` would hard-error
///    on an invalid explicit path, which is NOT today's behaviour and #262
///    explicitly says to preserve it.
/// 2. `$ONEBRAIN_VAULT` set, arg absent — returns `None` here so no `--vault`
///    flag is passed; the child inherits the same env and resolves it itself
///    via [`resolve_daemon_vault`]'s existing fallback (zero change).
/// 3. **NEW:** both absent — walk up from `$PWD` with
///    [`onebrain_core::find_vault_root`], the same helper `serve`/`search`/
///    `token`/`doctor` use. Found root is passed through as if it were an
///    explicit `--vault`.
/// 4. Nothing found (or cwd unreadable) — `None`. `daemon start` still
///    succeeds vault-less, same as today.
fn resolve_start_vault(vault: Option<&Path>) -> Option<PathBuf> {
    if let Some(v) = vault {
        return Some(v.to_path_buf());
    }
    if std::env::var_os("ONEBRAIN_VAULT").is_some() {
        return None;
    }
    // Rung 3 (both explicit flag and env absent): walk up from cwd. NOTE this
    // rung means a *vault-less* spawn intent would incidentally bind cwd's
    // vault. It is only ever reached from a bare manual `daemon start`; the
    // programmatic spawn path (`daemon_client::ensure_running` →
    // `spawn_daemon_start`) always threads an already-resolved `--vault`
    // (rung 1) for every current caller, so `ensure_running(None)`'s
    // vault-less-spawn semantics are unaffected. A future caller reintroducing
    // `ensure_running(None)` on a walk-uppable cwd would want to reconsider.
    let cwd = std::env::current_dir().ok()?;
    onebrain_core::find_vault_root(&cwd).map(|root| root.as_path().to_path_buf())
}

/// `onebrain daemon start` — spawn a detached `__run` child if not already
/// running, record its PID, report.
///
/// `vault` (from `--vault`) is threaded to the detached `__run` child so a
/// caller conveys the vault the daemon should bind EXPLICITLY, rather than
/// mutating `$ONEBRAIN_VAULT` in the parent process (which is unsound under
/// concurrent reads and deprecated since Rust 1.81). When `None`, the child
/// falls back to `$ONEBRAIN_VAULT` — the pre-`--vault` behaviour. When BOTH
/// are absent (the bare `daemon start` case), [`resolve_start_vault`] adds a
/// 3rd rung — walking up from cwd — before spawning; see its doc comment
/// for why that has to happen here in the parent (#262).
pub fn run_start(mode: &OutputMode, vault: Option<&Path>) -> Result<()> {
    // Resolve the vault to convey to the child (walk-up for a bare start), then
    // the SLOT that vault maps to. `run_start` pre-creates this slot's log/pid/
    // lock; the detached `__run` child (spawned with the SAME `--vault`) derives
    // the identical slot and writes its json/pid there — so they never diverge.
    let effective_vault = resolve_start_vault(vault);
    let slot = slot_for_arg(effective_vault.as_deref())?;

    // Already-running guard: a live PID file in THIS slot means a no-op start.
    if let DaemonStatusData {
        running: true,
        pid: Some(pid),
        ..
    } = compute_status(&slot.pid, is_alive)
    {
        return emit_already_running(mode, pid);
    }

    // Take THIS SLOT's exclusive start lock so two parallel `daemon start` calls
    // for the SAME vault can't both spawn (see [`acquire_start_lock`]). Two
    // DIFFERENT vaults take DIFFERENT slot locks, so they never serialize — the
    // multi-vault property. We won it → hold the guard across the check-then-spawn
    // window. The reclaim decision probes the LOCK's own creator PID
    // (`pid_exists`), not the daemon PID file (unwritten during a fresh start).
    let _guard = match acquire_start_lock(&slot.lock, pid_exists)? {
        StartLock::Acquired(g) => g,
        // A concurrent starter won. Report the daemon it started as
        // already-running (its real PID — polled briefly so we don't emit a
        // misleading `pid 0` while the winner is a few ms from writing it;
        // Copilot AI-finding #1).
        StartLock::Contended => {
            return emit_already_running(mode, contended_winner_pid(&slot));
        }
    };

    // Re-check under the lock: another starter may have finished between our
    // first check and taking the lock (they held the lock, spawned, released).
    if let DaemonStatusData {
        running: true,
        pid: Some(pid),
        ..
    } = compute_status(&slot.pid, is_alive)
    {
        return emit_already_running(mode, pid);
    }

    // Spawn the detached child and record its PID in this slot.
    let pid = spawn_detached_run(&slot.log, effective_vault.as_deref())
        .context("spawn detached daemon process")?;
    write_pid(&slot.pid, pid)?;

    // Wait — STILL HOLDING THE SLOT LOCK — until the daemon is fully up (it has
    // published its `daemon-<hash>.json` after binding). Releasing the lock the
    // instant we spawn would let a serialized same-vault racer take it and,
    // seeing the child not yet bound (no json, PID not yet a confirmed session
    // leader), spawn a SECOND daemon for this slot. Holding until the daemon
    // advertises readiness means every later racer sees a confirmed-running
    // daemon and backs off. Bounded so a wedged child can't hang `start` forever.
    //
    // #278 daemon-startup audit: `__run`'s bind-then-announce ordering was
    // already correct (its `on_bind` callback writes `daemon.json` only after a
    // successful bind — see `run_internal`). But THIS caller had the same
    // "success-implying output before the operation succeeds" anti-pattern one
    // layer up: it used to report `started: true` unconditionally, even when
    // `ReadyOutcome::Died` meant the child had already exited (bind failure or
    // early crash) — the parent would print "daemon started" for a daemon that
    // was, at that moment, provably not running. A `TimedOut` outcome (child
    // still alive, just slow to bind within the 5s window) stays best-effort
    // `started: true`, matching the pre-existing bounded-wait tradeoff.
    if let ReadyOutcome::Died = wait_until_ready(pid, &slot.json, std::time::Duration::from_secs(5))
    {
        anyhow::bail!(
            "daemon process (pid {pid}) exited before it finished binding its HTTP listener; \
             check the daemon log for the underlying error"
        );
    }

    let data = DaemonStartData {
        started: true,
        already_running: false,
        pid,
    };
    let env = Envelope::ok("daemon.start", None, data);
    emit(&env, mode, std::io::stdout().lock(), render_start_text)?;
    Ok(())
}

/// Poll briefly for a concurrent starter's PID to land in THIS slot's PID file,
/// so a `Contended` start reports the winner's real PID instead of a bare `0`
/// (Copilot AI-finding #1). The winner takes the lock, spawns, then writes the
/// PID file, so a loser can lose the race by microseconds; a short poll (≤10 ×
/// 25ms = 250ms) resolves the real PID in the common case. Reads the PID FILE
/// directly (not `compute_status`) so it returns as soon as the winner writes
/// it, before the child has finished `setsid`. Falls back to `0` only if the
/// winner still hasn't published a PID after the wait.
fn contended_winner_pid(slot: &SlotPaths) -> u32 {
    for _ in 0..10 {
        if let Some(pid) = read_pid(&slot.pid) {
            return pid;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    read_pid(&slot.pid).unwrap_or(0)
}

/// Outcome of [`wait_until_ready`]: whether the caller may trust that the
/// daemon actually came up, or must NOT report success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadyOutcome {
    /// Confirmed: `discovery` exists and `pid` is a live session leader.
    Ready,
    /// The child exited before publishing `discovery` (e.g. a bind failure) —
    /// a caller must NOT report this as a successful start.
    Died,
    /// Still alive at the deadline but never confirmed readiness — best-effort
    /// (see the caller's comment on why this stays a soft "probably fine").
    TimedOut,
}

/// Poll until the freshly-spawned daemon is READY — it has published its
/// `discovery` file (`daemon.json`, written after it binds) AND `pid` is a live
/// session leader — or `timeout` elapses (best-effort). Called by `run_start`
/// while STILL holding the start lock, so a serialized concurrent starter always
/// observes a fully-up daemon and backs off instead of spawning a second one.
///
/// If the child dies early ([`child_has_exited`]), stop waiting immediately and
/// report [`ReadyOutcome::Died`] — there's no point holding the lock for a
/// daemon that already failed, and the caller must not claim it started.
fn wait_until_ready(pid: u32, discovery: &Path, timeout: std::time::Duration) -> ReadyOutcome {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if is_alive(pid) && discovery.exists() {
            return ReadyOutcome::Ready;
        }
        // Child forked but exited before binding → don't spin the full timeout.
        if child_has_exited(pid) {
            return ReadyOutcome::Died;
        }
        if std::time::Instant::now() >= deadline {
            return ReadyOutcome::TimedOut;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// `true` when `pid` — the direct child [`spawn_detached_run`] just forked in
/// THIS `run_start` call — has already exited.
///
/// Uses a non-blocking `waitpid(pid, WNOHANG)` REAP rather than the
/// `kill(pid, 0)` liveness probe [`is_alive`]/[`pid_exists`] rely on: a
/// process that exits before its parent reaps it becomes a ZOMBIE, and a
/// zombie's pid slot stays valid for `kill()` — signaling a zombie still
/// succeeds — so `kill(pid, 0)` alone cannot distinguish "exited, awaiting
/// reap" from "genuinely still running". `waitpid` is the only call that can
/// tell the difference, and reaping here (this process is the direct parent,
/// so it's entitled to) also means a bind-failure death never leaks a zombie
/// for `run_start`'s own short remaining lifetime (#278 audit finding: without
/// this, a dead-on-arrival daemon child would appear "alive" to `kill(pid, 0)`
/// for the ENTIRE [`wait_until_ready`] window, since nothing ever reaped it —
/// masking a confirmable bind failure as `ReadyOutcome::TimedOut`, which
/// `run_start` treats as best-effort success).
///
/// `EINTR` (interrupted by a signal before `waitpid` could answer) is retried
/// inline rather than punted to the caller's next 25ms poll tick — at the
/// deadline edge that punt could turn a confirmable death into a spurious
/// `TimedOut`. Any other `Err` (e.g. `ECHILD` — not literally our child, which
/// only happens in tests that pass an arbitrary pid) falls back to the plain
/// existence probe: a pid that doesn't exist at all is definitely dead.
#[cfg(unix)]
fn child_has_exited(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;
    loop {
        match waitpid(Pid::from_raw(pid as i32), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return false,
            Ok(_) => return true, // Exited / Signaled / etc. — reaped, confirmed dead.
            Err(Errno::EINTR) => continue,
            Err(_) => return !bare_pid_or_session_alive(pid),
        }
    }
}

/// Non-Unix stub — the daemon isn't supported yet (see [`is_alive`]).
#[cfg(not(unix))]
fn child_has_exited(_pid: u32) -> bool {
    false
}

/// `true` while the child process still exists at all (bare `pid_exists`) —
/// the fallback existence probe [`child_has_exited`] uses when `waitpid`
/// itself can't answer (e.g. `pid` isn't literally our child).
fn bare_pid_or_session_alive(pid: u32) -> bool {
    pid_exists(pid)
}

/// Emit the `already_running` start envelope for `pid`.
fn emit_already_running(mode: &OutputMode, pid: u32) -> Result<()> {
    let data = DaemonStartData {
        started: false,
        already_running: true,
        pid,
    };
    let env = Envelope::ok("daemon.start", None, data);
    emit(&env, mode, std::io::stdout().lock(), render_start_text)?;
    Ok(())
}

fn render_start_text(env: &Envelope<DaemonStartData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.already_running {
        format!("daemon already running (pid {})", d.pid)
    } else {
        format!("daemon started (pid {})", d.pid)
    }
}

/// Remove ALL of a slot's runtime files: the PID file, the `daemon-<hash>.json`
/// discovery record, and the `daemon-<hash>.lock` start lock. Best-effort (a
/// missing file is fine).
///
/// Clearing the lock here is the CLI recovery for a WEDGED start lock: if a
/// `daemon start` is SIGKILL'd inside the check-then-spawn window it leaves the
/// lock behind, and if the OS later recycles that PID onto an unrelated live
/// process, `pid_exists` stays true so `acquire_start_lock` would report
/// "already running" forever with no way out but a manual `rm`. `daemon stop`
/// now unwedges it.
fn clear_slot_files(slot: &SlotPaths) {
    let _ = remove_pid(&slot.pid);
    let _ = DaemonInfo::remove(&slot.json);
    let _ = remove_pid_lock_stale(&slot.lock);
}

/// SIGTERM the daemon occupying `slot` (if live), wait briefly for it to exit,
/// then clear the slot's PID / discovery / lock files. Returns `(stopped, pid)`.
///
/// The slot-aware core shared by the `daemon stop` verb AND
/// [`daemon_client`](crate::commands::daemon_client)'s version-skew restart —
/// keeping the SIGTERM + cleanup in one place (rather than the client shelling
/// `onebrain daemon stop`, which would target the cwd's slot, not the intended
/// one). An error only on a failed SIGTERM (the old daemon may still hold the
/// engine lock, so the caller needs to know).
pub(crate) fn stop_slot(slot: &SlotPaths) -> Result<(bool, Option<u32>)> {
    match compute_status(&slot.pid, is_alive) {
        DaemonStatusData {
            running: true,
            pid: Some(pid),
            ..
        } => {
            terminate(pid).with_context(|| format!("signal daemon pid {pid}"))?;
            // Best-effort: the daemon's SIGTERM handler removes its own PID +
            // discovery files, but if it died uncleanly (or isn't ours — a
            // recycled session leader could slip through) we still clear the
            // slot's files so a later `start` isn't blocked and no client
            // connects to a dead daemon via a stale record.
            clear_slot_files(slot);
            Ok((true, Some(pid)))
        }
        // Nothing live to stop. Clear any stale files so the slate is clean.
        _ => {
            clear_slot_files(slot);
            Ok((false, None))
        }
    }
}

/// `onebrain daemon stop [--vault <path>] [--all]` — stop a per-vault daemon.
///
/// - `--all` → stop EVERY slot's daemon (and clear every wedged lock).
/// - `--vault <path>` → stop that vault's slot.
/// - neither → stop the cwd-resolved slot (walk-up like `serve`/`search`; the
///   vault-less sentinel slot when cwd isn't inside a vault).
pub fn run_stop(mode: &OutputMode, vault: Option<&Path>, all: bool) -> Result<()> {
    if all {
        return run_stop_all(mode);
    }
    let slot = match vault {
        // Explicit `--vault X`: target X's slot by canonicalizing X DIRECTLY,
        // NOT through `slot_for_arg`/`resolve_daemon_vault` (which re-validates X
        // is still a vault). Start/stop symmetry (LOW-1): the daemon's slot was
        // keyed on `canonical(X)` at start; if X's `onebrain.yml` has since been
        // removed while the directory still exists, `resolve_daemon_vault` would
        // drop X to the vault-less slot and stop the WRONG daemon. Canonicalizing
        // X directly still finds X's slot as long as the directory exists. A
        // MOVED/DELETED X can't canonicalize (`None`) — its slot is inherently
        // underivable from a path that no longer maps to it, so we refuse rather
        // than fall back to the vault-less slot (which would stop the wrong one).
        Some(x) => match daemon_client::canonical_vault_id(x) {
            Some(id) => daemon_client::slot_paths_for_id(Some(&id))?,
            None => {
                let env = Envelope::ok(
                    "daemon.stop",
                    None,
                    DaemonStopData {
                        stopped: false,
                        pid: None,
                    },
                );
                emit(&env, mode, std::io::stdout().lock(), render_stop_text)?;
                return Ok(());
            }
        },
        // Default: cwd-resolved slot. `resolve_start_vault(None)` returns `None`
        // in the `$ONEBRAIN_VAULT`-set case, and `slot_for_arg(None)` then
        // re-resolves the env vault — so the env vault's slot is targeted, not the
        // vault-less one.
        None => {
            let effective_vault = resolve_start_vault(None);
            slot_for_arg(effective_vault.as_deref())?
        }
    };
    let (stopped, pid) = stop_slot(&slot)?;
    let env = Envelope::ok("daemon.stop", None, DaemonStopData { stopped, pid });
    emit(&env, mode, std::io::stdout().lock(), render_stop_text)?;
    Ok(())
}

/// `daemon stop --all` — stop every per-vault daemon on the machine. Reports how
/// many were stopped (the `pid` field carries the last one, for the text line).
fn run_stop_all(mode: &OutputMode) -> Result<()> {
    let mut stopped = 0usize;
    let mut last_pid = None;
    // Every per-vault slot, PLUS the legacy pre-v3.4.13 single slot (so an
    // upgrade leaves nothing running). `stop_slot` on an absent slot is a
    // harmless no-op that just clears any stale files.
    let mut slots = daemon_client::all_slots()?;
    slots.push(daemon_client::legacy_slot()?);
    for slot in slots {
        let (was_stopped, pid) = stop_slot(&slot)?;
        if was_stopped {
            stopped += 1;
            last_pid = pid;
        }
    }
    let env = Envelope::ok(
        "daemon.stop",
        None,
        DaemonStopData {
            stopped: stopped > 0,
            pid: last_pid,
        },
    );
    emit(&env, mode, std::io::stdout().lock(), move |e| {
        render_stop_all_text(e, stopped)
    })?;
    Ok(())
}

/// Text line for `daemon stop --all`: `stopped N daemon(s)` or `no daemons running`.
fn render_stop_all_text(_env: &Envelope<DaemonStopData>, stopped: usize) -> String {
    match stopped {
        0 => "no daemons running".to_string(),
        1 => "stopped 1 daemon".to_string(),
        n => format!("stopped {n} daemons"),
    }
}

fn render_stop_text(env: &Envelope<DaemonStopData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    match (d.stopped, d.pid) {
        (true, Some(pid)) => format!("daemon stopped (pid {pid})"),
        _ => "daemon not running".to_string(),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Detached spawn (Unix) — re-exec ourselves running `daemon __run`, in a new
// session, with stdio pointed at the log file.
// ─────────────────────────────────────────────────────────────────────────

/// Spawn `<current-exe> daemon __run` as a detached background process and
/// return its PID. The child:
/// - runs in its OWN session ([`setsid`] in a `pre_exec` hook) so it survives
///   the parent terminal closing and isn't in our process group, and
/// - has stdout+stderr redirected to `log_path` (stdin → `/dev/null`).
///
/// No `unsafe fork`, no libc double-fork — `std::process::Command` does the
/// `fork`+`exec` for us; we only add `setsid` + the stdio redirection.
#[cfg(unix)]
fn spawn_detached_run(log_path: &Path, vault: Option<&Path>) -> Result<u32> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt; // for `.mode(...)`
    use std::os::unix::process::CommandExt; // for `pre_exec`
    use std::process::{Command, Stdio};

    // Ensure the run dir exists (0700) before opening the log inside it.
    if let Some(parent) = log_path.parent() {
        ensure_private_run_dir(parent)?;
    }

    let exe = std::env::current_exe().context("resolve current executable path")?;

    // Append so restarts accumulate history rather than truncating the log.
    //
    // SECURITY (fix B): the daemon log can contain operational detail and lives
    // for the life of the install, so it must be private to the user. `.mode`
    // sets the permission bits at CREATE time — owner read/write only (0600),
    // no group/other — so the log is never created world-readable. `.mode` only
    // applies when this call creates the file; for an existing log we re-assert
    // 0600 below so an older, looser file is tightened on the next start.
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    // Re-assert 0600 on an already-existing log (the `.mode` above is a no-op
    // when the file already exists).
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(log_path, perms)
            .with_context(|| format!("tighten daemon log perms {}", log_path.display()))?;
    }
    // The child needs its own owned handle for each of stdout/stderr.
    let log_err = log
        .try_clone()
        .context("clone daemon log handle for stderr")?;

    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .arg("__run")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // Convey the vault to the child EXPLICITLY via `--vault` rather than
    // mutating `$ONEBRAIN_VAULT` in the parent (unsound/deprecated). The child
    // still honours `$ONEBRAIN_VAULT` as a fallback when no `--vault` is passed.
    if let Some(v) = vault {
        cmd.arg("--vault").arg(v);
    }

    // SAFETY: `pre_exec` runs in the forked child *before* exec. Both setsid()
    // and chdir() are async-signal-safe and we touch no shared state (no
    // allocation, no locks), which is the contract `pre_exec` requires.
    // - setsid(): detaching into a new session makes the child a session leader
    //   with no controlling terminal.
    // - chdir("/"): a long-lived daemon must not pin the cwd that `daemon start`
    //   happened to run from (which would block that directory from being
    //   unmounted/removed); root is always present.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            // `chdir("/")`. We call `libc` directly rather than
            // `nix::unistd::chdir` because the latter is gated behind nix's `fs`
            // feature, which we don't enable (and don't want to pull in just for
            // one call). `c"/"` is a compile-time NUL-terminated C string, so no
            // allocation happens inside this async-signal-safe context.
            if libc::chdir(c"/".as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().context("spawn detached daemon child")?;
    Ok(child.id())
}

/// Non-Unix stub — the detached-spawn path is Unix-only for now.
#[cfg(not(unix))]
fn spawn_detached_run(_log_path: &Path, _vault: Option<&Path>) -> Result<u32> {
    anyhow::bail!("daemon is not yet supported on this platform")
}

/// Send SIGTERM to `pid`, then poll briefly for it to exit (best-effort).
#[cfg(unix)]
fn terminate(pid: u32) -> Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use std::time::{Duration, Instant};

    let target = Pid::from_raw(pid as i32);
    // ESRCH here means it already exited between status + now — that's fine.
    if let Err(e) = kill(target, Signal::SIGTERM) {
        if e != nix::errno::Errno::ESRCH {
            return Err(anyhow::anyhow!("SIGTERM pid {pid}: {e}"));
        }
    }

    // Give the SIGTERM handler up to ~2s to unwind. We don't hard-fail if it's
    // still alive — `run_stop` removes the PID file regardless, and the HTTP
    // server (step 2) will add a SIGKILL escalation if graceful shutdown
    // proves unreliable.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !is_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[cfg(not(unix))]
fn terminate(_pid: u32) -> Result<()> {
    anyhow::bail!("daemon is not yet supported on this platform")
}

// ─────────────────────────────────────────────────────────────────────────
// `daemon __run` — the detached child's body. ASYNC since v3.3 step 2: it runs
// the shared HTTP surface ([`crate::server`]) on a tokio runtime until SIGTERM.
// ─────────────────────────────────────────────────────────────────────────

/// Hidden internal verb: the long-lived daemon process.
///
/// 1. Initialise `tracing` file logging (`~/.onebrain/run/daemon.log`).
/// 2. Write our own PID (the parent also wrote it; we re-assert it so a
///    directly-launched `__run` is still tracked).
/// 3. Build a tokio runtime and run the HTTP server until SIGTERM, then remove
///    the PID file + exit cleanly.
///
/// The server is the SAME one `onebrain serve` runs — only the shutdown trigger
/// differs (SIGTERM here vs Ctrl-C there). The daemon resolves its
/// [`crate::server::ServeConfig`] from:
/// - **vault_root** — the `--vault` arg passed by `daemon start` if present,
///   else `$ONEBRAIN_VAULT` — but ONLY if the chosen candidate names a real
///   vault (see [`resolve_daemon_vault`]). Otherwise `None`.
/// - **port** — an EPHEMERAL OS-assigned port ([`DAEMON_EPHEMERAL_PORT`] = `0`),
///   published (actual value) in the slot json; `$ONEBRAIN_DAEMON_PORT` overrides.
/// - **token** — freshly generated per process.
/// - **dist_dir** — `$ONEBRAIN_DIST` if set, else `None`. `$ONEBRAIN_DIST` is
///   an OVERRIDE (webui development / the plugin launcher pinning a dist);
///   with it unset the daemon serves the web UI EMBEDDED in the binary,
///   exactly like `serve` (see `server::static::serve_static` — a binary built
///   without bundled assets falls back to a token-bearing placeholder page).
///   The daemon is always webui-ready; there is no API-only mode.
///
/// VAULT RESOLUTION (fix A): because the detached child `chdir`s to `/`, walk-up
/// from cwd can't find the vault the user started from, so the daemon relies on
/// `$ONEBRAIN_VAULT` (exported by the launcher). That candidate is VALIDATED to
/// be a real OneBrain vault before it is trusted — a missing or non-vault path
/// resolves to `None`, NOT to a placeholder like `/`. With `None`, the vault
/// endpoints (config/tree/file) return 503; the static surface + token still
/// work so the daemon runs and reports cleanly while exposing no filesystem.
///
/// KNOWN LIMITATION: the concurrent-start guard lives in [`run_start`] (it locks
/// before spawning `__run`), so invoking `onebrain daemon __run` DIRECTLY while a
/// daemon is already running still overwrites `daemon.pid`/`daemon.json` with
/// this process's values, orphaning the existing daemon. `__run` is a hidden,
/// internal verb only `daemon start` (which holds the lock) is meant to spawn.
///
/// `vault` is the `--vault` arg `daemon start` threaded through; it takes
/// precedence over `$ONEBRAIN_VAULT` (see [`resolve_daemon_vault`]).
pub fn run_internal(vault: Option<&Path>) -> Result<()> {
    use crate::server::{self, resolve_token, ServeConfig};

    // Resolve the vault the daemon serves FIRST — the slot files are keyed on it.
    // The detached child has chdir'd to `/`, so walk-up from cwd is useless; rely
    // on the `--vault` arg (threaded by `daemon start`) or `$ONEBRAIN_VAULT`.
    //
    // SECURITY (fix A): we NEVER fall back to a placeholder root like `/`. A
    // `/` root would let `GET /api/vault/file?path=etc/passwd` serve
    // `/etc/passwd`. Instead we REQUIRE a real vault: the candidate dir must
    // actually be a vault (contain `onebrain.yml` or the legacy `vault.yml`). If
    // it isn't — or nothing is set — we bind `None`, and the vault handlers
    // return 503 (the static surface + token still work).
    let vault_root = resolve_daemon_vault(vault);
    // Canonical identity of the bound vault: stamped into the slot json AND used
    // to key the slot itself (`daemon-<hash>.*`). `None` → the vault-less
    // sentinel slot (`daemon-none.*`). `run_start` derives the SAME slot from the
    // same `--vault`, so the child writes where the parent pre-created.
    let vault_id = vault_root
        .as_deref()
        .and_then(daemon_client::canonical_vault_id);
    let slot = daemon_client::slot_paths_for_id(vault_id.as_deref())?;

    init_tracing(&slot.log)?;

    let pid = std::process::id();
    write_pid(&slot.pid, pid)?;

    // Optional pre-built webui dist, passed by the plugin launcher.
    let dist_dir = std::env::var_os("ONEBRAIN_DIST").map(PathBuf::from);
    // Honours $ONEBRAIN_TOKEN (≥32 chars, [A-Za-z0-9_-]) for a stable token
    // across restarts. A malformed pinned token is a hard error (see
    // `resolve_token`) rather than a silent swap for a random one.
    let token = resolve_token()?;

    // Port: an EPHEMERAL OS-assigned port by default (v3.4.13, #230) — two
    // per-vault daemons can't both bind a fixed port, so each takes `:0` and
    // publishes the ACTUAL bound port in its slot json (discovery/serve/mcp/
    // status all read the port from the slot, never assume 6789).
    // `$ONEBRAIN_DAEMON_PORT` overrides it (tests pin a value or `0`; a pinned
    // fixed value is a single-daemon convenience).
    let port = std::env::var("ONEBRAIN_DAEMON_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DAEMON_EPHEMERAL_PORT);

    // Daemon always binds localhost — a persistent boot-time engine should never
    // listen on a public interface implicitly. Remote access is tunnel-only
    // (docs/daemon.md § Remote access); `serve` under `$ONEBRAIN_BIND` is the
    // explicit, foreground-only non-loopback path (containers, #205).
    let mut cfg = ServeConfig::localhost(vault_root, port, token.clone(), dist_dir);
    // The daemon is the SOLE redb owner: hold the search engine for the process
    // lifetime so mcp + CLI search route through `/api/vault/search` /
    // `/api/internal/*` instead of each opening their own engine.
    cfg.hold_engine = true;

    tracing::info!(pid, "daemon __run started; bringing up HTTP surface");

    // Idle-shutdown TTL: after this long with no authenticated request, the
    // daemon exits (dropping the engine → releasing the redb lock). Configurable
    // via `$ONEBRAIN_DAEMON_IDLE_SECS`; default 30 min. `0` disables it (run
    // forever) — handy for a pinned always-on daemon.
    let idle_secs = resolve_idle_secs();

    let discovery_path = slot.json.clone();

    // Own a tokio runtime for the lifetime of the daemon. `enable_all` turns on
    // the I/O + time drivers the server + signal handling need.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for daemon")?;

    let discovery_for_bind = discovery_path.clone();
    let result = runtime.block_on(async move {
        // Build router + grab the shared state so the idle loop can read
        // `last_activity`.
        let (router, state) = server::build_router_with_state(cfg);

        // Compose the shutdown trigger: SIGTERM OR idle-timeout.
        let idle_state = state.clone();
        let shutdown = async move {
            let sigterm = sigterm_future();
            tokio::pin!(sigterm);
            let idle = idle_shutdown(idle_state, idle_secs);
            tokio::pin!(idle);
            tokio::select! {
                _ = &mut sigterm => tracing::info!("shutdown: SIGTERM"),
                _ = &mut idle => tracing::info!("shutdown: idle timeout"),
            }
        };

        // Publish discovery (`daemon.json`) with the ACTUAL bound port once the
        // listener is up. Written from the `on_bind` callback so a `0` (ephemeral)
        // port resolves to the real one clients must connect to.
        let on_bind = move |addr: std::net::SocketAddr| {
            let info = DaemonInfo {
                port: addr.port(),
                token,
                pid,
                version: env!("CARGO_PKG_VERSION").to_string(),
                vault: vault_id,
            };
            if let Err(e) = info.write(&discovery_for_bind) {
                tracing::warn!(error = %e, "failed to write slot discovery file");
            } else {
                tracing::info!(port = addr.port(), "published slot discovery file");
            }
        };

        server::run_server_from_router(router, addr_from(port), on_bind, shutdown).await
    });

    // Always clear this slot's PID + discovery files on the way out, even if the
    // server returned an error — stale files would block/mislead the next start.
    tracing::info!("daemon shutting down; removing slot PID + discovery files");
    remove_pid(&slot.pid)?;
    let _ = DaemonInfo::remove(&slot.json);
    tracing::info!("slot PID + discovery files removed; exit");
    result
}

/// Default daemon bind port: `0` = OS-assigned ephemeral (v3.4.13, #230). Two
/// per-vault daemons can't share a fixed port, so each binds `:0` and publishes
/// its ACTUAL port in its slot json. `$ONEBRAIN_DAEMON_PORT` overrides it.
const DAEMON_EPHEMERAL_PORT: u16 = 0;

/// Default idle-shutdown TTL: 30 minutes with no authenticated request.
const DEFAULT_IDLE_SECS: u64 = 30 * 60;

/// Resolve the idle-shutdown TTL: `$ONEBRAIN_DAEMON_IDLE_SECS` (a `0` disables
/// idle-shutdown), else [`DEFAULT_IDLE_SECS`]. Shared by the daemon body
/// (`run_internal`) and the `daemon status` dashboard so both report the same
/// resolution.
fn resolve_idle_secs() -> u64 {
    std::env::var("ONEBRAIN_DAEMON_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_IDLE_SECS)
}

/// The localhost socket address for `port` (the daemon always binds 127.0.0.1).
fn addr_from(port: u16) -> std::net::SocketAddr {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Resolve when the daemon has been idle for `idle_secs` (no authenticated
/// request in that window). `idle_secs == 0` disables idle-shutdown (this
/// future never resolves, so only SIGTERM stops the daemon).
///
/// Polls the shared `last_activity` marker once a minute (cheap, and a minute of
/// slack on a 30-minute TTL is irrelevant). Reading an atomic + comparing to now
/// avoids any per-request timer churn.
async fn idle_shutdown(state: std::sync::Arc<crate::server::AppState>, idle_secs: u64) {
    use std::sync::atomic::Ordering;
    let poll = std::time::Duration::from_secs(60);
    loop {
        tokio::time::sleep(poll).await;
        // Maintenance (MAJOR 3): the ledger's bounded prune runs HERE, on the
        // daemon's existing periodic loop, so the per-write path can stay a
        // throttled O(1) insert instead of an O(n) scan per delivery. Runs
        // every tick regardless of `idle_secs` (a never-idle daemon still needs
        // its ledger pruned).
        run_ledger_gc(&state);

        // Idle-shutdown check. `idle_secs == 0` disables shutdown (the daemon
        // runs until SIGTERM) but the maintenance sweep above still runs.
        if idle_secs != 0 {
            let last = state.last_activity.load(Ordering::Relaxed);
            let now = crate::server::now_epoch_secs();
            if should_idle_shutdown(last, now, idle_secs) {
                return;
            }
        }
    }
}

/// Prune ledger entries older than the TTL from the daemon-held token cache
/// (MAJOR 3). Best-effort: a GC error is logged and swallowed — a failed prune
/// must never take the daemon down or block the idle-shutdown check. No-op when
/// the daemon holds no token cache.
fn run_ledger_gc(state: &crate::server::AppState) {
    let Some(cache) = state.token_cache.as_ref() else {
        return;
    };
    let now = crate::server::now_epoch_secs() as i64;
    match cache.ledger().gc(now) {
        Ok(n) if n > 0 => tracing::debug!(pruned = n, "ledger GC pruned stale entries"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "ledger GC pass failed (non-fatal)"),
    }
}

/// Pure idle-shutdown predicate: `true` when `now` is at least `idle_secs` past
/// `last_activity`. `idle_secs == 0` disables (never fires). Extracted from the
/// async poll loop so the timing decision is unit-testable without a runtime;
/// `saturating_sub` guards a clock that stepped backwards (last > now → 0).
fn should_idle_shutdown(last_activity: u64, now: u64, idle_secs: u64) -> bool {
    if idle_secs == 0 {
        return false;
    }
    now.saturating_sub(last_activity) >= idle_secs
}

/// Resolve the vault the daemon should serve, or `None` when none is bound.
///
/// Candidate precedence: the `--vault` arg (`arg`, threaded from `daemon start`)
/// if present, else the `$ONEBRAIN_VAULT` env var (the back-compat fallback).
/// Passing the vault as an explicit argument avoids the parent having to mutate
/// its own process environment (`std::env::set_var`, unsound/deprecated).
///
/// Whichever candidate is chosen is then VALIDATED to be a REAL vault before it
/// is trusted (fix A): a directory only counts if it contains a config file
/// (`onebrain.yml`, or legacy `vault.yml`) at its root, exactly the check
/// `onebrain_core::find_vault_root` / `load_vault_config` rely on. We use
/// `find_config_file` (not a walk-up) because the candidate is meant to name the
/// vault root directly. No candidate, or a path that isn't a vault, yields
/// `None` — never a fallback like `/`.
fn resolve_daemon_vault(arg: Option<&Path>) -> Option<PathBuf> {
    let candidate = match arg {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(std::env::var_os("ONEBRAIN_VAULT")?),
    };
    // `find_config_file` returns `Some(path-to-config)` only when the dir really
    // holds a vault config. Map that to the vault ROOT (the candidate dir).
    if onebrain_core::find_config_file(&candidate).is_some() {
        Some(candidate)
    } else {
        tracing::warn!(
            vault = %candidate.display(),
            "daemon vault candidate is not a OneBrain vault (no onebrain.yml/vault.yml); \
             serving with no vault bound (vault endpoints return 503)"
        );
        None
    }
}

/// A future that resolves when SIGTERM is received — the daemon's graceful-
/// shutdown trigger, handed to `server::run_server_from_router`'s
/// `with_graceful_shutdown`.
///
/// Uses tokio's async signal handling (a self-pipe under the hood) rather than
/// the old `sigaction` + `AtomicBool` park loop: it composes cleanly as a
/// `Future` the server can race against its accept loop, and tokio installs the
/// handler in an async-signal-safe way.
#[cfg(unix)]
async fn sigterm_future() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            term.recv().await;
            tracing::info!("SIGTERM received; shutting down daemon");
        }
        Err(e) => {
            // Installing the handler failed — extraordinarily rare. Log and
            // resolve immediately so we don't serve un-stoppably; the parent's
            // `stop` SIGTERM would then just kill the process outright.
            tracing::error!(error = %e, "failed to install SIGTERM handler");
        }
    }
}

/// Non-Unix: no SIGTERM. The daemon is Unix-only for now, so this never runs in
/// production; it exists so the async server fn type-checks on Windows.
#[cfg(not(unix))]
async fn sigterm_future() {
    std::future::pending::<()>().await
}

/// Point `tracing` at `stderr`, which the parent already redirected to the
/// daemon log file when it spawned us (see [`spawn_detached_run`]). Writing to
/// `stderr` instead of re-opening `daemon.log` keeps a SINGLE file descriptor
/// on the log, avoiding two independent append cursors racing on the same file.
/// A real subscriber still gives us structured, timestamped, level-filtered
/// lines instead of bare `println!`.
///
/// We keep the `create_dir_all` for the run dir — it isn't needed for the log
/// anymore, but the PID file written right after lives in the same directory.
///
/// Log level honours `RUST_LOG` (via `EnvFilter`), defaulting to `info`.
fn init_tracing(log_path: &Path) -> Result<()> {
    use tracing_subscriber::EnvFilter;

    if let Some(parent) = log_path.parent() {
        ensure_private_run_dir(parent)?;
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // `try_init` instead of `init`: a global subscriber can only be set once per
    // process. Ignoring an "already set" error keeps a future foreground/
    // in-process run (or an integration test that ran `__run` earlier in the
    // same process) from panicking. Our detached child is a fresh process, so
    // it always wins the race in production.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false) // log file, not a terminal
        .try_init();
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Tests — pure status/PID logic only. Injected `is_alive` + `tempdir()` PID
// paths mean no real process is ever spawned. Mirrors `session_init.rs`.
// ─────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn pid_file(dir: &Path) -> PathBuf {
        dir.join("daemon.pid")
    }

    #[test]
    fn no_pid_file_reports_not_running() {
        let dir = tempdir().unwrap();
        // Probe would say "alive" for anything — but with no file we never call
        // it, so the result must still be "not running".
        let status = compute_status(&pid_file(dir.path()), |_| true);
        assert_eq!(
            status,
            DaemonStatusData {
                running: false,
                pid: None,
                ..Default::default()
            }
        );
    }

    #[test]
    fn live_pid_reports_running_with_pid() {
        let dir = tempdir().unwrap();
        let path = pid_file(dir.path());
        write_pid(&path, 4242).unwrap();
        // Inject a probe that only treats 4242 as alive.
        let status = compute_status(&path, |pid| pid == 4242);
        assert_eq!(
            status,
            DaemonStatusData {
                running: true,
                pid: Some(4242),
                ..Default::default()
            }
        );
    }

    #[test]
    fn stale_pid_reports_not_running() {
        // PID file exists but the recorded process is dead → not running.
        let dir = tempdir().unwrap();
        let path = pid_file(dir.path());
        write_pid(&path, 9999).unwrap();
        let status = compute_status(&path, |_| false); // nothing alive
        assert_eq!(
            status,
            DaemonStatusData {
                running: false,
                pid: None,
                ..Default::default()
            }
        );
    }

    #[test]
    fn garbage_pid_file_reports_not_running() {
        let dir = tempdir().unwrap();
        let path = pid_file(dir.path());
        fs::write(&path, "not-a-number\n").unwrap();
        // Probe says "alive" for everything; a garbage file must still parse to
        // None and short-circuit to not-running before the probe is consulted.
        let status = compute_status(&path, |_| true);
        assert_eq!(
            status,
            DaemonStatusData {
                running: false,
                pid: None,
                ..Default::default()
            }
        );
    }

    #[test]
    fn empty_pid_file_reports_not_running() {
        // Edge of "garbage": a zero-byte / whitespace-only file (e.g. a crash
        // mid-write) parses to None, not a panic.
        let dir = tempdir().unwrap();
        let path = pid_file(dir.path());
        fs::write(&path, "   \n").unwrap();
        let status = compute_status(&path, |_| true);
        assert!(!status.running);
        assert!(status.pid.is_none());
    }

    #[test]
    fn write_then_read_roundtrips_pid() {
        let dir = tempdir().unwrap();
        let path = pid_file(dir.path());
        write_pid(&path, 12345).unwrap();
        assert_eq!(read_pid(&path), Some(12345));
    }

    #[test]
    fn write_pid_creates_missing_parent_dir() {
        // The run dir may not exist on a fresh machine — write_pid must mkdir -p.
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("daemon.pid");
        write_pid(&nested, 7).unwrap();
        assert_eq!(read_pid(&nested), Some(7));
    }

    #[test]
    fn remove_pid_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = pid_file(dir.path());
        // Removing a non-existent file is a no-op success.
        remove_pid(&path).unwrap();
        write_pid(&path, 1).unwrap();
        remove_pid(&path).unwrap();
        assert!(read_pid(&path).is_none());
    }

    // ─────────────────────────────────────────────────────────────────────
    // Concurrent-start guard.
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn start_lock_first_caller_acquires() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        // No lock present → the first caller creates it and wins. The probe
        // (unused on a fresh acquire) treats everything as live.
        match acquire_start_lock(&lock, |_| true).unwrap() {
            StartLock::Acquired(_g) => {}
            StartLock::Contended => panic!("first caller must acquire the lock"),
        }
    }

    #[test]
    fn start_lock_second_concurrent_caller_is_contended() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        // First caller holds the lock (guard kept alive). The lock records the
        // creating (test) process's PID; probing it as LIVE → the second caller
        // must back off as contended, NOT reclaim + spawn.
        let _first = match acquire_start_lock(&lock, |_| true).unwrap() {
            StartLock::Acquired(g) => g,
            StartLock::Contended => panic!("first caller must acquire"),
        };
        match acquire_start_lock(&lock, |_| true).unwrap() {
            StartLock::Contended => {}
            StartLock::Acquired(_) => panic!("second concurrent caller must be contended"),
        }
    }

    #[test]
    fn start_lock_stale_lock_is_reclaimed_when_creator_dead() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        // A crashed `daemon start`: the lock records a PID whose process is gone.
        fs::write(&lock, "999999").unwrap();
        // Probe reports that PID as DEAD → the guard reclaims the stale lock.
        match acquire_start_lock(&lock, |pid| pid != 999999).unwrap() {
            StartLock::Acquired(_g) => {}
            StartLock::Contended => panic!("a lock whose creator is dead must be reclaimed"),
        }
    }

    #[test]
    fn start_lock_live_creator_is_respected_not_reclaimed() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        // A lock whose recorded PID is a LIVE concurrent starter → contended
        // (this is the case that regressed: it must NOT be reclaimed even though
        // no daemon PID file exists yet).
        fs::write(&lock, "424242").unwrap();
        match acquire_start_lock(&lock, |pid| pid == 424242).unwrap() {
            StartLock::Contended => {}
            StartLock::Acquired(_) => panic!("a live creator's lock must not be reclaimed"),
        }
    }

    #[test]
    fn start_lock_unreadable_pid_is_treated_as_live() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        // An empty lock (a concurrent creator hasn't written its PID yet) →
        // Unknown → contended (never reclaim on a guess).
        fs::write(&lock, "").unwrap();
        match acquire_start_lock(&lock, |_| false).unwrap() {
            StartLock::Contended => {}
            StartLock::Acquired(_) => panic!("an unreadable lock PID must not be reclaimed"),
        }
    }

    #[test]
    fn lock_owner_state_classifies_live_dead_unknown() {
        let dir = tempdir().unwrap();
        let live = dir.path().join("live.lock");
        fs::write(&live, "100").unwrap();
        assert_eq!(lock_owner_state(&live, &|_| true), LockOwner::Live);

        let dead = dir.path().join("dead.lock");
        fs::write(&dead, "100").unwrap();
        assert_eq!(lock_owner_state(&dead, &|_| false), LockOwner::Dead);

        let empty = dir.path().join("empty.lock");
        fs::write(&empty, "  \n").unwrap();
        assert_eq!(lock_owner_state(&empty, &|_| true), LockOwner::Unknown);
    }

    #[test]
    fn start_guard_drop_removes_lock_file() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        {
            let _g = match acquire_start_lock(&lock, |_| true).unwrap() {
                StartLock::Acquired(g) => g,
                StartLock::Contended => panic!("must acquire"),
            };
            assert!(lock.exists(), "lock file exists while guard is held");
        }
        // Dropping the guard clears the lock so the next start isn't blocked.
        assert!(!lock.exists(), "lock file removed after guard drop");
    }

    // ─────────────────────────────────────────────────────────────────────
    // Idle-shutdown predicate (pure).
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn idle_shutdown_fires_after_ttl_elapses() {
        // last=now-120, idle=60 → 120 >= 60 → should exit.
        assert!(should_idle_shutdown(1_000, 1_120, 60));
    }

    #[test]
    fn idle_shutdown_holds_within_ttl() {
        // A recent request (30s ago) with a 60s TTL → stay up.
        assert!(!should_idle_shutdown(1_090, 1_120, 60));
        // Exactly at the boundary counts as elapsed (>=).
        assert!(should_idle_shutdown(1_060, 1_120, 60));
    }

    #[test]
    fn idle_shutdown_zero_ttl_never_fires() {
        // idle=0 disables idle-shutdown regardless of how long ago activity was.
        assert!(!should_idle_shutdown(0, u64::MAX, 0));
    }

    #[test]
    fn idle_shutdown_survives_backward_clock_step() {
        // If the clock stepped back so last > now, saturating_sub yields 0 → no
        // spurious shutdown.
        assert!(!should_idle_shutdown(2_000, 1_000, 60));
    }

    // ─────────────────────────────────────────────────────────────────────
    // Small pure/near-pure helpers.
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn addr_from_is_localhost_with_port() {
        let a = addr_from(6789);
        assert_eq!(a.ip().to_string(), "127.0.0.1");
        assert_eq!(a.port(), 6789);
    }

    #[test]
    fn read_lock_pid_parses_or_rejects() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("l.lock");
        fs::write(&p, "12345\n").unwrap();
        assert_eq!(read_lock_pid(&p), Some(12345));
        fs::write(&p, "0").unwrap();
        assert_eq!(read_lock_pid(&p), None, "pid 0 is rejected");
        fs::write(&p, "not-a-pid").unwrap();
        assert_eq!(read_lock_pid(&p), None);
        fs::write(&p, "").unwrap();
        assert_eq!(read_lock_pid(&p), None);
    }

    #[cfg(unix)]
    #[test]
    fn pid_exists_true_for_self_false_for_impossible() {
        assert!(pid_exists(std::process::id()), "our own pid must exist");
        // A pid far above any real one → almost certainly not a live process.
        assert!(!pid_exists(0x7FFF_FFF0));
    }

    #[cfg(unix)]
    #[test]
    fn wait_until_ready_bails_early_when_child_is_dead() {
        // A pid that doesn't exist → wait_until_ready returns fast (no full
        // timeout) via the `!bare_pid_or_session_alive` early-out, reporting
        // `Died` so the caller (run_start) knows NOT to claim success (#278).
        let dir = tempdir().unwrap();
        let never = dir.path().join("never.json");
        let start = std::time::Instant::now();
        let outcome = wait_until_ready(0x7FFF_FFF0, &never, std::time::Duration::from_secs(30));
        assert_eq!(outcome, ReadyOutcome::Died);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "should bail early for a dead child, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn wait_until_ready_times_out_when_alive_but_never_confirmed() {
        // Our OWN pid stays alive (`pid_exists` true) for the whole call — it's
        // not a session leader in the test harness, so `is_alive` never fires —
        // and the discovery file never appears → TimedOut (best-effort; the
        // caller still reports `started: true` for this outcome, unlike
        // `Died`). The `Ready` outcome (a REAL setsid'd session leader
        // publishing its discovery file) is exercised end-to-end by
        // `daemon_lifecycle_start_status_stop` below, which drives the actual
        // `daemon start` → success path against a genuine detached daemon.
        let dir = tempdir().unwrap();
        let never = dir.path().join("never.json");
        let outcome = wait_until_ready(
            std::process::id(),
            &never,
            std::time::Duration::from_millis(50),
        );
        assert_eq!(outcome, ReadyOutcome::TimedOut);
    }

    #[test]
    fn resolve_daemon_vault_valid_and_invalid() {
        // Each `set_var` guard is scoped (dropped) before the next — the env
        // lock is NON-REENTRANT, so holding two guards at once would deadlock.
        let notvault = tempdir().unwrap();
        {
            let _e = crate::test_env::set_var("ONEBRAIN_VAULT", notvault.path());
            // A dir that isn't a vault → None (never a fallback like `/`).
            assert!(resolve_daemon_vault(None).is_none());
        }

        let vault = tempdir().unwrap();
        std::fs::write(vault.path().join("onebrain.yml"), "search: {}\n").unwrap();
        {
            let _e = crate::test_env::set_var("ONEBRAIN_VAULT", vault.path());
            // Valid vault (has onebrain.yml) via the env FALLBACK → Some(that dir).
            assert_eq!(resolve_daemon_vault(None).as_deref(), Some(vault.path()));
        }
    }

    #[test]
    fn resolve_daemon_vault_arg_takes_precedence_over_env() {
        // The `--vault` arg wins over `$ONEBRAIN_VAULT`, and a non-vault arg
        // yields None even when the env names a real vault (no silent fallback).
        let arg_vault = tempdir().unwrap();
        std::fs::write(arg_vault.path().join("onebrain.yml"), "search: {}\n").unwrap();
        let env_vault = tempdir().unwrap();
        std::fs::write(env_vault.path().join("onebrain.yml"), "search: {}\n").unwrap();

        let _e = crate::test_env::set_var("ONEBRAIN_VAULT", env_vault.path());
        // Arg present + a real vault → arg wins over env.
        assert_eq!(
            resolve_daemon_vault(Some(arg_vault.path())).as_deref(),
            Some(arg_vault.path()),
            "--vault must take precedence over $ONEBRAIN_VAULT"
        );

        // Arg present but NOT a vault → None, without falling back to the env.
        let notvault = tempdir().unwrap();
        assert!(
            resolve_daemon_vault(Some(notvault.path())).is_none(),
            "a non-vault --vault must not silently fall back to the env"
        );
    }

    #[test]
    fn status_text_running_includes_pid() {
        // Bare running state (no daemon.json / all probes failed): the
        // dashboard degrades to just the Process section — never an error.
        let env = Envelope::ok(
            "daemon.status",
            None,
            DaemonStatusData {
                running: true,
                pid: Some(555),
                ..Default::default()
            },
        );
        let s = render_status_text(&env);
        assert!(s.contains("🟢  Process"), "got: {s}");
        assert!(s.contains("Running"), "got: {s}");
        assert!(s.contains("pid 555"), "got: {s}");
        // No probe data → no Bind/Webui/Engine/Models sections.
        for absent in ["🔌", "🌐", "🧠", "🎯"] {
            assert!(!s.contains(absent), "unexpected section {absent}: {s}");
        }
    }

    /// A fully-probed running daemon — every dashboard field present.
    fn rich_status_fixture() -> DaemonStatusData {
        DaemonStatusData {
            running: true,
            pid: Some(555),
            version: Some("3.4.8".to_string()),
            started: Some(1_760_000_000),
            idle_ttl_secs: Some(1800),
            port: Some(6789),
            vault: Some("/Users/keng/ob-1".to_string()),
            webui_source: Some("embedded".to_string()),
            url: Some("http://127.0.0.1:6789/?token=sekret".to_string()),
            engine_held: Some(true),
            doc_count: Some(812),
            pending_total: Some(3),
            last_indexed: Some(1_760_000_100),
            embed_model: Some("multilingual-e5-small".to_string()),
            reranker_model: Some("onebrain-rerank-v1".to_string()),
            reranker_ready: Some(true),
            reranker_downloaded: Some(true),
        }
    }

    #[test]
    fn status_text_rich_dashboard_has_grouped_sections() {
        let env = Envelope::ok("daemon.status", None, rich_status_fixture());
        let s = render_status_text(&env);
        // The five grouped sections, in the house convention.
        for section in [
            "🟢  Process",
            "🔌  Bind",
            "🌐  Webui",
            "🧠  Engine",
            "🎯  Models",
        ] {
            assert!(s.contains(section), "missing section {section:?}: {s}");
        }
        // Process: pid · version · started · idle TTL.
        assert!(s.contains("pid 555"), "{s}");
        assert!(s.contains("3.4.8"), "{s}");
        assert!(s.contains("    Started       "), "{s}");
        assert!(s.contains("    Idle TTL      30 min"), "{s}");
        // Bind: port · vault · webui source.
        assert!(s.contains("    Port          6789"), "{s}");
        assert!(s.contains("    Vault         /Users/keng/ob-1"), "{s}");
        assert!(s.contains("    Web UI        embedded"), "{s}");
        // Webui: the clickable token-bearing URL.
        assert!(
            s.contains("    URL           http://127.0.0.1:6789/?token=sekret"),
            "{s}"
        );
        // Engine: held · docs · pending · last indexed.
        assert!(s.contains("    Held          ✅  yes"), "{s}");
        assert!(s.contains("    Docs          812"), "{s}");
        assert!(s.contains("    Pending       3"), "{s}");
        assert!(s.contains("    Last indexed  "), "{s}");
        // Models: embed + reranker name/readiness.
        assert!(s.contains("    Embed         multilingual-e5-small"), "{s}");
        assert!(
            s.contains("    Reranker      onebrain-rerank-v1 (ready)"),
            "{s}"
        );
    }

    #[test]
    fn status_text_engine_probe_failure_omits_engine_and_models() {
        // daemon.json read fine (Bind/Webui present) but both HTTP probes
        // failed → Engine + Models sections are absent, output still renders.
        let data = DaemonStatusData {
            engine_held: None,
            doc_count: None,
            pending_total: None,
            last_indexed: None,
            embed_model: None,
            reranker_model: None,
            reranker_ready: None,
            reranker_downloaded: None,
            webui_source: None,
            ..rich_status_fixture()
        };
        let env = Envelope::ok("daemon.status", None, data);
        let s = render_status_text(&env);
        assert!(s.contains("🔌  Bind"), "{s}");
        assert!(s.contains("🌐  Webui"), "{s}");
        assert!(!s.contains("🧠"), "engine section must be omitted: {s}");
        assert!(!s.contains("🎯"), "models section must be omitted: {s}");
    }

    #[test]
    fn status_text_vaultless_daemon_says_none_bound() {
        let data = DaemonStatusData {
            vault: None,
            ..rich_status_fixture()
        };
        let env = Envelope::ok("daemon.status", None, data);
        let s = render_status_text(&env);
        assert!(
            s.contains("    Vault         none bound (vault endpoints 503)"),
            "{s}"
        );
    }

    #[test]
    fn format_ttl_humanizes() {
        assert_eq!(format_ttl(0), "disabled (runs until stopped)");
        assert_eq!(format_ttl(1800), "30 min");
        assert_eq!(format_ttl(3600), "1 h");
        assert_eq!(format_ttl(90), "90 s");
    }

    #[test]
    fn enrich_status_fills_from_info_and_probes() {
        let info = crate::commands::daemon_client::DaemonInfo {
            port: 7001,
            token: "tok-abc".to_string(),
            pid: 42,
            version: "3.4.8".to_string(),
            vault: Some("/v".to_string()),
        };
        let health = serde_json::json!({
            "ok": true, "engine_held": true, "dist_dir": null, "embedded_ui": true
        });
        let internal = serde_json::json!({
            "doc_count": 10, "pending_total": 2, "last_indexed": 123,
            "embed_model": "e5", "reranker_model": "rr",
            "reranker_ready": false, "reranker_downloaded": true
        });
        let mut data = DaemonStatusData {
            running: true,
            pid: Some(42),
            ..Default::default()
        };
        enrich_status(
            &mut data,
            &info,
            Some(99),
            1800,
            Some(&health),
            Some(&internal),
        );
        assert_eq!(data.version.as_deref(), Some("3.4.8"));
        assert_eq!(data.port, Some(7001));
        assert_eq!(data.vault.as_deref(), Some("/v"));
        assert_eq!(
            data.url.as_deref(),
            Some("http://127.0.0.1:7001/?token=tok-abc")
        );
        assert_eq!(data.started, Some(99));
        assert_eq!(data.idle_ttl_secs, Some(1800));
        assert_eq!(data.engine_held, Some(true));
        // dist_dir null → embedded.
        assert_eq!(data.webui_source.as_deref(), Some("embedded"));
        assert_eq!(data.doc_count, Some(10));
        assert_eq!(data.pending_total, Some(2));
        assert_eq!(data.last_indexed, Some(123));
        assert_eq!(data.embed_model.as_deref(), Some("e5"));
        assert_eq!(data.reranker_model.as_deref(), Some("rr"));
        assert_eq!(data.reranker_ready, Some(false));
        assert_eq!(data.reranker_downloaded, Some(true));
    }

    #[test]
    fn enrich_status_probe_failures_leave_fields_absent() {
        let info = crate::commands::daemon_client::DaemonInfo {
            port: 7001,
            token: "tok".to_string(),
            pid: 42,
            version: "3.4.8".to_string(),
            vault: None,
        };
        let mut data = DaemonStatusData {
            running: true,
            pid: Some(42),
            ..Default::default()
        };
        // Both probes failed → daemon.json-derived fields only.
        enrich_status(&mut data, &info, None, 0, None, None);
        assert_eq!(data.port, Some(7001));
        assert!(data.engine_held.is_none());
        assert!(data.webui_source.is_none());
        assert!(data.doc_count.is_none());
        assert!(data.embed_model.is_none());
        assert!(data.reranker_model.is_none());
    }

    #[test]
    fn enrich_status_dist_dir_string_is_the_override_path() {
        let info = crate::commands::daemon_client::DaemonInfo {
            port: 1,
            token: "t".to_string(),
            pid: 1,
            version: "3.4.8".to_string(),
            vault: None,
        };
        let mut data = DaemonStatusData::default();
        let health = serde_json::json!({ "ok": true, "dist_dir": "/opt/webui-dist" });
        enrich_status(&mut data, &info, None, 0, Some(&health), None);
        assert_eq!(data.webui_source.as_deref(), Some("/opt/webui-dist"));

        // No override + no bundled assets (a from-source build) → the honest
        // "placeholder", not "embedded".
        let mut data = DaemonStatusData::default();
        let bare = serde_json::json!({ "ok": true, "dist_dir": null, "embedded_ui": false });
        enrich_status(&mut data, &info, None, 0, Some(&bare), None);
        assert_eq!(data.webui_source.as_deref(), Some("placeholder"));

        // No override + a missing embedded_ui flag (version-skewed daemon on
        // the null arm) → default "embedded" (the release-binary truth).
        let mut data = DaemonStatusData::default();
        let skewed = serde_json::json!({ "ok": true, "dist_dir": null });
        enrich_status(&mut data, &info, None, 0, Some(&skewed), None);
        assert_eq!(data.webui_source.as_deref(), Some("embedded"));

        // A pre-3.4.8 daemon that doesn't report dist_dir at all → unknown.
        let mut data = DaemonStatusData::default();
        let old_health = serde_json::json!({ "ok": true, "engine_held": false });
        enrich_status(&mut data, &info, None, 0, Some(&old_health), None);
        assert!(data.webui_source.is_none());
    }

    #[test]
    fn status_text_placeholder_webui_is_spelled_out() {
        let data = DaemonStatusData {
            webui_source: Some("placeholder".to_string()),
            ..rich_status_fixture()
        };
        let env = Envelope::ok("daemon.status", None, data);
        let s = render_status_text(&env);
        assert!(
            s.contains("    Web UI        placeholder page (no bundled web UI in this binary)"),
            "{s}"
        );
    }

    #[test]
    fn status_text_not_running_has_no_pid() {
        let env = Envelope::ok(
            "daemon.status",
            None,
            DaemonStatusData {
                running: false,
                pid: None,
                ..Default::default()
            },
        );
        assert_eq!(render_status_text(&env), "daemon not running");
    }

    #[test]
    fn start_text_distinguishes_fresh_from_already_running() {
        let fresh = Envelope::ok(
            "daemon.start",
            None,
            DaemonStartData {
                started: true,
                already_running: false,
                pid: 10,
            },
        );
        assert!(render_start_text(&fresh).contains("started"));

        let dupe = Envelope::ok(
            "daemon.start",
            None,
            DaemonStartData {
                started: false,
                already_running: true,
                pid: 10,
            },
        );
        assert!(render_start_text(&dupe).contains("already running"));
    }

    #[test]
    fn stop_text_distinguishes_stopped_from_noop() {
        let stopped = Envelope::ok(
            "daemon.stop",
            None,
            DaemonStopData {
                stopped: true,
                pid: Some(77),
            },
        );
        let s = render_stop_text(&stopped);
        assert!(s.contains("stopped"), "got: {s}");
        assert!(s.contains("77"), "got: {s}");

        let noop = Envelope::ok(
            "daemon.stop",
            None,
            DaemonStopData {
                stopped: false,
                pid: None,
            },
        );
        assert_eq!(render_stop_text(&noop), "daemon not running");
    }

    #[test]
    fn status_data_serializes_to_expected_json() {
        // Lock the MINIMAL JSON shape: { "running": bool, "pid": N|null } —
        // every dashboard field is skip_serializing_if, so a not-running (or
        // probe-less) status keeps the exact pre-3.4.8 shape.
        let running = serde_json::to_value(DaemonStatusData {
            running: true,
            pid: Some(3),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(running["running"], true);
        assert_eq!(running["pid"], 3);

        let stopped = serde_json::to_value(DaemonStatusData {
            running: false,
            pid: None,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(stopped["running"], false);
        assert!(stopped["pid"].is_null());
        // Absent dashboard fields are OMITTED, not null.
        assert_eq!(
            stopped.as_object().unwrap().len(),
            2,
            "not-running JSON must stay minimal: {stopped}"
        );

        // And a rich status serializes every dashboard field.
        let rich = serde_json::to_value(rich_status_fixture()).unwrap();
        assert_eq!(rich["url"], "http://127.0.0.1:6789/?token=sekret");
        assert_eq!(rich["port"], 6789);
        assert_eq!(rich["engine_held"], true);
        assert_eq!(rich["doc_count"], 812);
        assert_eq!(rich["embed_model"], "multilingual-e5-small");
        assert_eq!(rich["reranker_model"], "onebrain-rerank-v1");
        assert_eq!(rich["webui_source"], "embedded");
        assert_eq!(rich["idle_ttl_secs"], 1800);
    }

    #[test]
    fn status_list_text_empty_one_and_many() {
        // Empty list → the plain "not running" line.
        let empty = Envelope::ok(
            "daemon.status",
            None,
            DaemonStatusListData { daemons: vec![] },
        );
        assert_eq!(render_status_list_text(&empty), "daemon not running");

        // One daemon → a single grouped Process block, no count header.
        let one = Envelope::ok(
            "daemon.status",
            None,
            DaemonStatusListData {
                daemons: vec![DaemonStatusData {
                    running: true,
                    pid: Some(11),
                    ..Default::default()
                }],
            },
        );
        let s = render_status_list_text(&one);
        assert!(s.contains("pid 11"), "{s}");
        assert!(
            !s.contains("daemons running"),
            "no count header for one: {s}"
        );

        // Two daemons → a count header + both pids, in one report.
        let two = Envelope::ok(
            "daemon.status",
            None,
            DaemonStatusListData {
                daemons: vec![
                    DaemonStatusData {
                        running: true,
                        pid: Some(11),
                        port: Some(40001),
                        ..Default::default()
                    },
                    DaemonStatusData {
                        running: true,
                        pid: Some(22),
                        port: Some(40002),
                        ..Default::default()
                    },
                ],
            },
        );
        let s = render_status_list_text(&two);
        assert!(s.contains("2 daemons running"), "{s}");
        assert!(s.contains("pid 11") && s.contains("pid 22"), "{s}");
        assert!(s.contains("40001") && s.contains("40002"), "{s}");
    }

    #[test]
    fn stop_all_text_pluralizes() {
        let env = Envelope::ok(
            "daemon.stop",
            None,
            DaemonStopData {
                stopped: false,
                pid: None,
            },
        );
        assert_eq!(render_stop_all_text(&env, 0), "no daemons running");
        assert_eq!(render_stop_all_text(&env, 1), "stopped 1 daemon");
        assert_eq!(render_stop_all_text(&env, 3), "stopped 3 daemons");
    }

    #[cfg(unix)]
    #[test]
    fn stop_slot_clears_files_and_reports_not_running_when_absent() {
        // stop_slot on an absent slot is a harmless no-op that still clears any
        // stale files and reports (false, None).
        let dir = tempdir().unwrap();
        let slot = SlotPaths {
            json: dir.path().join("daemon-x.json"),
            pid: dir.path().join("daemon-x.pid"),
            lock: dir.path().join("daemon-x.lock"),
            log: dir.path().join("daemon-x.log"),
        };
        // Plant stale files with no live daemon.
        fs::write(&slot.json, "{}").unwrap();
        fs::write(&slot.lock, "999999").unwrap();
        let (stopped, pid) = stop_slot(&slot).unwrap();
        assert!(!stopped);
        assert!(pid.is_none());
        assert!(!slot.json.exists(), "stale json cleared");
        assert!(!slot.lock.exists(), "stale lock cleared");
    }

    /// RAII teardown for the real-daemon integration tests: runs
    /// `onebrain daemon stop` (with the test's own HOME/env) on drop, so a
    /// FAILED assertion between `daemon start` and the test's own `stop` never
    /// leaks a detached daemon — with `ONEBRAIN_DAEMON_IDLE_SECS=0` a leaked
    /// daemon runs forever. Two gaps remain by nature: an external SIGKILL of
    /// the test runner (the daemon is setsid-detached by design), and a
    /// `panic = "abort"` test profile (e.g. `cargo test --release` with abort
    /// panics), where no unwinding happens so Drop never runs.
    #[cfg(unix)]
    struct StopDaemonOnDrop {
        bin: PathBuf,
        envs: Vec<(String, String)>,
    }
    #[cfg(unix)]
    impl Drop for StopDaemonOnDrop {
        fn drop(&mut self) {
            let mut cmd = std::process::Command::new(&self.bin);
            for (k, v) in &self.envs {
                cmd.env(k, v);
            }
            // `--all`: stop EVERY per-vault slot under this test's HOME (v3.4.13).
            // A plain `stop` would target only the cwd's slot, missing a daemon
            // bound to a specific test vault. Best-effort; a double stop is a
            // no-op.
            let _ = cmd.args(["daemon", "stop", "--all"]).output();
        }
    }

    /// Compute a slot runtime-file path under an EXPLICIT test HOME. The
    /// integration tests set `HOME` on the CHILD process, not on this test
    /// process, so production's `slot_paths_for_id`/`run_dir` (which read THIS
    /// process's `$HOME`) can't be used. Mirrors production slot naming:
    /// `daemon-<hash>` for a vault (hash of the canonical path), `daemon-none`
    /// for vault-less.
    #[cfg(unix)]
    fn slot_file_under_home(home: &Path, vault: Option<&Path>, ext: &str) -> PathBuf {
        let stem = match vault.and_then(daemon_client::canonical_vault_id) {
            Some(id) => format!(
                "daemon-{}",
                onebrain_search::engine::short_path_hash(Path::new(&id))
            ),
            None => daemon_client::VAULTLESS_SLOT_STEM.to_string(),
        };
        home.join(".onebrain")
            .join("run")
            .join(format!("{stem}.{ext}"))
    }

    // ─────────────────────────────────────────────────────────────────────
    // Lifecycle integration test — drives the REAL `onebrain` binary through
    // status → start → status → stop → status. Unlike the unit tests above,
    // this exercises the actual spawn/setsid/SIGTERM path (`spawn_detached_run`
    // + `terminate` + the `__run` body) that the dependency-injected core
    // skips.
    //
    // `HOME` is pointed at a fresh `tempdir()` so `run_dir()` (which keys off
    // `dirs::home_dir()`) resolves under the temp dir — this NEVER touches the
    // user's real `~/.onebrain`. Unix-only because the daemon is Unix-only.
    // ─────────────────────────────────────────────────────────────────────
    #[cfg(unix)]
    #[test]
    fn daemon_lifecycle_start_status_stop() {
        use assert_cmd::Command;
        use std::time::{Duration, Instant};

        let home = tempdir().unwrap();
        let _teardown = StopDaemonOnDrop {
            bin: assert_cmd::cargo::cargo_bin("onebrain"),
            envs: vec![
                ("HOME".into(), home.path().display().to_string()),
                ("ONEBRAIN_DAEMON_PORT".into(), "0".into()),
            ],
        };

        // Small helper: run `onebrain daemon <verb>` with HOME overridden,
        // returning combined stdout as a String. We assert success separately.
        //
        // `ONEBRAIN_DAEMON_PORT=0` makes the detached `__run` bind an
        // OS-assigned ephemeral port instead of the fixed default (6789), so
        // this test never collides with a real running daemon or a parallel
        // test on the same machine. (We exercise PID-file lifecycle here, not
        // HTTP — the curl-the-port smoke test is the manual verify step.)
        let run = |verb: &str| -> String {
            let out = Command::cargo_bin("onebrain")
                .unwrap()
                .env("HOME", home.path())
                .env("ONEBRAIN_DAEMON_PORT", "0")
                .args(["daemon", verb])
                .output()
                .expect("spawn onebrain binary");
            assert!(
                out.status.success(),
                "`daemon {verb}` exited non-zero: {:?}",
                out
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        // 1. Before anything: not running.
        assert!(
            run("status").contains("not running"),
            "expected clean slate to report not running"
        );

        // 2. Start: spawns the detached `__run` child.
        let start_out = run("start");
        assert!(
            start_out.contains("started"),
            "expected start to report started, got: {start_out}"
        );

        // 3. Poll status until it reports running (the child needs a moment to
        //    install its session + re-assert the PID file). Bail after ~3s.
        let deadline = Instant::now() + Duration::from_secs(3);
        let last = loop {
            let status = run("status");
            // Rich dashboard (v3.4.8): the running state renders as a grouped
            // `Process` section with a `Running  yes (pid N)` row.
            if status.contains("Running") && status.contains("pid") {
                break status;
            }
            if Instant::now() >= deadline {
                // Make sure we don't leak the daemon before failing.
                let _ = run("stop");
                panic!("daemon never reported running; last status: {status}");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            last.contains("Running") && last.contains("pid"),
            "running status should carry a pid, got: {last}"
        );

        // 4. Stop: SIGTERM the child, remove the PID file.
        let stop_out = run("stop");
        assert!(
            stop_out.contains("stopped"),
            "expected stop to report stopped, got: {stop_out}"
        );

        // 5. After stop: back to not running.
        assert!(
            run("status").contains("not running"),
            "expected post-stop status to report not running"
        );
    }

    /// #278 daemon-startup audit: `daemon start` must NOT report "daemon
    /// started" when the detached `__run` child dies before it finishes
    /// binding its HTTP listener. Pins `ONEBRAIN_DAEMON_PORT` to a port we
    /// occupy ourselves (rather than the usual `0` ephemeral port), so the
    /// child's `TcpListener::bind` fails immediately, the child process exits,
    /// and `wait_until_ready` observes `ReadyOutcome::Died` — `run_start` must
    /// then bail with an error instead of emitting the success envelope.
    #[cfg(unix)]
    #[test]
    fn daemon_start_bind_failure_reports_error_not_started() {
        use assert_cmd::Command;
        use std::net::TcpListener;

        let home = tempdir().unwrap();
        let _teardown = StopDaemonOnDrop {
            bin: assert_cmd::cargo::cargo_bin("onebrain"),
            envs: vec![("HOME".into(), home.path().display().to_string())],
        };

        // Occupy a port ourselves so the detached child's bind fails. Held
        // open for the whole `daemon start` call below.
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = occupied.local_addr().unwrap().port();

        let out = Command::cargo_bin("onebrain")
            .unwrap()
            .env("HOME", home.path())
            .env("ONEBRAIN_DAEMON_PORT", port.to_string())
            .args(["daemon", "start"])
            .output()
            .expect("spawn onebrain binary");

        drop(occupied);

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "daemon start against an occupied port must fail; stdout={stdout} stderr={stderr}"
        );
        assert!(
            !stdout.contains("daemon started"),
            "must not report success when the child died before binding: stdout={stdout}"
        );
        assert!(
            stderr.contains("exited before it finished binding"),
            "must surface the confirmed-death error: stderr={stderr}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Warm-daemon integration test — drives the REAL binary end to end:
    //   start → read daemon.json → two concurrent clients (search + status) →
    //   reindex {pending} → stop → restart.
    //
    // A lex index is pre-seeded so search + status open WITHOUT any model
    // download (the daemon holds the engine; the `pending` reindex on a
    // fully-indexed lex vault has no vector drift it can embed offline, so we
    // only assert the call is accepted, not a doc-count change — that's covered
    // by the engine crate's FakeEmbedder tests). `ONEBRAIN_DAEMON_PORT=0` binds
    // an ephemeral port, discovered via daemon.json; `ONEBRAIN_DAEMON_IDLE_SECS=0`
    // disables idle-shutdown for the test's lifetime. Unix-only (daemon is).
    // ─────────────────────────────────────────────────────────────────────
    #[cfg(unix)]
    #[test]
    fn warm_daemon_concurrent_clients_and_restart() {
        use assert_cmd::Command;
        use onebrain_search::chunk::Chunk;
        use onebrain_search::lex::LexIndex;
        use std::time::{Duration, Instant};

        let home = tempdir().unwrap();
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();

        // A minimal vault with a fixed collection so we know the cache path.
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: warm-daemon-it\n",
        )
        .unwrap();
        // NOTE: no `.md` file is written to disk on purpose. The lex index below
        // is seeded directly, so search has data — but `pending_vector_paths`
        // (which walks the DISK) finds nothing new to embed, keeping the
        // `reindex {pending}` call model-free (CI has no cached model). A doc on
        // disk with no DOC_HASHES entry WOULD be pending → a real embed/download.

        // Pre-seed the lex index (no model, no redb vector writes) so the
        // daemon's held engine + `/api/vault/search?mode=lex` have data.
        let tantivy = crate::commands::search_common::index_artifact_path(
            &cache.path().join("warm-daemon-it"),
            "tantivy",
        );
        {
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

        // Env shared by every `onebrain` invocation below.
        let envs: Vec<(&str, String)> = vec![
            ("HOME", home.path().display().to_string()),
            ("ONEBRAIN_VAULT", vault.path().display().to_string()),
            ("ONEBRAIN_CACHE_DIR", cache.path().display().to_string()),
            ("ONEBRAIN_DAEMON_PORT", "0".to_string()),
            ("ONEBRAIN_DAEMON_IDLE_SECS", "0".to_string()),
        ];
        let run = |verb: &str| -> std::process::Output {
            let mut cmd = Command::cargo_bin("onebrain").unwrap();
            for (k, v) in &envs {
                cmd.env(k, v);
            }
            cmd.args(["daemon", verb]).output().expect("spawn onebrain")
        };
        let _teardown = StopDaemonOnDrop {
            bin: assert_cmd::cargo::cargo_bin("onebrain"),
            envs: envs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        };

        // Start the daemon.
        assert!(run("start").status.success(), "daemon start failed");

        // Wait for the slot json to appear (the daemon writes it after binding).
        // ONEBRAIN_VAULT is set, so the daemon binds `vault` → its slot.
        let discovery = slot_file_under_home(home.path(), Some(vault.path()), "json");
        let deadline = Instant::now() + Duration::from_secs(8);
        let info: crate::commands::daemon_client::DaemonInfo = loop {
            if let Ok(Some(info)) = crate::commands::daemon_client::DaemonInfo::read(&discovery) {
                // Also wait until it answers a liveness probe. Use the
                // engine-INDEPENDENT `/api/health` route (matching
                // `daemon_client::is_live`): `/api/internal/status` 503s while
                // the daemon holds no engine (the startup window), so probing it
                // here could spin the readiness loop against a live-but-warming
                // daemon until the deadline.
                let url = format!("http://127.0.0.1:{}/api/health", info.port);
                let ok = ureq::get(&url)
                    .header("x-onebrain-token", &info.token)
                    .call()
                    .is_ok();
                if ok {
                    break info;
                }
            }
            if Instant::now() >= deadline {
                let _ = run("stop");
                panic!("daemon never became ready (no live daemon.json)");
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        // Two concurrent clients: a search and a status, from separate threads,
        // both must succeed (no redb "Database already open").
        let base = format!("http://127.0.0.1:{}", info.port);
        let token = info.token.clone();
        let (b1, t1) = (base.clone(), token.clone());
        let search = std::thread::spawn(move || {
            ureq::get(&format!("{b1}/api/vault/search?q=quick&mode=lex"))
                .header("x-onebrain-token", &t1)
                .call()
                .map(|r| r.status().as_u16())
        });
        let (b2, t2) = (base.clone(), token.clone());
        let status = std::thread::spawn(move || {
            ureq::get(&format!("{b2}/api/internal/status"))
                .header("x-onebrain-token", &t2)
                .call()
                .map(|r| r.status().as_u16())
        });
        let search_code = search.join().unwrap();
        let status_code = status.join().unwrap();
        assert_eq!(search_code.ok(), Some(200), "concurrent search failed");
        assert_eq!(status_code.ok(), Some(200), "concurrent status failed");

        // POST reindex {pending} is accepted (200) — the pending worklist is
        // empty for a lex-seeded vault with no vector drift the daemon can embed
        // offline, so we assert acceptance, not a count change.
        let reindex = ureq::post(&format!("{base}/api/internal/reindex"))
            .header("x-onebrain-token", &token)
            .header("content-type", "application/json")
            .send(r#"{"mode":"pending"}"#);
        assert_eq!(
            reindex.map(|r| r.status().as_u16()).ok(),
            Some(200),
            "reindex pending should be accepted"
        );

        // Kill the daemon, then a fresh `start` must bring one back up (restart).
        assert!(run("stop").status.success(), "daemon stop failed");
        // daemon.json is removed on clean shutdown.
        let gone_deadline = Instant::now() + Duration::from_secs(3);
        while discovery.exists() && Instant::now() < gone_deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !discovery.exists(),
            "daemon.json should be removed on clean shutdown"
        );

        // Restart: a new daemon binds + republishes discovery.
        assert!(run("start").status.success(), "daemon restart failed");
        let restart_deadline = Instant::now() + Duration::from_secs(8);
        let restarted = loop {
            if let Ok(Some(_)) = crate::commands::daemon_client::DaemonInfo::read(&discovery) {
                break true;
            }
            if Instant::now() >= restart_deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let _ = run("stop"); // clean up regardless
        assert!(restarted, "daemon did not restart after stop");
    }

    // ─────────────────────────────────────────────────────────────────────
    // Concurrent-start orchestration — N threads race the REAL `onebrain daemon
    // start` under one shared tempdir HOME + `ONEBRAIN_DAEMON_PORT=0`. Exactly
    // ONE must bind (one live daemon.json / one PID); the rest must report
    // "already running" (started=false). Then a clean stop tears it down.
    //
    // This is the end-to-end proof of the `O_EXCL` concurrent-start guard under
    // real process contention (the unit tests cover the lock fn in isolation).
    // Unix-only (daemon is). Download-free: no vault bound, no engine, no index.
    // ─────────────────────────────────────────────────────────────────────
    #[cfg(unix)]
    #[test]
    fn concurrent_starts_yield_exactly_one_daemon() {
        use assert_cmd::cargo::cargo_bin;
        use std::process::Command as StdCommand;
        use std::time::{Duration, Instant};

        let home = tempdir().unwrap();
        let home_path = home.path().to_path_buf();
        let bin = cargo_bin("onebrain");
        let _teardown = StopDaemonOnDrop {
            bin: bin.clone(),
            envs: vec![("HOME".into(), home_path.display().to_string())],
        };

        // Spawn N threads that each run `onebrain daemon start` as close to
        // simultaneously as possible, collecting each invocation's stdout.
        const N: usize = 6;
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let bin = bin.clone();
            let home_path = home_path.clone();
            handles.push(std::thread::spawn(move || {
                let out = StdCommand::new(&bin)
                    .env("HOME", &home_path)
                    .env("ONEBRAIN_DAEMON_PORT", "0")
                    .env("ONEBRAIN_DAEMON_IDLE_SECS", "0")
                    .args(["daemon", "start"])
                    .output()
                    .expect("spawn onebrain daemon start");
                assert!(
                    out.status.success(),
                    "daemon start exited non-zero: {out:?}"
                );
                String::from_utf8_lossy(&out.stdout).into_owned()
            }));
        }
        let outputs: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Exactly one invocation reports a fresh start; the rest are no-ops.
        // (A racing loser may briefly see "already running" before daemon.json
        // lands, or after — either way it must NOT report a second fresh start.)
        let fresh = outputs
            .iter()
            .filter(|o| o.contains("started") && !o.contains("already running"))
            .count();
        let already = outputs
            .iter()
            .filter(|o| o.contains("already running"))
            .count();
        assert_eq!(
            fresh, 1,
            "exactly one fresh start expected; outputs: {outputs:?}"
        );
        assert_eq!(
            already,
            N - 1,
            "the other {} starts must be no-ops; outputs: {outputs:?}",
            N - 1
        );

        // Wait for the winning daemon to publish discovery, then assert exactly
        // one live slot json + one PID. Bare `daemon start` from a non-vault cwd
        // binds vault-less → the `daemon-none` slot.
        let discovery = slot_file_under_home(home.path(), None, "json");
        let deadline = Instant::now() + Duration::from_secs(8);
        while !discovery.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            discovery.exists(),
            "the winning daemon never published daemon.json"
        );
        let info = crate::commands::daemon_client::DaemonInfo::read(&discovery)
            .unwrap()
            .expect("daemon.json present");
        assert!(info.pid > 0, "discovery records a real PID");

        // Clean teardown: stop, and confirm discovery is removed.
        let stop = StdCommand::new(&bin)
            .env("HOME", home.path())
            .env("ONEBRAIN_DAEMON_PORT", "0")
            .args(["daemon", "stop"])
            .output()
            .expect("spawn onebrain daemon stop");
        assert!(stop.status.success(), "daemon stop failed: {stop:?}");
        let gone = Instant::now() + Duration::from_secs(3);
        while discovery.exists() && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!discovery.exists(), "daemon.json should be gone after stop");
    }

    // ─────────────────────────────────────────────────────────────────────
    // Wedged-lock recovery: a SIGKILL'd `daemon start` can leave `daemon.lock`
    // behind, and if the OS recycles that PID onto an unrelated LIVE process the
    // lock never looks stale → every `daemon start` reports "already running"
    // forever. `onebrain daemon stop` must clear the lock so `start` recovers.
    // We simulate the recycled-PID case by planting a lock whose PID is a
    // live-but-unrelated process (this test process), then assert stop unwedges
    // it and a subsequent start succeeds. Unix-only (daemon is).
    // ─────────────────────────────────────────────────────────────────────
    #[cfg(unix)]
    #[test]
    fn daemon_stop_clears_a_wedged_lock() {
        use assert_cmd::cargo::cargo_bin;
        use std::process::Command as StdCommand;
        use std::time::{Duration, Instant};

        let home = tempdir().unwrap();
        let bin = cargo_bin("onebrain");
        let _teardown = StopDaemonOnDrop {
            bin: bin.clone(),
            envs: vec![("HOME".into(), home.path().display().to_string())],
        };
        let run_dir = home.path().join(".onebrain").join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        // Bare `daemon start`/`stop` from a non-vault cwd operate on the
        // `daemon-none` slot, so plant the wedged lock in THAT slot.
        let lock = slot_file_under_home(home.path(), None, "lock");

        // Plant a wedged lock: its recorded PID is THIS test process — live, but
        // not our daemon. `pid_exists` sees it alive, so without the stop-unlink
        // fix `acquire_start_lock` would treat it as a held lock forever.
        std::fs::write(&lock, std::process::id().to_string()).unwrap();
        assert_eq!(
            read_lock_pid(&lock),
            Some(std::process::id()),
            "planted lock records a live PID"
        );

        let daemon = |verb: &str| -> std::process::Output {
            StdCommand::new(&bin)
                .env("HOME", home.path())
                .env("ONEBRAIN_DAEMON_PORT", "0")
                .env("ONEBRAIN_DAEMON_IDLE_SECS", "0")
                .args(["daemon", verb])
                .output()
                .expect("spawn onebrain daemon")
        };

        // `daemon stop` must clear the wedged lock (nothing live to signal, but
        // it still resets the runtime files).
        assert!(daemon("stop").status.success(), "daemon stop failed");
        assert!(!lock.exists(), "daemon stop must clear the wedged lock");

        // And now `daemon start` succeeds (no longer wedged) and comes up.
        let start = daemon("start");
        assert!(start.status.success(), "daemon start after unwedge failed");
        let out = String::from_utf8_lossy(&start.stdout);
        assert!(
            out.contains("started") && !out.contains("already running"),
            "start after unwedge should be a FRESH start, got: {out}"
        );

        // Confirm it really bound, then clean up.
        let discovery = slot_file_under_home(home.path(), None, "json");
        let deadline = Instant::now() + Duration::from_secs(8);
        while !discovery.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            discovery.exists(),
            "recovered daemon never published its slot json"
        );
        // Teardown: `_teardown` (StopDaemonOnDrop) stops the recovered daemon
        // on EVERY exit path, panicking asserts included.
    }

    // ─────────────────────────────────────────────────────────────────────
    // `daemon start --vault <path>` binds the PASSED vault — the explicit
    // mechanism that replaced `mcp.rs`'s `std::env::set_var("ONEBRAIN_VAULT")`.
    // `$ONEBRAIN_VAULT` is deliberately UNSET here, so a daemon that binds the
    // vault proves the ARGUMENT carried it (threaded start → __run → bind),
    // not the environment. Asserted via `daemon.json.vault`, the canonical
    // bound-vault identity. Unix-only (daemon is).
    // ─────────────────────────────────────────────────────────────────────
    #[cfg(unix)]
    #[test]
    fn daemon_start_vault_arg_binds_that_vault_without_env() {
        use assert_cmd::cargo::cargo_bin;
        use std::process::Command as StdCommand;
        use std::time::{Duration, Instant};

        let home = tempdir().unwrap();
        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: vault-arg-it\n",
        )
        .unwrap();
        let bin = cargo_bin("onebrain");
        let _teardown = StopDaemonOnDrop {
            bin: bin.clone(),
            envs: vec![("HOME".into(), home.path().display().to_string())],
        };

        // Start WITH --vault and WITHOUT $ONEBRAIN_VAULT (env_remove makes the
        // arg the only possible source of the bound vault).
        let start = StdCommand::new(&bin)
            .env("HOME", home.path())
            .env_remove("ONEBRAIN_VAULT")
            .env("ONEBRAIN_DAEMON_PORT", "0")
            .env("ONEBRAIN_DAEMON_IDLE_SECS", "0")
            .args(["daemon", "start", "--vault"])
            .arg(vault.path())
            .output()
            .expect("spawn onebrain daemon start --vault");
        assert!(
            start.status.success(),
            "daemon start --vault exited non-zero: {start:?}"
        );

        // The daemon bound `vault` (via --vault) → its slot.
        let discovery = slot_file_under_home(home.path(), Some(vault.path()), "json");
        let deadline = Instant::now() + Duration::from_secs(8);
        let info = loop {
            if let Ok(Some(info)) = crate::commands::daemon_client::DaemonInfo::read(&discovery) {
                break info;
            }
            if Instant::now() >= deadline {
                // `_teardown` stops the daemon (if any) on this panic.
                panic!("daemon never published its slot json");
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        // The daemon bound the vault from --vault: daemon.json records its
        // canonical identity (NOT null / vault-less).
        let expected = crate::commands::daemon_client::canonical_vault_id(vault.path());
        assert!(expected.is_some(), "test vault must canonicalize");
        assert_eq!(
            info.vault, expected,
            "daemon.json.vault must be the --vault path (arg carried the vault, not env)"
        );
        // Teardown: `_teardown` (StopDaemonOnDrop) stops the daemon on every
        // exit path, the failing-assert ones included.
    }

    // ─────────────────────────────────────────────────────────────────────
    // #262: bare `daemon start` (no `--vault`, no `$ONEBRAIN_VAULT`) must
    // walk up from cwd to find the vault, exactly like `serve`/`search`/
    // `token`/`doctor` do. Spawned with `.current_dir()` set to a
    // SUBDIRECTORY of the vault (not the vault root itself) to prove real
    // walk-up rather than a lucky direct hit. `$ONEBRAIN_VAULT` is removed
    // so the ONLY possible source of the bound vault is cwd walk-up.
    // ─────────────────────────────────────────────────────────────────────
    #[cfg(unix)]
    #[test]
    fn daemon_start_bare_walks_up_cwd_vault() {
        use assert_cmd::cargo::cargo_bin;
        use std::process::Command as StdCommand;
        use std::time::{Duration, Instant};

        let home = tempdir().unwrap();
        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: bare-walkup-it\n",
        )
        .unwrap();
        let subdir = vault.path().join("00-inbox");
        std::fs::create_dir_all(&subdir).unwrap();

        let bin = cargo_bin("onebrain");
        let _teardown = StopDaemonOnDrop {
            bin: bin.clone(),
            envs: vec![("HOME".into(), home.path().display().to_string())],
        };

        // Start with NO --vault and NO $ONEBRAIN_VAULT, from a SUBDIRECTORY
        // of the vault — walk-up is the only way this can resolve.
        let start = StdCommand::new(&bin)
            .env("HOME", home.path())
            .env_remove("ONEBRAIN_VAULT")
            .env("ONEBRAIN_DAEMON_PORT", "0")
            .env("ONEBRAIN_DAEMON_IDLE_SECS", "0")
            .current_dir(&subdir)
            .args(["daemon", "start"])
            .output()
            .expect("spawn onebrain daemon start (bare)");
        assert!(
            start.status.success(),
            "daemon start exited non-zero: {start:?}"
        );

        // The daemon walked up cwd to `vault` → its slot.
        let discovery = slot_file_under_home(home.path(), Some(vault.path()), "json");
        let deadline = Instant::now() + Duration::from_secs(8);
        let info = loop {
            if let Ok(Some(info)) = crate::commands::daemon_client::DaemonInfo::read(&discovery) {
                break info;
            }
            if Instant::now() >= deadline {
                // `_teardown` stops the daemon (if any) on this panic.
                panic!("daemon never published its slot json");
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        // The daemon bound the vault found by walking up from cwd: daemon.json
        // records its canonical identity (NOT null / vault-less).
        let expected = crate::commands::daemon_client::canonical_vault_id(vault.path());
        assert!(expected.is_some(), "test vault must canonicalize");
        assert_eq!(
            info.vault, expected,
            "daemon.json.vault must be the walked-up vault root (bare start must walk up cwd)"
        );
        // Teardown: `_teardown` (StopDaemonOnDrop) stops the daemon on every
        // exit path, the failing-assert ones included.
    }

    // ─────────────────────────────────────────────────────────────────────
    // THE deliverable-verify (#230): two per-vault daemons coexist on one
    // machine — started CONCURRENTLY, each binds its OWN vault + ephemeral port
    // + slot, neither stops the other (no thrash), `daemon status` lists BOTH,
    // and `daemon stop --all` retires them. This is the multi-vault property the
    // whole track exists for; the single-slot model got this wrong (starting B
    // reported A "already running" and never bound B). Unix-only, download-free
    // (distinct auto-collections, engine open needs no model). One shared HOME +
    // CACHE tempdir so it NEVER touches the real ~/.onebrain / real daemon.
    // ─────────────────────────────────────────────────────────────────────
    #[cfg(unix)]
    #[test]
    fn two_vault_daemons_coexist_status_lists_both_and_stop_all_retires() {
        use assert_cmd::cargo::cargo_bin;
        use std::process::Command as StdCommand;
        use std::time::{Duration, Instant};

        let home = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let vault_a = tempdir().unwrap();
        let vault_b = tempdir().unwrap();
        std::fs::write(
            vault_a.path().join("onebrain.yml"),
            "search:\n  collection: two-vault-a\n",
        )
        .unwrap();
        std::fs::write(
            vault_b.path().join("onebrain.yml"),
            "search:\n  collection: two-vault-b\n",
        )
        .unwrap();

        let bin = cargo_bin("onebrain");
        let _teardown = StopDaemonOnDrop {
            bin: bin.clone(),
            envs: vec![
                ("HOME".into(), home.path().display().to_string()),
                (
                    "ONEBRAIN_CACHE_DIR".into(),
                    cache.path().display().to_string(),
                ),
            ],
        };

        // Start a daemon for A and for B CONCURRENTLY (two threads racing), each
        // with an explicit `--vault`. Distinct slots take distinct start locks,
        // so they must NOT serialize, and neither may report "already running".
        let spawn_start = |vault: PathBuf| {
            let bin = bin.clone();
            let home = home.path().to_path_buf();
            let cache = cache.path().to_path_buf();
            std::thread::spawn(move || {
                StdCommand::new(&bin)
                    .env("HOME", &home)
                    .env("ONEBRAIN_CACHE_DIR", &cache)
                    .env_remove("ONEBRAIN_VAULT")
                    .env("ONEBRAIN_DAEMON_PORT", "0")
                    .env("ONEBRAIN_DAEMON_IDLE_SECS", "0")
                    .args(["daemon", "start", "--vault"])
                    .arg(&vault)
                    .output()
                    .expect("spawn daemon start")
            })
        };
        let ta = spawn_start(vault_a.path().to_path_buf());
        let tb = spawn_start(vault_b.path().to_path_buf());
        let out_a = ta.join().unwrap();
        let out_b = tb.join().unwrap();
        assert!(out_a.status.success(), "start A failed: {out_a:?}");
        assert!(out_b.status.success(), "start B failed: {out_b:?}");
        for (label, out) in [("A", &out_a), ("B", &out_b)] {
            let s = String::from_utf8_lossy(&out.stdout);
            assert!(
                s.contains("started") && !s.contains("already running"),
                "start {label} must be a FRESH start (not 'already running'): {s}"
            );
        }

        // Both slots go live (health-probe each on its OWN ephemeral port).
        let disc_a = slot_file_under_home(home.path(), Some(vault_a.path()), "json");
        let disc_b = slot_file_under_home(home.path(), Some(vault_b.path()), "json");
        let read_live = |disc: &Path| -> Option<crate::commands::daemon_client::DaemonInfo> {
            let info = crate::commands::daemon_client::DaemonInfo::read(disc).ok()??;
            let url = format!("http://127.0.0.1:{}/api/health", info.port);
            ureq::get(&url)
                .header("x-onebrain-token", &info.token)
                .call()
                .ok()?;
            Some(info)
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let (info_a, info_b) = loop {
            if let (Some(a), Some(b)) = (read_live(&disc_a), read_live(&disc_b)) {
                break (a, b);
            }
            if Instant::now() >= deadline {
                let _ = StdCommand::new(&bin)
                    .env("HOME", home.path())
                    .args(["daemon", "stop", "--all"])
                    .output();
                panic!(
                    "both daemons never came up (a_json={} b_json={})",
                    disc_a.exists(),
                    disc_b.exists()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        // The core multi-vault properties: distinct slots, ports, pids, vaults.
        assert_ne!(disc_a, disc_b, "A and B must occupy distinct slots");
        assert_ne!(
            info_a.port, info_b.port,
            "each daemon binds its OWN ephemeral port (a={} b={})",
            info_a.port, info_b.port
        );
        assert_ne!(info_a.pid, info_b.pid, "two distinct daemon processes");
        assert_eq!(
            info_a.vault,
            crate::commands::daemon_client::canonical_vault_id(vault_a.path())
        );
        assert_eq!(
            info_b.vault,
            crate::commands::daemon_client::canonical_vault_id(vault_b.path())
        );

        // Neither stopped the other: BOTH still answer after both are up. This is
        // the anti-thrash assertion — the passive coexistence the design demands.
        assert!(
            read_live(&disc_a).is_some(),
            "A must survive B's start (no thrash)"
        );
        assert!(
            read_live(&disc_b).is_some(),
            "B must survive A's start (no thrash)"
        );

        // `daemon status` enumerates BOTH slots.
        let status = StdCommand::new(&bin)
            .env("HOME", home.path())
            .env("ONEBRAIN_CACHE_DIR", cache.path())
            .args(["daemon", "status"])
            .output()
            .expect("daemon status");
        let stat = String::from_utf8_lossy(&status.stdout);
        assert!(
            stat.contains("2 daemons running"),
            "status must list both: {stat}"
        );
        assert!(
            stat.contains(&info_a.port.to_string()) && stat.contains(&info_b.port.to_string()),
            "status must show both ports: {stat}"
        );

        // `daemon stop --all` retires BOTH; their slot jsons disappear.
        let stop = StdCommand::new(&bin)
            .env("HOME", home.path())
            .args(["daemon", "stop", "--all"])
            .output()
            .expect("daemon stop --all");
        assert!(stop.status.success(), "stop --all failed: {stop:?}");
        let gone = Instant::now() + Duration::from_secs(4);
        while (disc_a.exists() || disc_b.exists()) && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !disc_a.exists() && !disc_b.exists(),
            "stop --all must retire both daemons (a={} b={})",
            disc_a.exists(),
            disc_b.exists()
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // LOW-1 start/stop symmetry: a daemon started for vault X lives in X's slot
    // (keyed on canonical(X)). If X's `onebrain.yml` is removed while the dir
    // still exists, `daemon stop --vault X` must STILL target X's slot
    // (canonicalize X directly, don't re-validate it's a vault) rather than
    // dropping to the vault-less slot and stopping the wrong daemon. Unix-only.
    // ─────────────────────────────────────────────────────────────────────
    #[cfg(unix)]
    #[test]
    fn stop_vault_targets_slot_even_after_onebrain_yml_removed() {
        use assert_cmd::cargo::cargo_bin;
        use std::process::Command as StdCommand;
        use std::time::{Duration, Instant};

        let home = tempdir().unwrap();
        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: low1-it\n",
        )
        .unwrap();
        let bin = cargo_bin("onebrain");
        let _teardown = StopDaemonOnDrop {
            bin: bin.clone(),
            envs: vec![("HOME".into(), home.path().display().to_string())],
        };

        // Start a daemon bound to X.
        let start = StdCommand::new(&bin)
            .env("HOME", home.path())
            .env_remove("ONEBRAIN_VAULT")
            .env("ONEBRAIN_DAEMON_PORT", "0")
            .env("ONEBRAIN_DAEMON_IDLE_SECS", "0")
            .args(["daemon", "start", "--vault"])
            .arg(vault.path())
            .output()
            .expect("daemon start --vault");
        assert!(start.status.success(), "start failed: {start:?}");

        let disc = slot_file_under_home(home.path(), Some(vault.path()), "json");
        let deadline = Instant::now() + Duration::from_secs(8);
        while !disc.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(disc.exists(), "daemon never published its slot json");

        // Remove onebrain.yml: X is no longer a "valid vault", but its dir + slot
        // remain, and canonical(X) still resolves — so stop must find X's slot.
        std::fs::remove_file(vault.path().join("onebrain.yml")).unwrap();

        let stop = StdCommand::new(&bin)
            .env("HOME", home.path())
            .env_remove("ONEBRAIN_VAULT")
            .args(["daemon", "stop", "--vault"])
            .arg(vault.path())
            .output()
            .expect("daemon stop --vault");
        assert!(stop.status.success(), "stop --vault failed: {stop:?}");
        let out = String::from_utf8_lossy(&stop.stdout);
        assert!(
            out.contains("stopped"),
            "stop --vault must retire X's daemon even without onebrain.yml, got: {out}"
        );

        // X's slot json is gone → its daemon was actually stopped (not the
        // vault-less slot).
        let gone = Instant::now() + Duration::from_secs(3);
        while disc.exists() && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !disc.exists(),
            "stop --vault X must have retired X's slot daemon"
        );
    }
}
