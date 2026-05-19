//! 8-layer session token resolution mirroring Bun v2.3.3 `resolveSessionToken`.
//!
//! Layer order:
//!   1. WT_SESSION       (Windows Terminal · strip-then-truncate to 8 chars)
//!   2. TMUX_PANE        (tmux pane id   · strip-then-truncate to 8 chars)
//!   3. TERM_SESSION_ID  (macOS Terminal · strip-then-truncate to 8 chars)
//!   4. findClaudeAncestorPid walk-up (Unix · 12 hops · returns claude PID · no cache)
//!   5. Day-scoped cache file ($TMPDIR/onebrain-day-YYYYMMDD.token · read-only)
//!   6. process.ppid (numeric · write to cache · return)
//!   7. PowerShell parent PID (Windows · 3000ms timeout · write to cache · return)
//!   8. Random 5-digit numeric [10000, 99999] · write to cache · return

use crate::{CacheError, Result};
use chrono::Local;
use onebrain_core::SessionToken;
use std::path::{Path, PathBuf};

/// Result of looking up a process: parent PID + command basename.
///
/// `comm_basename` is the trimmed command name with any leading path stripped
/// and `.exe` suffix removed — ready for direct equality comparison with
/// `"claude"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub ppid: u32,
    pub comm_basename: String,
}

/// Function returning [`ProcInfo`] for a given PID, or `None` on lookup failure.
/// Injectable for tests so we never touch real `ps`.
pub type ProcLookup = std::sync::Arc<dyn Fn(u32) -> Option<ProcInfo> + Send + Sync>;

/// Inputs that drive token resolution · injected for testability.
///
/// In production, populated via [`Self::from_env`]. Tests construct one
/// manually to bypass the global env, OS process tree, and `$TMPDIR`.
#[derive(Clone, Default)]
pub struct ResolveInputs {
    pub wt_session: Option<String>,
    pub tmux_pane: Option<String>,
    pub term_session_id: Option<String>,
    pub ppid: Option<u32>,
    /// Override `chrono::Local::now()` for the cache filename · format `YYYYMMDD`.
    pub today_override: Option<String>,
    /// Override `std::env::temp_dir()` (where the day cache lives).
    pub tmp_dir_override: Option<PathBuf>,
    /// Override the default `ps` walker for findClaudeAncestorPid.
    pub proc_lookup: Option<ProcLookup>,
}

impl std::fmt::Debug for ResolveInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolveInputs")
            .field("wt_session", &self.wt_session)
            .field("tmux_pane", &self.tmux_pane)
            .field("term_session_id", &self.term_session_id)
            .field("ppid", &self.ppid)
            .field("today_override", &self.today_override)
            .field("tmp_dir_override", &self.tmp_dir_override)
            .field("proc_lookup", &self.proc_lookup.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

impl ResolveInputs {
    /// Snapshot the real env + process state. Production callers should use this.
    pub fn from_env() -> Self {
        Self {
            wt_session: std::env::var("WT_SESSION").ok(),
            tmux_pane: std::env::var("TMUX_PANE").ok(),
            term_session_id: std::env::var("TERM_SESSION_ID").ok(),
            ppid: parent_pid(),
            today_override: None,
            tmp_dir_override: None,
            proc_lookup: None,
        }
    }
}

fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(std::os::unix::process::parent_id())
    }
    #[cfg(windows)]
    {
        // Full PowerShell parent walk is Layer 7 (subprocess fallback below).
        // For Layer 6 (ppid), Windows users get the system-reported parent if
        // available; if not, fall through to Layer 8 random.
        None
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Compute the basename of a `comm` value as `ps -o comm=` returns it
/// (which may be a full path on Linux/macOS and may end in `.exe` on Windows).
fn comm_basename_of(raw: &str) -> String {
    let trimmed = raw.trim();
    // Strip everything up to and including the last `/` or `\`.
    let after_path = match trimmed.rfind(['/', '\\']) {
        Some(idx) => &trimmed[idx + 1..],
        None => trimmed,
    };
    // Strip trailing `.exe` (case-insensitive).
    if after_path.len() >= 4 {
        let tail = &after_path[after_path.len() - 4..];
        if tail.eq_ignore_ascii_case(".exe") {
            return after_path[..after_path.len() - 4].to_string();
        }
    }
    after_path.to_string()
}

/// Default ProcLookup backed by `ps -o ppid=,comm= -p <pid>`.
///
/// Returns `None` silently on Windows (no `ps`). On Unix, warns to stderr and
/// returns `None` for any unexpected output — surfacing the walk-up failure so
/// the day-cache fallback is visible, not silent.
#[cfg(unix)]
fn default_proc_lookup(pid: u32) -> Option<ProcInfo> {
    use std::process::Command;

    let output = match Command::new("ps")
        .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("onebrain: ps spawn failed for pid {pid} ({})", e.kind());
            return None;
        }
    };
    if !output.status.success() {
        eprintln!(
            "onebrain: ps -p {pid} exited {}",
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let out = stdout.trim();
    if out.is_empty() {
        eprintln!("onebrain: ps -p {pid} returned empty output");
        return None;
    }
    if out.contains('\n') {
        let preview: String = out.replace('\n', " | ").chars().take(120).collect();
        eprintln!("onebrain: ps -p {pid} returned multi-line output: {preview}");
        return None;
    }
    // "<ppid> <comm-or-path>"
    let trimmed = out.trim_start();
    let (ppid_str, rest) = match trimmed.find(char::is_whitespace) {
        Some(idx) => (&trimmed[..idx], trimmed[idx..].trim_start()),
        None => {
            let preview: String = out.chars().take(120).collect();
            eprintln!("onebrain: ps -p {pid} unparseable: {preview}");
            return None;
        }
    };
    let ppid: u32 = match ppid_str.parse() {
        Ok(n) => n,
        Err(_) => return None,
    };
    Some(ProcInfo {
        ppid,
        comm_basename: comm_basename_of(rest),
    })
}

#[cfg(not(unix))]
fn default_proc_lookup(_pid: u32) -> Option<ProcInfo> {
    None
}

/// Walk up the process tree from `start_pid` looking for a process whose
/// `comm_basename` is `claude`. Returns its PID, or `None` if not found.
///
/// Capped at 12 hops — empirically deeper than any shell→multiplexer→claude
/// chain seen in practice, shallow enough that a corrupted process table
/// terminates in milliseconds.
pub fn find_claude_ancestor_pid<F>(start_pid: u32, lookup: F, max_depth: u32) -> Option<u32>
where
    F: Fn(u32) -> Option<ProcInfo>,
{
    let mut current = start_pid;
    for _ in 0..max_depth {
        if current <= 1 {
            return None;
        }
        let info = lookup(current)?;
        if info.comm_basename == "claude" {
            return Some(current);
        }
        if info.ppid <= 1 {
            eprintln!(
                "onebrain: walk-up reached init from pid {start_pid} without finding claude (last comm={})",
                info.comm_basename
            );
            return None;
        }
        current = info.ppid;
    }
    eprintln!(
        "onebrain: walk-up exhausted {max_depth} hops from pid {start_pid} without finding claude"
    );
    None
}

/// Resolve the session token via Bun v2.3.3's 8-layer priority chain.
/// See module docs for the layer order.
pub fn resolve_session_token(inputs: &ResolveInputs) -> Result<SessionToken> {
    // 1. WT_SESSION (truncated to 8 chars after strip)
    if let Some(raw) = &inputs.wt_session {
        if let Some(t) = SessionToken::sanitize_truncated(raw, 8) {
            return Ok(t);
        }
    }

    // 2. TMUX_PANE (truncated to 8 chars after strip)
    if let Some(raw) = &inputs.tmux_pane {
        if let Some(t) = SessionToken::sanitize_truncated(raw, 8) {
            return Ok(t);
        }
    }

    // 3. TERM_SESSION_ID (truncated to 8 chars after strip)
    if let Some(raw) = &inputs.term_session_id {
        if let Some(t) = SessionToken::sanitize_truncated(raw, 8) {
            return Ok(t);
        }
    }

    // 4. findClaudeAncestorPid walk-up · no cache write.
    if let Some(start_ppid) = inputs.ppid {
        if start_ppid > 1 {
            let found = match &inputs.proc_lookup {
                Some(custom) => {
                    let f = custom.clone();
                    find_claude_ancestor_pid(start_ppid, |p| (f)(p), 12)
                }
                None => find_claude_ancestor_pid(start_ppid, default_proc_lookup, 12),
            };
            if let Some(claude_pid) = found {
                if claude_pid > 1 {
                    if let Some(t) = SessionToken::sanitize(&claude_pid.to_string()) {
                        return Ok(t);
                    }
                }
            }
        }
    }

    // Resolve cache file path used by layers 5–8.
    let tmp_dir = inputs
        .tmp_dir_override
        .clone()
        .unwrap_or_else(std::env::temp_dir);
    let today = inputs
        .today_override
        .clone()
        .unwrap_or_else(|| Local::now().format("%Y%m%d").to_string());
    let cache_file = tmp_dir.join(format!("onebrain-day-{today}.token"));

    // 5. Day-scoped cache (read existing)
    if cache_file.is_file() {
        let raw = std::fs::read_to_string(&cache_file).map_err(|source| CacheError::CacheIo {
            path: cache_file.clone(),
            source,
        })?;
        let trimmed = raw.trim();
        // Bun: accept only numeric values > 1 from the cache. The cache is
        // populated by layers 6–8 (ppid, powershell ppid, random numeric) —
        // all numeric — so this matches the cached-token contract.
        if let Ok(n) = trimmed.parse::<i64>() {
            if n > 1 {
                if let Some(t) = SessionToken::sanitize(trimmed) {
                    return Ok(t);
                }
            }
        }
    }

    // 6. ppid · numeric · write to cache · return
    if let Some(ppid) = inputs.ppid {
        if ppid > 1 {
            let raw = ppid.to_string();
            if let Some(t) = SessionToken::sanitize(&raw) {
                write_cache(&cache_file, t.as_str())?;
                return Ok(t);
            }
        }
    }

    // 7. PowerShell parent PID (Windows only) · 3000ms timeout · best-effort
    #[cfg(windows)]
    {
        if let Some(token) = powershell_parent_pid_token(&cache_file) {
            return Ok(token);
        }
    }

    // 8. Random 5-digit numeric [10000, 99999] · write to cache · return
    let random = random_5digit();
    let token = SessionToken::from_clean(random);
    write_cache(&cache_file, token.as_str())?;
    Ok(token)
}

#[cfg(windows)]
fn powershell_parent_pid_token(cache_file: &Path) -> Option<SessionToken> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let mut child = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-Process -Id $PID).Parent.Id",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel::<Option<String>>();
    thread::spawn(move || {
        let mut buf = String::new();
        let res = stdout.read_to_string(&mut buf).ok().map(|_| buf);
        let _ = tx.send(res);
    });

    let raw_opt = match rx.recv_timeout(Duration::from_millis(3000)) {
        Ok(s) => {
            let _ = child.wait();
            s
        }
        Err(_) => {
            let _ = child.kill();
            None
        }
    };
    let raw = raw_opt?;
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    if n <= 1 {
        return None;
    }
    let token = SessionToken::sanitize(&digits)?;
    write_cache(cache_file, token.as_str()).ok()?;
    Some(token)
}

fn write_cache(path: &Path, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CacheError::CacheIo {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, token).map_err(|source| CacheError::CacheIo {
        path: path.to_path_buf(),
        source,
    })
}

/// Generate a random 5-digit decimal string in `[10000, 99999]`.
/// Bun: `Math.floor(Math.random() * 90000) + 10000`.
fn random_5digit() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before UNIX epoch")
        .as_nanos();
    let mut seed = nanos as u64;
    // LCG mix to avoid concentration in low bits of `nanos`.
    seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let n = (seed >> 33) % 90000 + 10000;
    n.to_string()
}

// ---------------------------------------------------------------------------
// Stale state file cleanup (Bun: cleanStaleStateFile)
// ---------------------------------------------------------------------------

/// Delete `$tmpDir/onebrain-{token}.state` if it exists and its mtime is
/// strictly before `process_start`. Mirrors Bun's `cleanStaleStateFile`.
///
/// Quiet on ENOENT (race with concurrent caller). Other I/O errors are surfaced
/// to stderr — they signal $TMPDIR permission problems the user should see.
pub fn clean_stale_state_file(
    token: &SessionToken,
    tmp_dir: &Path,
    process_start: std::time::SystemTime,
) {
    let state_file = tmp_dir.join(format!("onebrain-{}.state", token.as_str()));
    let metadata = match std::fs::metadata(&state_file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!(
                "onebrain: cleanStaleStateFile failed ({}) at {}",
                e.kind(),
                state_file.display()
            );
            return;
        }
    };
    let mtime = match metadata.modified() {
        Ok(t) => t,
        Err(_) => return,
    };
    if mtime < process_start {
        if let Err(e) = std::fs::remove_file(&state_file) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "onebrain: cannot remove stale state file {} ({})",
                    state_file.display(),
                    e.kind()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn lookup_from_pairs(pairs: Vec<(u32, ProcInfo)>) -> ProcLookup {
        Arc::new(move |pid: u32| {
            pairs
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, info)| info.clone())
        })
    }

    // ---- env-var layers ----

    #[test]
    fn wt_session_truncated_to_8_chars() {
        let inputs = ResolveInputs {
            wt_session: Some("abc-12345678".to_string()),
            tmp_dir_override: Some(tempdir().unwrap().path().to_path_buf()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        assert_eq!(token.as_str(), "abc12345");
    }

    #[test]
    fn wt_session_takes_precedence_over_tmux_pane() {
        let inputs = ResolveInputs {
            wt_session: Some("abc-123".to_string()),
            tmux_pane: Some("%12".to_string()),
            term_session_id: Some("xyz".to_string()),
            tmp_dir_override: Some(tempdir().unwrap().path().to_path_buf()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        assert_eq!(token.as_str(), "abc123");
    }

    #[test]
    fn tmux_pane_used_when_wt_session_empty() {
        let inputs = ResolveInputs {
            wt_session: Some("".to_string()),
            tmux_pane: Some("%12".to_string()),
            tmp_dir_override: Some(tempdir().unwrap().path().to_path_buf()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        assert_eq!(token.as_str(), "12");
    }

    #[test]
    fn term_session_id_used_when_others_absent() {
        let inputs = ResolveInputs {
            term_session_id: Some("iterm-7E2A-something-very-long".to_string()),
            tmp_dir_override: Some(tempdir().unwrap().path().to_path_buf()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        // strip non-alphanumeric → "iterm7E2Asomethingverylong" then slice(0,8)
        assert_eq!(token.as_str(), "iterm7E2");
    }

    // ---- findClaudeAncestorPid ----

    #[test]
    fn walk_up_happy_path_at_depth_3() {
        // 100 (bash) → 99 (zsh) → 98 (claude) — found at depth 3.
        let lookup = lookup_from_pairs(vec![
            (
                100,
                ProcInfo {
                    ppid: 99,
                    comm_basename: "bash".to_string(),
                },
            ),
            (
                99,
                ProcInfo {
                    ppid: 98,
                    comm_basename: "zsh".to_string(),
                },
            ),
            (
                98,
                ProcInfo {
                    ppid: 1,
                    comm_basename: "claude".to_string(),
                },
            ),
        ]);
        let inputs = ResolveInputs {
            ppid: Some(100),
            proc_lookup: Some(lookup),
            tmp_dir_override: Some(tempdir().unwrap().path().to_path_buf()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        assert_eq!(token.as_str(), "98");
    }

    #[test]
    fn walk_up_reaches_init_without_claude_falls_through_to_ppid() {
        let lookup = lookup_from_pairs(vec![(
            42,
            ProcInfo {
                ppid: 1,
                comm_basename: "bash".to_string(),
            },
        )]);
        let tmp = tempdir().unwrap();
        let inputs = ResolveInputs {
            ppid: Some(42),
            proc_lookup: Some(lookup),
            tmp_dir_override: Some(tmp.path().to_path_buf()),
            today_override: Some("20260519".to_string()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        // Walk-up gave up → Layer 6 ppid kicks in.
        assert_eq!(token.as_str(), "42");
    }

    #[test]
    fn walk_up_depth_cap_exhausted_falls_through() {
        // 13 hops of bash · cap is 12 · should give up.
        let mut pairs = Vec::new();
        for i in 0..15u32 {
            pairs.push((
                100 + i,
                ProcInfo {
                    ppid: 100 + i + 1,
                    comm_basename: "bash".to_string(),
                },
            ));
        }
        let lookup = lookup_from_pairs(pairs);
        let tmp = tempdir().unwrap();
        let inputs = ResolveInputs {
            ppid: Some(100),
            proc_lookup: Some(lookup),
            tmp_dir_override: Some(tmp.path().to_path_buf()),
            today_override: Some("20260519".to_string()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        // Walk-up exhausted → Layer 6 ppid wins.
        assert_eq!(token.as_str(), "100");
    }

    // ---- cache file location + filename ----

    #[test]
    fn cache_file_uses_yyyymmdd_no_dashes_in_tmp_dir() {
        let tmp = tempdir().unwrap();
        let inputs = ResolveInputs {
            ppid: Some(54321),
            today_override: Some("20260519".to_string()),
            tmp_dir_override: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        let _ = resolve_session_token(&inputs).unwrap();
        let expected = tmp.path().join("onebrain-day-20260519.token");
        assert!(
            expected.is_file(),
            "expected cache file at {expected:?} did not exist"
        );
    }

    #[test]
    fn day_cache_hit_returns_cached_numeric_token() {
        let tmp = tempdir().unwrap();
        let cache_file = tmp.path().join("onebrain-day-20260520.token");
        std::fs::write(&cache_file, "98765").unwrap();
        let inputs = ResolveInputs {
            today_override: Some("20260520".to_string()),
            tmp_dir_override: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        assert_eq!(token.as_str(), "98765");
    }

    #[test]
    fn day_cache_ignored_when_not_numeric_falls_to_random() {
        let tmp = tempdir().unwrap();
        let cache_file = tmp.path().join("onebrain-day-20260521.token");
        // Old-style alphanumeric token left over from a prior version.
        std::fs::write(&cache_file, "abcDE").unwrap();
        let inputs = ResolveInputs {
            today_override: Some("20260521".to_string()),
            tmp_dir_override: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        // Non-numeric → cache ignored → no ppid → falls to random 5-digit.
        let n: u64 = token.as_str().parse().unwrap();
        assert!(
            (10000..=99999).contains(&n),
            "expected 5-digit numeric, got {token}"
        );
    }

    // ---- Layer 6 cache write ----

    #[test]
    fn ppid_layer_writes_to_cache() {
        let tmp = tempdir().unwrap();
        let inputs = ResolveInputs {
            ppid: Some(12345),
            today_override: Some("20260522".to_string()),
            tmp_dir_override: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        assert_eq!(token.as_str(), "12345");
        let cache_file = tmp.path().join("onebrain-day-20260522.token");
        assert_eq!(std::fs::read_to_string(&cache_file).unwrap(), "12345");
    }

    // ---- Layer 8 random fallback ----

    #[test]
    fn random_fallback_emits_5_digit_numeric_in_range() {
        let tmp = tempdir().unwrap();
        let inputs = ResolveInputs {
            // No env vars, no ppid → straight to random.
            today_override: Some("20260523".to_string()),
            tmp_dir_override: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        let token = resolve_session_token(&inputs).unwrap();
        let n: u64 = token.as_str().parse().expect("token is not numeric");
        assert!(
            (10000..=99999).contains(&n),
            "token {n} outside [10000, 99999]"
        );
        // Was written to the cache.
        let cache_file = tmp.path().join("onebrain-day-20260523.token");
        assert_eq!(
            std::fs::read_to_string(&cache_file).unwrap(),
            token.as_str()
        );
    }

    // ---- comm_basename_of ----

    #[test]
    fn comm_basename_strips_unix_path() {
        assert_eq!(comm_basename_of("/usr/local/bin/claude"), "claude");
    }

    #[test]
    fn comm_basename_strips_windows_path_and_exe() {
        assert_eq!(comm_basename_of("C:\\Users\\foo\\claude.exe"), "claude");
    }

    #[test]
    fn comm_basename_trims_whitespace() {
        assert_eq!(comm_basename_of("  bash  "), "bash");
    }

    // ---- clean_stale_state_file ----

    #[test]
    fn clean_stale_state_file_removes_old_file() {
        let tmp = tempdir().unwrap();
        let token = SessionToken::sanitize("abc123").unwrap();
        let state_file = tmp.path().join("onebrain-abc123.state");
        std::fs::write(&state_file, "stale").unwrap();
        // Pretend process started in the far future → mtime is "before" start.
        let far_future =
            std::time::SystemTime::now() + std::time::Duration::from_secs(60 * 60 * 24);
        clean_stale_state_file(&token, tmp.path(), far_future);
        assert!(
            !state_file.exists(),
            "stale state file should have been removed"
        );
    }

    #[test]
    fn clean_stale_state_file_keeps_fresh_file() {
        let tmp = tempdir().unwrap();
        let token = SessionToken::sanitize("abc123").unwrap();
        let state_file = tmp.path().join("onebrain-abc123.state");
        std::fs::write(&state_file, "fresh").unwrap();
        // Process start = far past → mtime is *after* start → keep.
        let far_past = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        clean_stale_state_file(&token, tmp.path(), far_past);
        assert!(state_file.exists(), "fresh state file should be kept");
    }

    #[test]
    fn clean_stale_state_file_silent_on_missing_file() {
        let tmp = tempdir().unwrap();
        let token = SessionToken::sanitize("nope").unwrap();
        // No file exists · should not panic or error.
        clean_stale_state_file(&token, tmp.path(), std::time::SystemTime::now());
    }
}
