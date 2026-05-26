//! VaultYmlKeysCheck — validates the schema of `onebrain.yml`
//! (legacy `vault.yml` fallback when canonical is absent).
//!
//! Port of Bun `checkVaultYmlKeys` (src/lib/validator.ts:374-515). Verifies:
//! - required top-level keys (hard errors: `folders`)
//! - soft-required top-level keys (warnings: `update_channel`)
//! - required `folders.*` sub-keys (errors)
//! - `update_channel` enum value (errors)
//! - `checkpoint.messages` / `checkpoint.minutes` numeric > 0 (warnings)
//! - deprecated keys: `onebrain_version`, `method`, `runtime.harness` (warnings)
//!
//! Hint strings differ by which combination of errors/warnings fired — see
//! the Bun branch logic for parity.

use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use serde_yaml::Value;
use std::path::Path;

pub struct VaultYmlKeysCheck;

const CHECK_NAME: &str = "onebrain.yml-keys";

const REQUIRED_TOP_LEVEL: &[&str] = &["folders"];
const SOFT_REQUIRED_TOP_LEVEL: &[&str] = &["update_channel"];
const REQUIRED_FOLDER_KEYS: &[&str] = &[
    "inbox",
    "projects",
    "areas",
    "knowledge",
    "resources",
    "agent",
    "archive",
    "logs",
];

impl Check for VaultYmlKeysCheck {
    fn name(&self) -> &'static str {
        CHECK_NAME
    }

    fn run(&self, vault_root: &Path, _config: &VaultConfig) -> DoctorResult {
        // Schema-validate whichever config file is present — canonical
        // `onebrain.yml` preferred, legacy `vault.yml` fallback. The
        // separate `vault-config-migration` check surfaces the filename
        // issue; this check focuses on schema only.
        let path = onebrain_core::find_config_file(vault_root)
            .unwrap_or_else(|| vault_root.join("onebrain.yml"));
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                // Bun parity: no `details` field on the missing-file branch.
                return DoctorResult::error(CHECK_NAME, "onebrain.yml not found")
                    .with_hint("Run onebrain init to create onebrain.yml");
            }
        };

        let parsed: Value = match serde_yaml::from_str(&text) {
            Ok(v) => v,
            Err(_) => {
                return DoctorResult::error(CHECK_NAME, "onebrain.yml contains invalid YAML");
            }
        };

        let raw = match parsed.as_mapping() {
            Some(m) => m,
            None => {
                return DoctorResult::error(CHECK_NAME, "onebrain.yml is not a valid YAML mapping");
            }
        };

        let mut errors: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // Required top-level keys (hard errors)
        for key in REQUIRED_TOP_LEVEL {
            if raw.get(Value::String((*key).to_string())).is_none() {
                errors.push(format!("missing key: {}", key));
            }
        }

        // Soft-required top-level keys (warnings)
        for key in SOFT_REQUIRED_TOP_LEVEL {
            if raw.get(Value::String((*key).to_string())).is_none() {
                warnings.push(format!("missing key: {}", key));
            }
        }

        // Required folder sub-keys
        let folders_map = raw
            .get(Value::String("folders".to_string()))
            .and_then(|v| v.as_mapping());
        for key in REQUIRED_FOLDER_KEYS {
            let present = folders_map
                .map(|m| m.get(Value::String((*key).to_string())).is_some())
                .unwrap_or(false);
            if !present {
                errors.push(format!("missing folders.{}", key));
            }
        }

        // update_channel value validation (only when present)
        if let Some(uc) = raw.get(Value::String("update_channel".to_string())) {
            let is_valid = matches!(uc.as_str(), Some("stable") | Some("next"));
            if !is_valid {
                errors.push(format!(
                    "invalid update_channel: {} (must be stable or next)",
                    yaml_to_display(uc)
                ));
            }
        }

        // checkpoint.messages / checkpoint.minutes (warnings)
        let checkpoint_map = raw
            .get(Value::String("checkpoint".to_string()))
            .and_then(|v| v.as_mapping());
        if let Some(cp) = checkpoint_map {
            if let Some(v) = cp.get(Value::String("messages".to_string())) {
                if !is_positive_number(v) {
                    warnings.push("checkpoint.messages should be a number > 0".to_string());
                }
            }
            if let Some(v) = cp.get(Value::String("minutes".to_string())) {
                if !is_positive_number(v) {
                    warnings.push("checkpoint.minutes should be a number > 0".to_string());
                }
            }
        }

        // Deprecated keys
        if raw
            .get(Value::String("onebrain_version".to_string()))
            .is_some()
        {
            warnings.push("deprecated key: onebrain_version (safe to remove)".to_string());
        }
        if raw.get(Value::String("method".to_string())).is_some() {
            warnings.push("deprecated key: method (safe to remove)".to_string());
        }
        if let Some(runtime) = raw
            .get(Value::String("runtime".to_string()))
            .and_then(|v| v.as_mapping())
        {
            if runtime.get(Value::String("harness".to_string())).is_some() {
                warnings.push("deprecated key: runtime.harness (safe to remove)".to_string());
            }
        }

        // Errors-first branch
        if !errors.is_empty() {
            let has_missing_key = errors.iter().any(|e| e.starts_with("missing key:"));
            let hint = if has_missing_key {
                Some("Run onebrain init --force to recreate onebrain.yml")
            } else {
                None
            };
            let message = format!("{} error(s)", errors.len());
            let details: Vec<String> = match hint {
                Some(h) => {
                    let mut d = errors.clone();
                    d.push(h.to_string());
                    d
                }
                None => errors.clone(),
            };
            let mut r = DoctorResult::error(CHECK_NAME, message).with_details(details);
            if let Some(h) = hint {
                r = r.with_hint(h);
            }
            return r;
        }

        // Warnings branch
        if !warnings.is_empty() {
            let has_deprecated = warnings.iter().any(|w| {
                w.contains("onebrain_version")
                    || w.contains("method")
                    || w.contains("runtime.harness")
            });
            let has_missing_soft_key = warnings.iter().any(|w| w.starts_with("missing key:"));
            let hint: Option<&str> = if has_missing_soft_key && has_deprecated {
                Some("Run onebrain doctor --fix to repair onebrain.yml")
            } else if has_missing_soft_key {
                Some("Run onebrain doctor --fix to backfill defaults")
            } else if has_deprecated {
                Some("Run onebrain doctor --fix to remove deprecated keys")
            } else {
                None
            };
            let message = format!("{} issue(s)", warnings.len());
            let details: Vec<String> = match hint {
                Some(h) => {
                    let mut d = warnings.clone();
                    d.push(h.to_string());
                    d
                }
                None => warnings.clone(),
            };
            let mut r = DoctorResult::warn(CHECK_NAME, message).with_details(details);
            if let Some(h) = hint {
                r = r.with_hint(h);
            }
            return r;
        }

        DoctorResult::ok(CHECK_NAME, "schema ok")
    }
}

/// Match Bun's `typeof value === 'number' && value > 0`. We accept both
/// integer and float YAML numbers; anything non-numeric or non-positive fails.
fn is_positive_number(v: &Value) -> bool {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                f > 0.0
            } else if let Some(i) = n.as_i64() {
                i > 0
            } else if let Some(u) = n.as_u64() {
                u > 0
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Best-effort string rendering of a YAML value for error messages,
/// matching Bun's `String(value)` coercion behavior.
fn yaml_to_display(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        // Sequences/Mappings: Bun would render as `[object Object]` / `1,2,3`.
        // We render the YAML representation — close enough for a doctor message.
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_core::DoctorStatus;
    use std::fs;
    use tempfile::tempdir;

    fn cfg() -> VaultConfig {
        VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
        }
    }

    fn write_yaml(dir: &Path, content: &str) {
        fs::write(dir.join("onebrain.yml"), content).unwrap();
    }

    /// Same helper, but writes the legacy filename — used by the
    /// back-compat read test below.
    fn write_legacy_yaml(dir: &Path, content: &str) {
        fs::write(dir.join("vault.yml"), content).unwrap();
    }

    /// Minimum-valid schema baseline (all required keys + `update_channel: stable`).
    /// Used as a starting point for tests that inject additional fields.
    fn valid_schema() -> &'static str {
        "update_channel: stable\n\
         folders:\n  \
           inbox: 00-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n"
    }

    #[test]
    fn missing_vault_yml_reports_error() {
        let d = tempdir().unwrap();
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert_eq!(r.message, "onebrain.yml not found");
        assert_eq!(
            r.hint.as_deref(),
            Some("Run onebrain init to create onebrain.yml")
        );
        // Bun parity: no details on the missing-file branch.
        assert!(r.details.is_empty());
    }

    #[test]
    fn non_mapping_yaml_reports_error() {
        let d = tempdir().unwrap();
        write_yaml(d.path(), "- just\n- a\n- list\n");
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert_eq!(r.message, "onebrain.yml is not a valid YAML mapping");
    }

    #[test]
    fn invalid_yaml_reports_error() {
        let d = tempdir().unwrap();
        write_yaml(d.path(), "not: : valid");
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert_eq!(r.message, "onebrain.yml contains invalid YAML");
    }

    /// Back-compat read: legacy `vault.yml`-only vault still passes the
    /// schema check (lookup falls back via `find_config_file`).
    #[test]
    fn legacy_vault_yml_still_passes_schema_check() {
        std::env::set_var("ONEBRAIN_QUIET_VAULT_YML_DEPRECATION", "1");
        let d = tempdir().unwrap();
        write_legacy_yaml(d.path(), valid_schema());
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        std::env::remove_var("ONEBRAIN_QUIET_VAULT_YML_DEPRECATION");
        assert_eq!(r.status, DoctorStatus::Ok);
    }

    #[test]
    fn missing_folders_top_level_is_error_with_init_hint() {
        let d = tempdir().unwrap();
        // No `folders:` block at all — triggers "missing key: folders" AND
        // all 8 "missing folders.*" sub-key errors.
        write_yaml(d.path(), "update_channel: stable\n");
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert!(r.details.iter().any(|x| x == "missing key: folders"));
        assert_eq!(
            r.hint.as_deref(),
            Some("Run onebrain init --force to recreate onebrain.yml")
        );
        // Last details entry should be the hint mirror per Bun.
        assert_eq!(
            r.details.last().map(String::as_str),
            Some("Run onebrain init --force to recreate onebrain.yml")
        );
    }

    #[test]
    fn missing_one_folder_subkey_is_error() {
        let d = tempdir().unwrap();
        // Omit `logs:` only — `folders` is present so the "missing key: folders"
        // top-level error is NOT raised, just the one sub-key error.
        write_yaml(
            d.path(),
            "update_channel: stable\n\
             folders:\n  \
               inbox: 00-inbox\n  \
               projects: 01-projects\n  \
               areas: 02-areas\n  \
               knowledge: 03-knowledge\n  \
               resources: 04-resources\n  \
               agent: 05-agent\n  \
               archive: 06-archive\n",
        );
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert!(r.details.iter().any(|x| x == "missing folders.logs"));
        // No `missing key:` error means no init-force hint.
        assert!(r.hint.is_none());
    }

    #[test]
    fn invalid_update_channel_value_is_error() {
        let d = tempdir().unwrap();
        write_yaml(
            d.path(),
            "update_channel: beta\n\
             folders:\n  \
               inbox: 00-inbox\n  \
               projects: 01-projects\n  \
               areas: 02-areas\n  \
               knowledge: 03-knowledge\n  \
               resources: 04-resources\n  \
               agent: 05-agent\n  \
               archive: 06-archive\n  \
               logs: 07-logs\n",
        );
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert!(r
            .details
            .iter()
            .any(|x| x == "invalid update_channel: beta (must be stable or next)"));
        // Pure value error — no missing-key hint.
        assert!(r.hint.is_none());
    }

    #[test]
    fn missing_update_channel_is_warning_with_backfill_hint() {
        let d = tempdir().unwrap();
        write_yaml(
            d.path(),
            "folders:\n  \
               inbox: 00-inbox\n  \
               projects: 01-projects\n  \
               areas: 02-areas\n  \
               knowledge: 03-knowledge\n  \
               resources: 04-resources\n  \
               agent: 05-agent\n  \
               archive: 06-archive\n  \
               logs: 07-logs\n",
        );
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.details.iter().any(|x| x == "missing key: update_channel"));
        assert_eq!(
            r.hint.as_deref(),
            Some("Run onebrain doctor --fix to backfill defaults")
        );
    }

    #[test]
    fn deprecated_onebrain_version_is_warning_with_remove_hint() {
        let d = tempdir().unwrap();
        write_yaml(
            d.path(),
            &format!("{}onebrain_version: \"2.1.0\"\n", valid_schema()),
        );
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r
            .details
            .iter()
            .any(|x| x == "deprecated key: onebrain_version (safe to remove)"));
        assert_eq!(
            r.hint.as_deref(),
            Some("Run onebrain doctor --fix to remove deprecated keys")
        );
    }

    #[test]
    fn deprecated_runtime_harness_is_warning() {
        let d = tempdir().unwrap();
        write_yaml(
            d.path(),
            &format!("{}runtime:\n  harness: claude-code\n", valid_schema()),
        );
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r
            .details
            .iter()
            .any(|x| x == "deprecated key: runtime.harness (safe to remove)"));
        assert_eq!(
            r.hint.as_deref(),
            Some("Run onebrain doctor --fix to remove deprecated keys")
        );
    }

    #[test]
    fn checkpoint_messages_zero_is_warning() {
        let d = tempdir().unwrap();
        write_yaml(
            d.path(),
            &format!("{}checkpoint:\n  messages: 0\n", valid_schema()),
        );
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r
            .details
            .iter()
            .any(|x| x == "checkpoint.messages should be a number > 0"));
    }

    #[test]
    fn clean_schema_is_ok() {
        let d = tempdir().unwrap();
        write_yaml(d.path(), valid_schema());
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Ok);
        assert_eq!(r.message, "schema ok");
        assert!(r.hint.is_none());
        assert!(r.details.is_empty());
    }

    #[test]
    fn missing_soft_key_and_deprecated_combined_uses_repair_hint() {
        let d = tempdir().unwrap();
        // Omit `update_channel` AND include a deprecated `method:` key.
        write_yaml(
            d.path(),
            "method: legacy\n\
             folders:\n  \
               inbox: 00-inbox\n  \
               projects: 01-projects\n  \
               areas: 02-areas\n  \
               knowledge: 03-knowledge\n  \
               resources: 04-resources\n  \
               agent: 05-agent\n  \
               archive: 06-archive\n  \
               logs: 07-logs\n",
        );
        let r = VaultYmlKeysCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert_eq!(
            r.hint.as_deref(),
            Some("Run onebrain doctor --fix to repair onebrain.yml")
        );
    }
}
