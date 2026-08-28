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
//! Every `tracing::warn!`/`.context(...)` in this module DOES name the exact
//! path involved, mirroring `auth/store.rs`'s own `ensure_private_dir`/
//! `write_json_atomic` — this is operator-only diagnostic output (the
//! gateway's local log, never a client-facing HTTP response), and an
//! audit-write failure is exactly when an operator needs to know *which*
//! directory or file failed (disk-full vs. permission-denied vs. wrong
//! mount all look identical without the path). The "no secret or host path"
//! constraint elsewhere in this codebase is scoped to client-facing
//! responses and test/assertion messages (see e.g. `server.rs`'s
//! `sanitized_internal`), not to operator tracing — PR 3's final security
//! review blessed exactly this pattern in `auth/store.rs`.
//!
//! ## Dead-code allow
//! Task 1 shipped this module with a blanket `#![allow(dead_code)]` (same
//! situation as [`super::auth`]'s own — see that module's doc comment for
//! the full precedent chain back to [`crate::commands::daemon_client`]):
//! [`AuditLog::open`] had no caller yet, and `Decision`'s `Auto`/`Denied`
//! variants — plus a since-removed `Blocked` and `Outcome::Error` — were
//! never constructed
//! anywhere reachable from `main` (a `pub` item in a BINARY crate is still
//! dead-code-linted by reachability from `main`, unlike a library crate's
//! public API surface).
//!
//! Gateway PR 4, Task 2 gave most of those a real caller: `gateway::run()`
//! now opens the log via [`AuditLog::open`], and `server.rs`'s policy-gate
//! wiring constructs `Decision::Auto`/`Denied` and
//! `Outcome::Ok`/`Error` on every tool call. (Task 2 also constructed a
//! `Decision::Blocked`; Task 5 DELETED that variant when it wired
//! `policy_gate`'s `NeedApproval` arm to a real approval instead of
//! hardcoding "blocked", so the outcomes that arm can produce today are
//! `Approved`, `Denied`, and `TimedOut`.) Gateway PR 4, Task 5's
//! `server::await_approval` gave the last two — `Decision::Approved` (a
//! human's actual "approve" response) and `Decision::TimedOut` (a pending
//! approval that expired unanswered) — their own real callers, so the
//! blanket allow (and every per-variant one) is gone.

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
/// gate. `Serialize`s lowercase (`auto`, `approved`, `denied`, `timedout`)
/// so the on-disk JSONL stays a stable, greppable wire format.
///
/// `Blocked` (a policy `NeedApproval` result with no approval channel to
/// route it to) existed here through Gateway PR 4, Task 2, when it was the
/// ONLY thing a `NeedApproval` outcome could produce — that build had no
/// [`super::approval::Approvals`] registry yet. Task 5 wired
/// `server::policy_gate`'s `NeedApproval` arm to the real registry, so
/// every outcome now resolves to one of the three variants below instead
/// (`Approved`, `Denied` by a human, or `TimedOut` waiting for one) — the
/// state `Blocked` named no longer occurs, so the variant was removed
/// rather than kept as permanently-dead code. A pre-Task-5 audit log may
/// still contain historical `"blocked"` lines on disk; nothing in this
/// codebase reads `Decision` back out of the log (it's `Serialize`-only,
/// operator-inspection-only), so that's harmless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Auto,
    /// A human approved this specific call through an interactive channel
    /// (the native macOS dialog and/or the `/approvals` HTTP surface).
    /// First constructed outside this module's own tests by Gateway PR 4,
    /// Task 5's `server::await_approval`.
    Approved,
    Denied,
    /// A pending approval expired with no response. First constructed
    /// outside this module's own tests by Gateway PR 4, Task 5's
    /// `server::await_approval`.
    TimedOut,
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

    /// Serialize `entry` to one JSON line, re-assert the dir's 0700 (see the
    /// inline comment below), then open the target month file with
    /// `.append(true)` (0600 on create, re-asserted after open too) and
    /// write it. Fails on: serialization (should not happen for this type,
    /// but `serde_json` still returns `Result`), the dir re-assert,
    /// opening the file (missing/unwritable dir, permission denied, etc.),
    /// or the write itself. [`Self::append`] is the only caller — this
    /// exists as a separate method purely so the `?`-based control flow can
    /// stay ordinary `Result` code, with the infallible-conversion wrapping
    /// isolated to one place.
    fn try_append(&self, entry: &AuditEntry) -> Result<()> {
        let mut line = serde_json::to_string(entry).context("serialize gateway audit log entry")?;
        line.push('\n');

        // Re-assert the dir's 0700 on every call, not just once at
        // `open_at` time — the gateway is long-lived, so a directory
        // loosened (or recreated) mid-run should self-heal on the very
        // next append. Matches `auth/store.rs::write_json_atomic`'s own
        // cadence, which re-calls `ensure_private_dir` on every write.
        ensure_private_dir(&self.root)?;

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
            .with_context(|| format!("open gateway audit log month file {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            {
                tracing::warn!(error = %e, path = %path.display(), "could not re-assert 0600 on gateway audit log file");
            }
        }
        file.write_all(line.as_bytes())
            .with_context(|| format!("write gateway audit log entry to {}", path.display()))?;
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
            .with_context(|| format!("create gateway audit log dir {}", dir.display()))?;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(error = %e, path = %dir.display(), "could not re-assert 0700 on gateway audit log dir");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create gateway audit log dir {}", dir.display()))
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

    #[test]
    fn month_file_name_falls_back_on_an_out_of_range_ts_without_panicking() {
        // u64::MAX doesn't even fit in an i64, let alone a representable
        // chrono timestamp — proves the fallback path (not the happy path)
        // without panicking, per the module doc's stated contract.
        assert_eq!(month_file_name(u64::MAX), "1970-01.jsonl");
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
    fn append_retightens_a_pre_existing_loose_month_file_to_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("audit");
        let log = AuditLog::open_at(root.clone()).unwrap();

        // Pre-create the target month file with loose (group/world readable)
        // permissions, as if something else created it or a prior process
        // left it loosened.
        let path = root.join("2023-11.jsonl");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        log.append(&sample_entry(1_700_000_000, "client-1"));

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "append must re-tighten a pre-existing loose month file to 0600, was {mode:o}"
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
