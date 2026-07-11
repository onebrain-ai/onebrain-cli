//! Orphan-checkpoints check — count `*-checkpoint-*.md` files anywhere under
//! the configured logs folder. Bun parity: `checkOrphanCheckpoints` in
//! `src/lib/validator.ts:219-261`. Walks recursively so both the post-v2.4.0
//! flat layout (`07-logs/checkpoint/`) and the legacy nested layout
//! (`07-logs/YYYY/MM/`) are counted with the same logic.

use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use std::path::Path;
use walkdir::WalkDir;

pub struct OrphanCheckpointsCheck;

impl Check for OrphanCheckpointsCheck {
    fn name(&self) -> &'static str {
        "orphan-checkpoints"
    }

    fn run(&self, vault_root: &Path, config: &VaultConfig) -> DoctorResult {
        let logs_folder = &config.folders.logs;
        let logs_path = vault_root.join(logs_folder);
        if !logs_path.is_dir() {
            return DoctorResult::ok("orphan-checkpoints", "0 orphans");
        }
        let count = WalkDir::new(&logs_path)
            .into_iter()
            .filter_map(|r| r.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.contains("-checkpoint-") && n.ends_with(".md"))
                    .unwrap_or(false)
            })
            .count();
        if count == 0 {
            return DoctorResult::ok("orphan-checkpoints", "0 orphans");
        }
        DoctorResult::warn(
            "orphan-checkpoints",
            format!("{} unmerged checkpoint(s) in {}/", count, logs_folder),
        )
        .with_hint("Run /wrapup to synthesize and merge them")
        .with_details(vec!["Run /wrapup to synthesize and merge them".to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_core::DoctorStatus;
    use tempfile::tempdir;

    fn cfg() -> VaultConfig {
        VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
            search: Default::default(),
            token_optimization: Default::default(),
            stats: Default::default(),
        }
    }

    #[test]
    fn no_logs_dir_reports_zero_orphans() {
        let d = tempdir().unwrap();
        let r = OrphanCheckpointsCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Ok);
        assert_eq!(r.message, "0 orphans");
    }

    #[test]
    fn empty_logs_dir_reports_zero_orphans() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("07-logs")).unwrap();
        let r = OrphanCheckpointsCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Ok);
        assert_eq!(r.message, "0 orphans");
    }

    #[test]
    fn three_flat_checkpoints_report_warn() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs/checkpoint");
        std::fs::create_dir_all(&logs).unwrap();
        for name in [
            "2026-05-19-XXX-checkpoint-01.md",
            "2026-05-19-XXX-checkpoint-02.md",
            "2026-05-19-YYY-checkpoint-01.md",
        ] {
            std::fs::write(logs.join(name), "x").unwrap();
        }
        let r = OrphanCheckpointsCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.message.contains("3 unmerged"));
        assert_eq!(
            r.hint.as_deref(),
            Some("Run /wrapup to synthesize and merge them")
        );
    }

    #[test]
    fn ignores_non_checkpoint_files() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs/session/2026/05");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("2026-05-19-session-01.md"), "x").unwrap();
        let cp = d.path().join("07-logs/checkpoint");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(cp.join("2026-05-19-XXX-checkpoint-01.md"), "x").unwrap();
        let r = OrphanCheckpointsCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.message.contains("1 unmerged"));
    }

    #[test]
    fn nested_legacy_layout_also_counted() {
        // pre-v2.4.0 nested layout: 07-logs/YYYY/MM/...-checkpoint-NN.md
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs/2026/05");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("2026-05-19-XXX-checkpoint-01.md"), "x").unwrap();
        let r = OrphanCheckpointsCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.message.contains("1 unmerged"));
    }
}
