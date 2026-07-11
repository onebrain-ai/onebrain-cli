//! `vault-config-migration` doctor check.
//!
//! Fires a 🟡 warning when the vault is on the legacy `vault.yml` filename
//! and has not yet migrated to canonical `onebrain.yml`. `doctor --fix`
//! performs a single atomic `fs::rename` to complete the migration.
//!
//! Idempotency contract:
//! - vault has `onebrain.yml` only → OK (no finding).
//! - vault has `vault.yml` only → Warn (migration recommended).
//! - vault has both → Warn (split state — `--fix` keeps `onebrain.yml`,
//!   removes the legacy file).
//! - vault has neither → not this check's concern (VaultYmlCheck reports
//!   the missing-config error).

use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig, CONFIG_FILENAME, LEGACY_CONFIG_FILENAME};
use std::path::Path;

pub const CHECK_NAME: &str = "vault-config-migration";

pub struct VaultConfigMigrationCheck;

impl Check for VaultConfigMigrationCheck {
    fn name(&self) -> &'static str {
        CHECK_NAME
    }

    fn run(&self, vault_root: &Path, _config: &VaultConfig) -> DoctorResult {
        let canonical_exists = vault_root.join(CONFIG_FILENAME).is_file();
        let legacy_exists = vault_root.join(LEGACY_CONFIG_FILENAME).is_file();

        match (canonical_exists, legacy_exists) {
            (true, false) | (false, false) => {
                // (false, false) — no config at all. VaultYmlCheck already
                // emits a clear error; we stay silent to avoid duplicate noise.
                DoctorResult::ok(CHECK_NAME, "onebrain.yml in use")
            }
            (false, true) => DoctorResult::warn(
                CHECK_NAME,
                "Legacy vault.yml detected — run `onebrain doctor --fix` to migrate to onebrain.yml",
            )
            .with_hint("Run onebrain doctor --fix to migrate vault.yml to onebrain.yml")
            .with_details(vec![
                "vault.yml is deprecated; onebrain.yml is the canonical filename in v3.1+"
                    .to_string(),
                "Migration is a single atomic rename · no data loss".to_string(),
            ]),
            (true, true) => DoctorResult::warn(
                CHECK_NAME,
                "Both onebrain.yml and legacy vault.yml present — onebrain.yml will be used",
            )
            .with_hint("Run onebrain doctor --fix to remove the stale vault.yml")
            .with_details(vec![
                "Lookup order prefers onebrain.yml; the legacy file is ignored at runtime"
                    .to_string(),
                "Keeping both invites confusion · safe to delete vault.yml".to_string(),
            ]),
        }
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
    fn ok_when_onebrain_yml_only() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("onebrain.yml"), "").unwrap();
        let r = VaultConfigMigrationCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Ok);
    }

    #[test]
    fn ok_when_neither_present() {
        // VaultYmlCheck reports the actual error · we stay silent here.
        let d = tempdir().unwrap();
        let r = VaultConfigMigrationCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Ok);
    }

    #[test]
    fn warns_when_legacy_only() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("vault.yml"), "old: yes\n").unwrap();
        let r = VaultConfigMigrationCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.message.contains("Legacy vault.yml"));
        assert!(r.hint.as_deref().unwrap_or("").contains("--fix"));
    }

    #[test]
    fn warns_when_both_present() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("onebrain.yml"), "").unwrap();
        std::fs::write(d.path().join("vault.yml"), "").unwrap();
        let r = VaultConfigMigrationCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.message.contains("Both"));
    }
}
