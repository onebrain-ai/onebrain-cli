//! qmd-embeddings check — non-fatal qmd status probe. Bun parity:
//! `checkQmdEmbeddings` in `src/lib/validator.ts:124-208`. Spawns `qmd status`
//! with a [`QMD_STATUS_TIMEOUT_SECS`] timeout and parses `Total: N files
//! indexed` / `Pending: M need embedding` from stdout. Any failure (missing
//! binary, timeout, parse error) downgrades to a non-fatal `ok` status so a
//! broken or absent qmd never blocks `onebrain doctor`.
//!
//! The timeout is generous (15 s, up from Bun's 3 s): on a real vault `qmd
//! status` reads a multi-megabyte index and can take ~10 s, so a 3 s cap
//! reported a spurious "unavailable (timeout)" on healthy, well-populated
//! collections.
//!
//! Probe-injection design (`run_with`): the real spawn lives in
//! `real_qmd_probe`, which is hard to unit-test (depends on `qmd` being on
//! PATH and stable output). Unit tests inject a stub `QmdProbe`; the real
//! probe is exercised by integration tests (Slice 6 Task 13).

use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use std::path::Path;
use std::time::Duration;

/// Hard deadline for the `qmd status` probe. Generous because a real index
/// (tens of MB) can take ~10 s to report; a tighter cap produced spurious
/// "qmd status unavailable (timeout)" on healthy vaults.
const QMD_STATUS_TIMEOUT_SECS: u64 = 15;

pub struct QmdEmbeddingsCheck;

/// Outcome of probing `qmd status`. Lets us unit-test all branches without
/// actually spawning the binary.
pub enum QmdProbe {
    /// qmd not found on PATH — non-fatal.
    NotFound,
    /// Spawned but timed out (we killed it).
    Timeout,
    /// Spawned, exited, returned this stdout (may or may not be parseable).
    Stdout(String),
    /// Other I/O error.
    Error,
}

impl QmdEmbeddingsCheck {
    /// Run with an injectable probe (for tests).
    pub fn run_with<F>(probe: F, config: &VaultConfig) -> DoctorResult
    where
        F: FnOnce() -> QmdProbe,
    {
        // 1. Config-level guard · qmd_collection missing is a warn (matches Bun).
        let Some(collection) = &config.qmd_collection else {
            return DoctorResult::warn("qmd-embeddings", "qmd_collection not set in onebrain.yml")
                .with_hint("Run /qmd to set up search index")
                .with_details(vec!["Run /qmd to set up search index".to_string()]);
        };

        // 2. Probe (real call or test stub) · everything else is non-fatal.
        match probe() {
            QmdProbe::NotFound => DoctorResult::ok("qmd-embeddings", "qmd not found in PATH"),
            QmdProbe::Timeout => {
                DoctorResult::ok("qmd-embeddings", "qmd status unavailable (timeout)")
            }
            QmdProbe::Error => DoctorResult::ok("qmd-embeddings", "qmd status unavailable"),
            QmdProbe::Stdout(stdout) => parse_qmd_status(&stdout, collection),
        }
    }
}

impl Check for QmdEmbeddingsCheck {
    fn name(&self) -> &'static str {
        "qmd-embeddings"
    }
    fn run(&self, _vault_root: &Path, config: &VaultConfig) -> DoctorResult {
        Self::run_with(real_qmd_probe, config)
    }
}

/// Parse Bun output: `Total:    N files indexed` + `Pending:  M need embedding`.
/// Bun uses regex; we get the same result with a prefix-strip + first-token
/// integer parse (no regex dep needed).
fn parse_qmd_status(stdout: &str, collection: &str) -> DoctorResult {
    let total = extract_count(stdout, "Total:");
    let pending = extract_count(stdout, "Pending:").unwrap_or(0);
    let Some(total) = total else {
        return DoctorResult::ok("qmd-embeddings", "qmd status unavailable");
    };
    let summary = format!("{} indexed · {} unembedded", total, pending);
    if pending > 0 {
        return DoctorResult::warn("qmd-embeddings", summary)
            .with_hint("Advisory: run /qmd embed when ready (or onebrain doctor --fix)")
            .with_details(vec![
                format!("collection: {}", collection),
                "Advisory: run /qmd embed when ready (or onebrain doctor --fix)".to_string(),
            ]);
    }
    DoctorResult::ok("qmd-embeddings", summary)
        .with_details(vec![format!("collection: {}", collection)])
}

/// Extract the integer immediately following `prefix` on any line of `stdout`.
/// Returns `None` if the prefix is absent or the next whitespace-delimited
/// token is not a non-negative integer.
fn extract_count(stdout: &str, prefix: &str) -> Option<u64> {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix(prefix) {
            let token = rest.split_whitespace().next()?;
            return token.parse().ok();
        }
    }
    None
}

/// Real probe — spawn `qmd status` with a [`QMD_STATUS_TIMEOUT_SECS`] timeout.
/// On any failure returns the matching `QmdProbe` variant. NEVER panics.
///
/// v3 perf rec #2: uses `wait-timeout` to block on the child instead of
/// polling `try_wait()` every 100 ms. Saves up to a full poll-tick of jitter
/// per call (30–100 ms) — the old polling loop could sleep past completion
/// before noticing the child had exited.
///
/// PATH lookup mirrors Bun.which: first standard PATH, then a fallback that
/// augments PATH with `$HOME/.bun/bin` (where `bun install -g qmd` lands by
/// default). On Windows we delegate to `powershell.exe` so the user's profile
/// PATH is consulted, matching Bun.
fn real_qmd_probe() -> QmdProbe {
    use std::io::Read;
    use std::process::Stdio;
    use wait_timeout::ChildExt;

    // 1. Resolve `qmd` binary.
    let mut command = match build_qmd_command() {
        Some(c) => c,
        None => return QmdProbe::NotFound,
    };

    // 2. Spawn with stdio piped.
    let mut child = match command.stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(_) => return QmdProbe::Error,
    };

    // 3. Block on the child with a hard deadline (replaces the old
    //    30 × 100 ms poll). On exit, drain stdout and return. On timeout,
    //    kill + reap.
    match child.wait_timeout(Duration::from_secs(QMD_STATUS_TIMEOUT_SECS)) {
        Ok(Some(_status)) => {
            let mut stdout = String::new();
            if let Some(mut s) = child.stdout.take() {
                if s.read_to_string(&mut stdout).is_err() {
                    return QmdProbe::Error;
                }
            }
            QmdProbe::Stdout(stdout)
        }
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            QmdProbe::Timeout
        }
        Err(_) => QmdProbe::Error,
    }
}

/// Build the platform-appropriate `qmd status` `Command`. Returns `None` when
/// the binary cannot be located on Unix (Windows defers to powershell.exe).
#[cfg(unix)]
fn build_qmd_command() -> Option<std::process::Command> {
    use std::process::Command;
    // Standard PATH first, then PATH augmented with $HOME/.bun/bin (Bun parity).
    let qmd_path = which::which("qmd").ok().or_else(|| {
        let home = std::env::var_os("HOME")?;
        let mut augmented = std::ffi::OsString::from(&home);
        augmented.push("/.bun/bin");
        if let Some(existing) = std::env::var_os("PATH") {
            augmented.push(":");
            augmented.push(existing);
        }
        which::which_in("qmd", Some(&augmented), std::env::current_dir().ok()?).ok()
    })?;
    let mut c = Command::new(qmd_path);
    c.arg("status");
    Some(c)
}

#[cfg(windows)]
fn build_qmd_command() -> Option<std::process::Command> {
    use std::process::Command;
    // Delegate to powershell so the user's profile PATH is consulted (Bun parity).
    let mut c = Command::new("powershell.exe");
    c.args(["-NoProfile", "-Command", "qmd status"]);
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_collection(c: &str) -> VaultConfig {
        VaultConfig {
            qmd_collection: Some(c.to_string()),
            checkpoint: Default::default(),
            folders: Default::default(),
        }
    }
    fn cfg_no_collection() -> VaultConfig {
        VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
        }
    }

    #[test]
    fn no_collection_warns_with_setup_hint() {
        let r = QmdEmbeddingsCheck::run_with(|| QmdProbe::NotFound, &cfg_no_collection());
        assert!(r.message.contains("qmd_collection not set"));
        assert_eq!(r.hint.as_deref(), Some("Run /qmd to set up search index"));
    }

    #[test]
    fn qmd_not_found_is_ok_non_fatal() {
        let r = QmdEmbeddingsCheck::run_with(|| QmdProbe::NotFound, &cfg_with_collection("ob-1"));
        assert_eq!(r.message, "qmd not found in PATH");
    }

    #[test]
    fn timeout_is_ok_non_fatal() {
        let r = QmdEmbeddingsCheck::run_with(|| QmdProbe::Timeout, &cfg_with_collection("ob-1"));
        assert!(r.message.contains("timeout"));
    }

    #[test]
    fn pending_zero_is_ok() {
        let stdout = "Total:    500 files indexed\nPending:  0 need embedding\n";
        let r = QmdEmbeddingsCheck::run_with(
            || QmdProbe::Stdout(stdout.into()),
            &cfg_with_collection("ob-1"),
        );
        assert_eq!(r.message, "500 indexed · 0 unembedded");
        assert!(r.details.iter().any(|d| d.contains("collection: ob-1")));
    }

    #[test]
    fn pending_positive_warns_with_advisory_hint() {
        let stdout = "Total:    500 files indexed\nPending:  3 need embedding\n";
        let r = QmdEmbeddingsCheck::run_with(
            || QmdProbe::Stdout(stdout.into()),
            &cfg_with_collection("ob-1"),
        );
        assert!(r.message.contains("3 unembedded"));
        assert!(r.hint.as_deref().unwrap().contains("Advisory"));
    }

    #[test]
    fn unparseable_stdout_is_ok_unavailable() {
        let stdout = "qmd: unknown command\n";
        let r = QmdEmbeddingsCheck::run_with(
            || QmdProbe::Stdout(stdout.into()),
            &cfg_with_collection("ob-1"),
        );
        assert_eq!(r.message, "qmd status unavailable");
    }
}
