use crate::doctor::Check;
use onebrain_core::{find_config_file, DoctorResult, VaultConfig, CONFIG_FILENAME};
use std::path::Path;

pub struct VaultYmlCheck;

impl Check for VaultYmlCheck {
    /// Check name is the canonical config filename `onebrain.yml` (v3.1.1+).
    /// Renamed from the legacy "vault.yml" label once Bun parity was dropped;
    /// the plugin `/doctor` skill matches on this string.
    ///
    /// The check still reads whichever config file is present — canonical
    /// `onebrain.yml` preferred, legacy `vault.yml` fallback.
    fn name(&self) -> &'static str {
        CONFIG_FILENAME
    }

    fn run(&self, vault_root: &Path, _config: &VaultConfig) -> DoctorResult {
        let path = match find_config_file(vault_root) {
            Some(p) => p,
            None => {
                return DoctorResult::error(CONFIG_FILENAME, "onebrain.yml not found")
                    .with_hint("Run onebrain init to create onebrain.yml")
                    .with_details(vec!["Run onebrain init to create onebrain.yml".to_string()]);
            }
        };
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(CONFIG_FILENAME);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => {
                return DoctorResult::error(CONFIG_FILENAME, format!("{filename} not found"))
                    .with_hint("Run onebrain init to create onebrain.yml")
                    .with_details(vec!["Run onebrain init to create onebrain.yml".to_string()]);
            }
        };
        let parsed: serde_yaml::Value = match serde_yaml::from_str(&content) {
            Ok(v) => v,
            Err(_) => {
                return DoctorResult::error(
                    CONFIG_FILENAME,
                    format!("{filename} contains invalid YAML"),
                )
                .with_hint(format!("Check {filename} syntax"))
                .with_details(vec![format!("Check {filename} syntax")]);
            }
        };
        let mut details = vec![];
        if let Some(uc) = parsed.get("update_channel").and_then(|v| v.as_str()) {
            details.push(format!("update_channel: {}", uc));
        }
        if let Some(qc) = parsed.get("qmd_collection").and_then(|v| v.as_str()) {
            details.push(format!("qmd: {}", qc));
        }
        let mut r = DoctorResult::ok(CONFIG_FILENAME, "valid");
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
            search: Default::default(),
            stats: Default::default(),
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
        std::fs::write(d.path().join("onebrain.yml"), "not: : valid").unwrap();
        let r = VaultYmlCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert!(r.message.contains("invalid YAML"));
    }

    #[test]
    fn valid_yaml_with_qmd_reports_ok_with_details() {
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("onebrain.yml"),
            "qmd_collection: ob-1\nupdate_channel: stable\n",
        )
        .unwrap();
        let r = VaultYmlCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Ok);
        assert!(r.details.iter().any(|d| d.contains("qmd: ob-1")));
        assert!(r
            .details
            .iter()
            .any(|d| d.contains("update_channel: stable")));
    }

    /// Back-compat read: legacy `vault.yml`-only vault still passes the
    /// check. Caller's deprecation warning fires at first lookup.
    #[test]
    fn legacy_vault_yml_still_loads() {
        // Guard restores the env var on drop, including if an assertion
        // below panics — an unguarded set/remove pair would otherwise leak
        // it into whatever test runs concurrently.
        let _guard =
            crate::test_support::EnvGuard::set("ONEBRAIN_QUIET_VAULT_YML_DEPRECATION", "1");
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("vault.yml"),
            "qmd_collection: legacy\nupdate_channel: stable\n",
        )
        .unwrap();
        let r = VaultYmlCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Ok);
        assert!(r.details.iter().any(|d| d.contains("qmd: legacy")));
    }
}
