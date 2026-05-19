use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use std::path::Path;

pub struct VaultYmlCheck;

impl Check for VaultYmlCheck {
    fn name(&self) -> &'static str {
        "vault.yml"
    }

    fn run(&self, vault_root: &Path, _config: &VaultConfig) -> DoctorResult {
        let path = vault_root.join("vault.yml");
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => {
                return DoctorResult::error("vault.yml", "vault.yml not found")
                    .with_hint("Run onebrain init to create vault.yml")
                    .with_details(vec![
                        "Run onebrain init to create vault.yml".to_string()
                    ]);
            }
        };
        let parsed: serde_yaml::Value = match serde_yaml::from_str(&content) {
            Ok(v) => v,
            Err(_) => {
                return DoctorResult::error("vault.yml", "vault.yml contains invalid YAML")
                    .with_hint("Check vault.yml syntax")
                    .with_details(vec!["Check vault.yml syntax".to_string()]);
            }
        };
        let mut details = vec![];
        if let Some(uc) = parsed.get("update_channel").and_then(|v| v.as_str()) {
            details.push(format!("update_channel: {}", uc));
        }
        if let Some(qc) = parsed.get("qmd_collection").and_then(|v| v.as_str()) {
            details.push(format!("qmd: {}", qc));
        }
        let mut r = DoctorResult::ok("vault.yml", "valid");
        if !details.is_empty() {
            r = r.with_details(details);
        }
        r
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
        }
    }

    #[test]
    fn missing_file_reports_error() {
        let d = tempdir().unwrap();
        let r = VaultYmlCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert!(r.message.contains("not found"));
    }

    #[test]
    fn invalid_yaml_reports_error() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("vault.yml"), "not: : valid").unwrap();
        let r = VaultYmlCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert!(r.message.contains("invalid YAML"));
    }

    #[test]
    fn valid_yaml_with_qmd_reports_ok_with_details() {
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("vault.yml"),
            "qmd_collection: ob-1\nupdate_channel: stable\n",
        )
        .unwrap();
        let r = VaultYmlCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Ok);
        assert!(r.details.iter().any(|d| d.contains("qmd: ob-1")));
        assert!(r.details.iter().any(|d| d.contains("update_channel: stable")));
    }
}
