//! qmd-embeddings check — non-fatal qmd status probe. Bun parity:
//! `checkQmdEmbeddings` in `src/lib/validator.ts:124-208`. Parses
//! `Total: N files indexed` / `Pending: M need embedding` from `qmd status`
//! stdout. Any failure (missing binary, timeout, parse error) downgrades to a
//! non-fatal `ok` status so a broken or absent qmd never blocks
//! `onebrain doctor`.
//!
//! The spawn + timeout + PATH resolution live in `onebrain-cache::qmd`
//! ([`probe_qmd_status`]), the single source of truth shared with session-init
//! and `onebrain qmd status`. This module only classifies the outcome into a
//! [`DoctorResult`]. Keeping all consumers on one probe is deliberate: they
//! had drifted (this check used a 15 s timeout + robust PATH lookup while the
//! cache probe used 2 s + a bare spawn), so a slow / PATH-hidden qmd reported
//! differently here than at session startup.
//!
//! Probe-injection design (`run_with`): the real spawn is hard to unit-test
//! (depends on `qmd` being on PATH and stable output). Unit tests inject a
//! stub [`QmdProbe`]; the real probe is exercised by integration tests.

use crate::doctor::Check;
use onebrain_cache::{probe_qmd_status, QmdProbe, QmdStatus};
use onebrain_core::{DoctorResult, VaultConfig};
use std::path::Path;

pub struct QmdEmbeddingsCheck;

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
        Self::run_with(probe_qmd_status, config)
    }
}

/// Parse `qmd status` output: `Total: N files indexed` + `Pending: M need
/// embedding`. Delegates to the shared [`QmdStatus::parse`] so doctor and the
/// other qmd consumers agree on the headline figures.
fn parse_qmd_status(stdout: &str, collection: &str) -> DoctorResult {
    let status = QmdStatus::parse(stdout);
    // Both figures must parse. A missing `Total:` or `Pending:` line means
    // incomplete/corrupted output — report unknown (non-fatal `ok`) rather than
    // inventing a `0` for the missing figure. session-init leaves
    // `pending_embedding` `None` (→ JSON `null`) for the same input, so every
    // consumer of the shared probe now treats it as unknown, not a false zero.
    let (Some(total), Some(pending)) = (status.total_files, status.pending_embedding) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_collection(c: &str) -> VaultConfig {
        VaultConfig {
            qmd_collection: Some(c.to_string()),
            checkpoint: Default::default(),
            folders: Default::default(),
            search: Default::default(),
        }
    }
    fn cfg_no_collection() -> VaultConfig {
        VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
            search: Default::default(),
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

    #[test]
    fn total_present_pending_missing_is_ok_unavailable() {
        // Incomplete/corrupted `qmd status`: a `Total:` line but no `Pending:`
        // line. Report unknown (non-fatal `ok`), not "0 unembedded" — the same
        // input maps to `None` in the shared probe (onebrain-cache test
        // `unembedded_count_is_none_when_stdout_has_no_pending_line`), which
        // session-init surfaces as `null`. Single source of truth: every
        // consumer treats this input as unknown, not a false zero.
        let stdout = "Total:    500 files indexed\n";
        let r = QmdEmbeddingsCheck::run_with(
            || QmdProbe::Stdout(stdout.into()),
            &cfg_with_collection("ob-1"),
        );
        assert_eq!(r.message, "qmd status unavailable");
    }

    #[test]
    fn pending_present_total_missing_is_ok_unavailable() {
        // Symmetric to the above: a `Pending:` line but no `Total:` line is also
        // incomplete output. Locks that BOTH arms of the combined destructure
        // reject a missing figure — guards against a future edit accidentally
        // making `Total:` optional again.
        let stdout = "Pending:  3 need embedding\n";
        let r = QmdEmbeddingsCheck::run_with(
            || QmdProbe::Stdout(stdout.into()),
            &cfg_with_collection("ob-1"),
        );
        assert_eq!(r.message, "qmd status unavailable");
    }

    /// `QmdProbe::Error` (spawn succeeded but the process exited non-zero or
    /// produced an unexpected error) must degrade to non-fatal `ok`, same as
    /// `NotFound` and `Timeout`. Covers the `QmdProbe::Error =>` arm.
    #[test]
    fn error_probe_is_ok_non_fatal() {
        let r = QmdEmbeddingsCheck::run_with(|| QmdProbe::Error, &cfg_with_collection("ob-1"));
        assert_eq!(r.message, "qmd status unavailable");
    }
}
