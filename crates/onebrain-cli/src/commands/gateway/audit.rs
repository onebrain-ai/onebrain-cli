//! Append-only JSONL audit log for gateway tool calls:
//! `~/.onebrain/gateway/audit/YYYY-MM.jsonl` — one JSON object per line, one
//! line per tool call the gateway's approval gate (Tasks 2-6) processes.
//!
//! **Persistence pattern mirrors [`super::auth::store`] exactly** — an
//! owner-only directory (0700) via [`ensure_private_dir`], owner-only files
//! (0600) on create, and home resolved ONLY via [`crate::home::home_dir`].
//! See that module for the full precedent chain back to `daemon_client`.
//!
//! **The property that matters here: [`AuditLog::append`] is infallible from
//! the caller's view.** It returns `()`, not `Result`. By the time a caller
//! has an [`AuditEntry`] to record, the tool call it describes has ALREADY
//! happened — there is nothing left to abort, and an audit-log write failure
//! (a full disk, a yanked permission bit, a serialize error) must never fail
//! or block the tool call it's recording, nor the response already on its
//! way back to the client. Every failure mode inside `append` collapses to a
//! single `tracing::warn!` and a normal return; see
//! [`AuditLog::try_append`]'s doc comment for the exact failure modes this
//! covers, and the `append_on_unwritable_dir_does_not_panic` test below for
//! the end-to-end proof.
//!
//! Log messages in this module deliberately never interpolate a filesystem
//! path (only fixed, descriptive strings) — the audit dir lives under the
//! user's home directory, and a path embedded in a `tracing::warn!` that
//! lands in a shared log file would leak the host username for no
//! diagnostic benefit `error = %e` doesn't already provide.
//!
//! ## Dead-code allow
//! Same situation as [`super::auth`]'s own `#![allow(dead_code)]` (see that
//! module's doc comment for the full precedent chain back to
//! [`crate::commands::daemon_client`]). This task builds the foundation
//! Tasks 2-6 wire up: [`AuditLog::open`] gets its first real caller when a
//! later task opens the log inside `gateway::run()`, and `Decision`'s
//! `Auto`/`Denied`/`Blocked` variants (plus `Outcome::Error`) get theirs when
//! the approval-gate logic that chooses between them lands. Every item is
//! already exercised by this module's own unit tests, so none of it is
//! untested dead code — it just has no caller OUTSIDE this module yet.
#![allow(dead_code)]

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

// ── Domain types ─────────────────────────────────────────────────────────

/// One line of the audit log: a single gateway tool-call record.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    /// Unix epoch seconds the call was recorded at.
    pub ts: u64,
    pub client_id: String,
    pub tool: String,
    pub vault: Option<String>,
    /// A **redacted**, one-line, human-readable description of the call's
    /// arguments, built by the CALLER before constructing this entry — e.g.
    /// `"note: Quarterly Plan"` or `"grep: TODO in 01-projects"`. This is
    /// NOT raw input and gets no further scrubbing here: `append` writes it
    /// to disk exactly as given. It must NEVER be a raw note body, a
    /// token, a pairing code, an authorization code, or anything else that
    /// itself needs redaction — the caller (Task 5's approval-gate wiring)
    /// owns getting this right before it ever reaches this struct.
    pub args_summary: String,
    pub decision: Decision,
    pub channel: Option<String>,
    pub duration_ms: u64,
    pub outcome: Outcome,
}

/// How a tool call was allowed (or not) through the gateway's approval
/// gate. `Serialize`s lowercase (`auto`, `approved`, `denied`, `timedout`,
/// `blocked`) so the on-disk JSONL stays a stable, greppable wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Auto,
    Approved,
    Denied,
    TimedOut,
    Blocked,
}

/// Whether the tool call itself succeeded once it was allowed to run.
/// `Serialize`s lowercase (`ok`, `error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Error,
}

// ── Log ──────────────────────────────────────────────────────────────────

/// Handle onto the audit log directory (normally
/// `~/.onebrain/gateway/audit/`). Cheap to construct — holds only the root
/// path; [`Self::append`] opens the target month file fresh on every call
/// (single-process, low-frequency gateway traffic; no file handle or
/// in-memory buffer to keep alive between calls).
pub struct AuditLog {
    root: PathBuf,
}

impl AuditLog {
    /// Open the real gateway audit log at `~/.onebrain/gateway/audit/`
    /// (created 0700 if absent). Same home resolution as
    /// [`super::auth::AuthStore::open`]: [`crate::home::home_dir`], which
    /// honours `$HOME`/`%USERPROFILE%` on both platforms.
    pub fn open() -> Result<AuditLog> {
        let home =
            crate::home::home_dir().context("resolve home directory for gateway audit log")?;
        Self::open_at(home.join(".onebrain").join("gateway").join("audit"))
    }

    /// Open (creating 0700 if absent) the audit log at an arbitrary `root`.
    /// `pub(crate)` — the real entry point is [`Self::open`]; this exists so
    /// tests can point the log at a tempdir instead of the real home.
    pub(crate) fn open_at(root: PathBuf) -> Result<AuditLog> {
        ensure_private_dir(&root)?;
        Ok(AuditLog { root })
    }

    /// Append one entry to the current month's `YYYY-MM.jsonl` file (named
    /// from `entry.ts`, not wall-clock "now" — see [`month_file_name`]).
    ///
    /// **Infallible from the caller's view** — see the module docs for the
    /// full rationale. [`Self::try_append`] does the real work and returns a
    /// `Result`; this wraps it and turns any `Err` into a single
    /// `tracing::warn!`, never a panic and never a propagated error.
    pub fn append(&self, entry: &AuditEntry) {
        if let Err(e) = self.try_append(entry) {
            tracing::warn!(
                error = %e,
                "gateway audit-log append failed; continuing (audit logging must never block a tool call)"
            );
        }
    }

    /// Serialize `entry` to one JSON line, then open the target month file
    /// with `.append(true)` (0600 on create) and write it. Fails on:
    /// serialization (should not happen for this type, but `serde_json`
    /// still returns `Result`), opening the file (missing/unwritable dir,
    /// permission denied, etc.), or the write itself. [`Self::append`] is
    /// the only caller — this exists as a separate method purely so the
    /// `?`-based control flow can stay ordinary `Result` code, with the
    /// infallible-conversion wrapping isolated to one place.
    fn try_append(&self, entry: &AuditEntry) -> Result<()> {
        let mut line = serde_json::to_string(entry).context("serialize gateway audit log entry")?;
        line.push('\n');

        let path = self.root.join(month_file_name(entry.ts));
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&path)
            .context("open gateway audit log month file")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            {
                tracing::warn!(error = %e, "could not re-assert 0600 on gateway audit log file");
            }
        }
        file.write_all(line.as_bytes())
            .context("write gateway audit log entry")?;
        Ok(())
    }
}

/// `YYYY-MM.jsonl` for `ts` (Unix epoch seconds), computed in UTC — a fixed,
/// unambiguous month boundary that never shifts under the host's local
/// timezone or a DST transition. `ts` is always
/// [`super::auth::core::now_epoch_secs`]-sourced by every real caller and so
/// always converts cleanly; an out-of-range value (not reachable in
/// practice) falls back to `"1970-01.jsonl"` rather than panicking — a
/// naming failure must not become an append failure either.
fn month_file_name(ts: u64) -> String {
    let month = i64::try_from(ts)
        .ok()
        .and_then(|secs| chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0))
        .map(|dt| dt.format("%Y-%m").to_string())
        .unwrap_or_else(|| "1970-01".to_string());
    format!("{month}.jsonl")
}

/// Create `dir` with owner-only (0700) permissions on Unix, re-asserting the
/// mode (and warning, never silently swallowing) if it already existed with
/// looser bits. Plain recursive create on non-Unix. Mirrors
/// `auth::store::ensure_private_dir` exactly.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .context("create gateway audit log dir")?;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(error = %e, "could not re-assert 0700 on gateway audit log dir");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).context("create gateway audit log dir")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(ts: u64, client_id: &str) -> AuditEntry {
        AuditEntry {
            ts,
            client_id: client_id.to_string(),
            tool: "note_create".to_string(),
            vault: Some("work".to_string()),
            args_summary: "note: Quarterly Plan".to_string(),
            decision: Decision::Approved,
            channel: Some("telegram".to_string()),
            duration_ms: 42,
            outcome: Outcome::Ok,
        }
    }

    fn read_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    // ── Step 1: shape, lowercase enums, month file naming ──────────────

    #[test]
    fn append_writes_one_json_line_per_call_with_expected_fields_and_lowercase_enums() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open_at(dir.path().join("audit")).unwrap();

        // 1_700_000_000 is 2023-11-14T22:13:20Z; +60s stays in the same month.
        let e1 = sample_entry(1_700_000_000, "client-1");
        let e2 = AuditEntry {
            vault: None,
            channel: None,
            decision: Decision::TimedOut,
            outcome: Outcome::Error,
            ..sample_entry(1_700_000_060, "client-2")
        };
        log.append(&e1);
        log.append(&e2);

        let lines = read_lines(&dir.path().join("audit").join("2023-11.jsonl"));
        assert_eq!(
            lines.len(),
            2,
            "must write exactly one line per append call"
        );

        let v1: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v1["ts"], 1_700_000_000);
        assert_eq!(v1["client_id"], "client-1");
        assert_eq!(v1["tool"], "note_create");
        assert_eq!(v1["vault"], "work");
        assert_eq!(v1["args_summary"], "note: Quarterly Plan");
        assert_eq!(v1["decision"], "approved");
        assert_eq!(v1["channel"], "telegram");
        assert_eq!(v1["duration_ms"], 42);
        assert_eq!(v1["outcome"], "ok");

        let v2: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(v2["client_id"], "client-2");
        assert!(v2["vault"].is_null(), "None must serialize as JSON null");
        assert!(v2["channel"].is_null(), "None must serialize as JSON null");
        assert_eq!(v2["decision"], "timedout");
        assert_eq!(v2["outcome"], "error");
    }

    #[test]
    fn entries_in_different_months_land_in_separate_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("audit");
        let log = AuditLog::open_at(root.clone()).unwrap();

        log.append(&sample_entry(1_700_000_000, "a")); // 2023-11
        log.append(&sample_entry(1_700_000_000 + 40 * 24 * 3600, "b")); // +40d -> 2023-12

        let mut names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names.len(),
            2,
            "two calls a month apart must produce two separate month files"
        );
    }

    // ── Reopen appends, not truncates ───────────────────────────────────

    #[test]
    fn second_open_on_same_month_appends_not_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("audit");

        let log1 = AuditLog::open_at(root.clone()).unwrap();
        log1.append(&sample_entry(1_700_000_000, "client-1"));
        drop(log1);

        let log2 = AuditLog::open_at(root.clone()).unwrap();
        log2.append(&sample_entry(1_700_000_100, "client-2"));

        let lines = read_lines(&root.join("2023-11.jsonl"));
        assert_eq!(
            lines.len(),
            2,
            "reopening the log and appending again must not truncate the month file"
        );
    }

    // ── Unix permission asserts ──────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn audit_dir_and_file_are_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("audit");
        let log = AuditLog::open_at(root.clone()).unwrap();
        log.append(&sample_entry(1_700_000_000, "client-1"));

        let dir_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "gateway audit log dir must be 0700, was {dir_mode:o}"
        );

        let file_mode = std::fs::metadata(root.join("2023-11.jsonl"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "gateway audit log month file must be 0600, was {file_mode:o}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reopening_an_existing_looser_dir_reasserts_0700() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("audit");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _log = AuditLog::open_at(root.clone()).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "open_at must re-assert 0700 on a pre-existing looser dir"
        );
    }

    // ── append is infallible from the caller's view ──────────────────────

    #[cfg(unix)]
    #[test]
    fn append_on_unwritable_dir_does_not_panic() {
        // Skip under root: a superuser ignores the read-only permission bit
        // below, so this test would have nothing to prove. Mirrors the same
        // guard used elsewhere in this crate's permission-failure tests
        // (`migration.rs`, `doctor.rs`).
        extern "C" {
            fn geteuid() -> u32;
        }
        if unsafe { geteuid() } == 0 {
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("audit");
        let log = AuditLog::open_at(root.clone()).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).unwrap();

        // `append`'s signature already guarantees no `Err` can reach the
        // caller; the property under test is that this also does not
        // panic — a failed write here becomes a single `tracing::warn!`
        // and nothing else.
        log.append(&sample_entry(1_700_000_000, "client-1"));

        // Restore before the tempdir drops so cleanup can remove it.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
