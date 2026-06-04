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
//! ## Files (under `~/.onebrain/run/`)
//! - `daemon.pid` — the running daemon's PID (one line, just the integer).
//! - `daemon.log` — the detached child's stdout+stderr and `tracing` output.
//!
//! ## Known limitations (step 1)
//! - **Concurrent-start race (TOCTOU).** `daemon start` checks "is one already
//!   running?" and then spawns in two separate steps. Two `daemon start`
//!   invocations racing in parallel can both pass the check and both spawn,
//!   orphaning one daemon. We leave this unguarded for the single-user CLI;
//!   when it matters it will get an exclusive lock (`flock` / `O_EXCL` PID
//!   file) so only one writer wins.
//! - **Recycled session-leader false positive.** The interim liveness/identity
//!   check (`is_alive`) requires the pid to be a live session leader, which
//!   kills the common recycled-PID false positive — but a recycled pid that
//!   *also* happens to be a session leader (another daemon) still slips
//!   through. Full start-time identity (a `daemon.json` sidecar recording
//!   pid + process start-time) lands in step 2 with the RPC/server infra.

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
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DaemonStatusData {
    pub running: bool,
    pub pid: Option<u32>,
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
        },
        // No file, garbage file, or a recorded-but-dead PID all collapse to
        // "not running". A stale PID file is left on disk here — `run_start`
        // overwrites it and `run_stop` removes it; status is read-only.
        _ => DaemonStatusData {
            running: false,
            pid: None,
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
// Path resolution — `~/.onebrain/run/{daemon.pid,daemon.log}`.
// ─────────────────────────────────────────────────────────────────────────

/// Directory holding the daemon's runtime files: `~/.onebrain/run/`.
///
/// Resolves the home directory via `dirs::home_dir` (honours `$HOME` on Unix,
/// `%USERPROFILE%` on Windows). Errors if the home dir can't be resolved
/// rather than silently falling back to cwd — a misplaced PID file would make
/// `status`/`stop` find nothing.
fn run_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("resolve home directory for daemon run dir")?;
    Ok(home.join(".onebrain").join("run"))
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

fn pid_path() -> Result<PathBuf> {
    Ok(run_dir()?.join("daemon.pid"))
}

fn log_path() -> Result<PathBuf> {
    Ok(run_dir()?.join("daemon.log"))
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
// Public wiring — resolve real paths/probe, do I/O, emit the envelope.
// ─────────────────────────────────────────────────────────────────────────

/// `onebrain daemon status` — read the PID file, probe liveness, report.
pub fn run_status(mode: &OutputMode) -> Result<()> {
    let data = compute_status(&pid_path()?, is_alive);
    let env = Envelope::ok("daemon.status", None, data);
    emit(&env, mode, std::io::stdout().lock(), render_status_text)?;
    Ok(())
}

fn render_status_text(env: &Envelope<DaemonStatusData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    match d.pid {
        Some(pid) if d.running => format!("daemon running (pid {pid})"),
        _ => "daemon not running".to_string(),
    }
}

/// `onebrain daemon start` — spawn a detached `__run` child if not already
/// running, record its PID, report.
pub fn run_start(mode: &OutputMode) -> Result<()> {
    let pid_path = pid_path()?;

    // Already-running guard: a live PID file means a no-op start.
    if let DaemonStatusData {
        running: true,
        pid: Some(pid),
    } = compute_status(&pid_path, is_alive)
    {
        let data = DaemonStartData {
            started: false,
            already_running: true,
            pid,
        };
        let env = Envelope::ok("daemon.start", None, data);
        emit(&env, mode, std::io::stdout().lock(), render_start_text)?;
        return Ok(());
    }

    // Spawn the detached child and record its PID.
    let pid = spawn_detached_run(&log_path()?).context("spawn detached daemon process")?;
    write_pid(&pid_path, pid)?;

    let data = DaemonStartData {
        started: true,
        already_running: false,
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

/// `onebrain daemon stop` — SIGTERM the recorded PID, wait briefly for it to
/// exit, remove the PID file, report.
pub fn run_stop(mode: &OutputMode) -> Result<()> {
    let pid_path = pid_path()?;

    let data = match compute_status(&pid_path, is_alive) {
        DaemonStatusData {
            running: true,
            pid: Some(pid),
        } => {
            terminate(pid).with_context(|| format!("signal daemon pid {pid}"))?;
            // Best-effort: the daemon's SIGTERM handler removes the PID file on
            // its way out, but if it died uncleanly (or isn't ours — `is_alive`
            // gates on the session-leader/pgid identity check, but a recycled
            // session leader could still slip through, see fix A) we still clear
            // the file so a later `start` isn't blocked by a stale PID.
            remove_pid(&pid_path)?;
            DaemonStopData {
                stopped: true,
                pid: Some(pid),
            }
        }
        // Nothing live to stop. Clear any stale PID file so the slate is clean.
        _ => {
            remove_pid(&pid_path)?;
            DaemonStopData {
                stopped: false,
                pid: None,
            }
        }
    };

    let env = Envelope::ok("daemon.stop", None, data);
    emit(&env, mode, std::io::stdout().lock(), render_stop_text)?;
    Ok(())
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
fn spawn_detached_run(log_path: &Path) -> Result<u32> {
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
fn spawn_detached_run(_log_path: &Path) -> Result<u32> {
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
/// - **vault_root** — `$ONEBRAIN_VAULT`, but ONLY if it names a real vault (see
///   [`resolve_daemon_vault`]). Otherwise `None`.
/// - **port** — the shared default ([`crate::commands::serve::DEFAULT_PORT`]).
/// - **token** — freshly generated per process.
/// - **dist_dir** — `$ONEBRAIN_DIST` if set, else `None` (API-only). The plugin
///   passes the pinned webui dist via this env when it launches the daemon.
///
/// VAULT RESOLUTION (fix A): because the detached child `chdir`s to `/`, walk-up
/// from cwd can't find the vault the user started from, so the daemon relies on
/// `$ONEBRAIN_VAULT` (exported by the launcher). That candidate is VALIDATED to
/// be a real OneBrain vault before it is trusted — a missing or non-vault path
/// resolves to `None`, NOT to a placeholder like `/`. With `None`, the vault
/// endpoints (config/tree/file) return 503; the static surface + token still
/// work so the daemon runs and reports cleanly while exposing no filesystem.
///
/// KNOWN LIMITATION (step 1, still open): invoking `onebrain daemon __run`
/// directly while a daemon is already running overwrites `daemon.pid` with this
/// process's PID, orphaning the existing daemon.
pub fn run_internal() -> Result<()> {
    use crate::commands::serve::DEFAULT_PORT;
    use crate::server::{self, generate_token, ServeConfig};

    let pid_path = pid_path()?;
    let log_path = log_path()?;

    init_tracing(&log_path)?;

    let pid = std::process::id();
    write_pid(&pid_path, pid)?;

    // Resolve the vault the daemon serves. The detached child has chdir'd to
    // `/`, so walk-up from cwd is useless; rely on `$ONEBRAIN_VAULT` (the
    // launcher's job to export).
    //
    // SECURITY (fix A): we NEVER fall back to a placeholder root like `/`. A
    // `/` root would let `GET /api/vault/file?path=etc/passwd` serve
    // `/etc/passwd`. Instead we REQUIRE a real vault: the candidate dir from
    // `$ONEBRAIN_VAULT` must actually be a vault (contain `onebrain.yml` or the
    // legacy `vault.yml`). If it isn't — or the env var is unset — we bind
    // `None`, and the vault handlers return 503 (the static surface + token
    // still work, so the daemon runs and reports cleanly; it just exposes no
    // filesystem).
    let vault_root = resolve_daemon_vault();
    // Optional pre-built webui dist, passed by the plugin launcher.
    let dist_dir = std::env::var_os("ONEBRAIN_DIST").map(PathBuf::from);
    let token = generate_token();

    // Port: `$ONEBRAIN_DAEMON_PORT` overrides the shared default. The override
    // exists mainly so the lifecycle integration test can bind a free port and
    // avoid colliding with a real daemon (or a parallel test) on 4317. `0` is
    // honoured (OS-assigned ephemeral port) for tests that don't curl.
    let port = std::env::var("ONEBRAIN_DAEMON_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);

    // Daemon always binds localhost — a persistent boot-time engine should never
    // listen on a public interface implicitly. `serve --host 0.0.0.0` is the
    // explicit, foreground-only path for remote self-host.
    let cfg = ServeConfig::localhost(vault_root, port, token, dist_dir);

    tracing::info!(pid, "daemon __run started; bringing up HTTP surface");

    // Own a tokio runtime for the lifetime of the daemon. `enable_all` turns on
    // the I/O + time drivers the server + signal handling need.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for daemon")?;

    let result = runtime.block_on(async move {
        let shutdown = sigterm_future();
        server::run_server(cfg, shutdown).await
    });

    // Always clear the PID file on the way out, even if the server returned an
    // error — a stale PID file would block the next `daemon start`.
    tracing::info!("daemon shutting down; removing PID file");
    remove_pid(&pid_path)?;
    tracing::info!("PID file removed; exit");
    result
}

/// Resolve the vault the daemon should serve, or `None` when none is bound.
///
/// Reads the `$ONEBRAIN_VAULT` candidate and verifies it is a REAL vault before
/// trusting it (fix A): a directory only counts if it contains a config file
/// (`onebrain.yml`, or legacy `vault.yml`) at its root, exactly the check
/// `onebrain_core::find_vault_root` / `load_vault_config` rely on. We use
/// `find_config_file` (not a walk-up) because `$ONEBRAIN_VAULT` is meant to name
/// the vault root directly. An unset env var, or a path that isn't a vault,
/// yields `None` — never a fallback like `/`.
fn resolve_daemon_vault() -> Option<PathBuf> {
    let candidate = PathBuf::from(std::env::var_os("ONEBRAIN_VAULT")?);
    // `find_config_file` returns `Some(path-to-config)` only when the dir really
    // holds a vault config. Map that to the vault ROOT (the candidate dir).
    if onebrain_core::find_config_file(&candidate).is_some() {
        Some(candidate)
    } else {
        tracing::warn!(
            vault = %candidate.display(),
            "ONEBRAIN_VAULT is not a OneBrain vault (no onebrain.yml/vault.yml); \
             serving with no vault bound (vault endpoints return 503)"
        );
        None
    }
}

/// A future that resolves when SIGTERM is received — the daemon's graceful-
/// shutdown trigger, handed to `server::run_server`'s `with_graceful_shutdown`.
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
                pid: None
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
                pid: Some(4242)
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
                pid: None
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
                pid: None
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

    #[test]
    fn status_text_running_includes_pid() {
        let env = Envelope::ok(
            "daemon.status",
            None,
            DaemonStatusData {
                running: true,
                pid: Some(555),
            },
        );
        let s = render_status_text(&env);
        assert!(s.contains("running"), "got: {s}");
        assert!(s.contains("555"), "got: {s}");
    }

    #[test]
    fn status_text_not_running_has_no_pid() {
        let env = Envelope::ok(
            "daemon.status",
            None,
            DaemonStatusData {
                running: false,
                pid: None,
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
        // Lock the JSON shape: { "running": bool, "pid": N|null }.
        let running = serde_json::to_value(DaemonStatusData {
            running: true,
            pid: Some(3),
        })
        .unwrap();
        assert_eq!(running["running"], true);
        assert_eq!(running["pid"], 3);

        let stopped = serde_json::to_value(DaemonStatusData {
            running: false,
            pid: None,
        })
        .unwrap();
        assert_eq!(stopped["running"], false);
        assert!(stopped["pid"].is_null());
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

        // Small helper: run `onebrain daemon <verb>` with HOME overridden,
        // returning combined stdout as a String. We assert success separately.
        //
        // `ONEBRAIN_DAEMON_PORT=0` makes the detached `__run` bind an
        // OS-assigned ephemeral port instead of the fixed default (4317), so
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
            if status.contains("running") && status.contains("pid") {
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
            last.contains("running") && last.contains("pid"),
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
}
