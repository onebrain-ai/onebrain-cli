use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use serde_json::Value;
use std::path::Path;

pub struct ClaudeSettingsCheck;

const STALE_MARKETPLACE_REPO: &str = "kengio/onebrain";
const CANONICAL_MARKETPLACE_REPO: &str = "onebrain-ai/onebrain";

impl Check for ClaudeSettingsCheck {
    fn name(&self) -> &'static str {
        "claude-settings"
    }

    fn run(&self, vault_root: &Path, _config: &VaultConfig) -> DoctorResult {
        let path = vault_root.join(".claude").join("settings.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return DoctorResult::ok("claude-settings", "no vault settings.json");
        };
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => {
                return DoctorResult::warn(
                    "claude-settings",
                    "settings.json contains invalid JSON",
                );
            }
        };
        let repo = parsed
            .pointer("/extraKnownMarketplaces/onebrain/source/repo")
            .and_then(|v| v.as_str());
        if repo == Some(STALE_MARKETPLACE_REPO) {
            return DoctorResult::warn("claude-settings", "stale marketplace repo")
                .with_hint("Run onebrain doctor --fix to rewrite to onebrain-ai/onebrain")
                .with_details(vec![format!(
                    "stale extraKnownMarketplaces.onebrain.source.repo: {} → {}",
                    STALE_MARKETPLACE_REPO, CANONICAL_MARKETPLACE_REPO
                )]);
        }
        DoctorResult::ok("claude-settings", "ok")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cfg() -> VaultConfig {
        VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
        }
    }

    #[test]
    fn missing_settings_file_reports_ok() {
        let d = tempdir().unwrap();
        let r = ClaudeSettingsCheck.run(d.path(), &cfg());
        assert_eq!(r.message, "no vault settings.json");
    }

    #[test]
    fn invalid_json_reports_warn() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        std::fs::write(d.path().join(".claude/settings.json"), "{ not json").unwrap();
        let r = ClaudeSettingsCheck.run(d.path(), &cfg());
        assert!(r.message.contains("invalid JSON"));
    }

    #[test]
    fn stale_marketplace_repo_reports_warn() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        let body = serde_json::json!({
            "extraKnownMarketplaces": {
                "onebrain": {
                    "source": { "repo": "kengio/onebrain" }
                }
            }
        });
        std::fs::write(d.path().join(".claude/settings.json"), body.to_string()).unwrap();
        let r = ClaudeSettingsCheck.run(d.path(), &cfg());
        assert_eq!(r.message, "stale marketplace repo");
        assert!(r.hint.is_some());
        assert!(
            r.details
                .iter()
                .any(|d| d.contains("kengio/onebrain → onebrain-ai/onebrain"))
        );
    }

    #[test]
    fn canonical_marketplace_repo_reports_ok() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        let body = serde_json::json!({
            "extraKnownMarketplaces": {
                "onebrain": {
                    "source": { "repo": "onebrain-ai/onebrain" }
                }
            }
        });
        std::fs::write(d.path().join(".claude/settings.json"), body.to_string()).unwrap();
        let r = ClaudeSettingsCheck.run(d.path(), &cfg());
        assert_eq!(r.message, "ok");
    }

    #[test]
    fn no_marketplaces_key_reports_ok() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        std::fs::write(
            d.path().join(".claude/settings.json"),
            r#"{"permissions":{}}"#,
        )
        .unwrap();
        let r = ClaudeSettingsCheck.run(d.path(), &cfg());
        assert_eq!(r.message, "ok");
    }
}
