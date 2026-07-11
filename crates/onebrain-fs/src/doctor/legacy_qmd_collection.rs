//! legacy-qmd-collection check — flags a top-level `qmd_collection` key in
//! `onebrain.yml`.
//!
//! v3.4 replaced qmd with the native search engine; the collection name now
//! lives under `search.collection`. A vault carrying the old top-level
//! `qmd_collection` key still works (`collection_for` reads it as a fallback),
//! but it's deprecated — `onebrain doctor --fix` migrates it to
//! `search.collection` and removes the legacy key. This check surfaces that as
//! a `warn` so the report points the user at the fix. A vault with no
//! `qmd_collection` is `ok` (nothing to migrate).
//!
//! Reads the config file from DISK (not the passed `VaultConfig`) so the
//! post-`--fix` re-check sees the migrated file rather than the stale
//! in-memory config loaded before the fix ran. Missing / unreadable /
//! unparseable config → `ok` here (`VaultYmlCheck` owns those failures).

use crate::doctor::Check;
use onebrain_core::{find_config_file, DoctorResult, VaultConfig};
use std::path::Path;

pub struct LegacyQmdCollectionCheck;

impl Check for LegacyQmdCollectionCheck {
    fn name(&self) -> &'static str {
        "legacy-qmd-collection"
    }

    fn run(&self, vault_root: &Path, _config: &VaultConfig) -> DoctorResult {
        match read_qmd_collection(vault_root) {
            Some(collection) => DoctorResult::warn(
                "legacy-qmd-collection",
                format!("legacy qmd_collection ({collection}) — migrate to search.collection"),
            )
            .with_hint("onebrain doctor --fix")
            .with_details(vec![
                "qmd_collection is deprecated in v3.4 — the native search engine reads search.collection".to_string(),
                "onebrain doctor --fix migrates the value and removes the legacy key".to_string(),
            ]),
            None => DoctorResult::ok("legacy-qmd-collection", "no legacy qmd_collection key"),
        }
    }
}

/// The top-level `qmd_collection` value read fresh from the vault's config
/// file, or `None` when the key (or a readable config) is absent. Rendered
/// with `yaml_to_display`-style simplicity: only a string value is returned
/// (a non-string value still warns, shown as its YAML debug form).
fn read_qmd_collection(vault_root: &Path) -> Option<String> {
    let path = find_config_file(vault_root)?;
    let text = std::fs::read_to_string(&path).ok()?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    let value = yaml.get("qmd_collection")?;
    Some(match value.as_str() {
        Some(s) => s.to_string(),
        None => serde_yaml::to_string(value)
            .unwrap_or_else(|_| "<non-string>".to_string())
            .trim()
            .to_string(),
    })
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
            search: Default::default(),
            token_optimization: Default::default(),
            stats: Default::default(),
        }
    }

    #[test]
    fn present_qmd_collection_warns_with_fix_hint() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("onebrain.yml"), "qmd_collection: ob-1\n").unwrap();
        let r = LegacyQmdCollectionCheck.run(d.path(), &cfg());
        assert_eq!(r.status, onebrain_core::DoctorStatus::Warn);
        assert!(r.message.contains("legacy qmd_collection"), "{r:?}");
        assert!(r.message.contains("ob-1"), "{r:?}");
        assert_eq!(r.hint.as_deref(), Some("onebrain doctor --fix"));
        // The details explain the deprecation and the fix path.
        assert!(
            r.details.iter().any(|d| d.contains("search.collection")),
            "{r:?}"
        );
        assert!(
            r.details.iter().any(|d| d.contains("doctor --fix")),
            "{r:?}"
        );
    }

    #[test]
    fn absent_qmd_collection_is_ok() {
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("onebrain.yml"),
            "search:\n  collection: ob-1\n",
        )
        .unwrap();
        let r = LegacyQmdCollectionCheck.run(d.path(), &cfg());
        assert_eq!(r.status, onebrain_core::DoctorStatus::Ok);
        assert!(r.message.contains("no legacy qmd_collection"), "{r:?}");
    }

    #[test]
    fn missing_or_malformed_config_is_ok_here() {
        // VaultYmlCheck owns config-file failures — this check stays quiet.
        let d = tempdir().unwrap();
        let r = LegacyQmdCollectionCheck.run(d.path(), &cfg());
        assert_eq!(r.status, onebrain_core::DoctorStatus::Ok);

        std::fs::write(d.path().join("onebrain.yml"), "not: : valid").unwrap();
        let r = LegacyQmdCollectionCheck.run(d.path(), &cfg());
        assert_eq!(r.status, onebrain_core::DoctorStatus::Ok);
    }

    #[test]
    fn reads_disk_not_the_passed_config() {
        // The passed VaultConfig says None, but the file on disk carries the
        // key — the DISK wins, so a post-fix re-check can't report stale
        // state in the other direction either.
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("onebrain.yml"), "qmd_collection: fresh\n").unwrap();
        let r = LegacyQmdCollectionCheck.run(d.path(), &cfg());
        assert_eq!(r.status, onebrain_core::DoctorStatus::Warn);
        assert!(r.message.contains("fresh"), "{r:?}");
    }

    #[test]
    fn non_string_value_still_warns_with_yaml_form() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("onebrain.yml"), "qmd_collection: [a, b]\n").unwrap();
        let r = LegacyQmdCollectionCheck.run(d.path(), &cfg());
        assert_eq!(r.status, onebrain_core::DoctorStatus::Warn);
    }

    #[test]
    fn check_name_is_stable() {
        // The `/doctor` skill and the CLI fix dispatcher key off this string.
        assert_eq!(LegacyQmdCollectionCheck.name(), "legacy-qmd-collection");
    }
}
