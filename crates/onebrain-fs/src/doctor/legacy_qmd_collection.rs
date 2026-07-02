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

use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use std::path::Path;

pub struct LegacyQmdCollectionCheck;

impl Check for LegacyQmdCollectionCheck {
    fn name(&self) -> &'static str {
        "legacy-qmd-collection"
    }

    fn run(&self, _vault_root: &Path, config: &VaultConfig) -> DoctorResult {
        match &config.qmd_collection {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(qmd: Option<&str>) -> VaultConfig {
        VaultConfig {
            qmd_collection: qmd.map(str::to_string),
            checkpoint: Default::default(),
            folders: Default::default(),
            search: Default::default(),
        }
    }

    #[test]
    fn present_qmd_collection_warns_with_fix_hint() {
        let r = LegacyQmdCollectionCheck.run(Path::new(""), &cfg(Some("ob-1")));
        assert_eq!(r.status, onebrain_core::DoctorStatus::Warn);
        assert!(r.message.contains("legacy qmd_collection"), "{r:?}");
        assert!(r.message.contains("ob-1"), "{r:?}");
        assert_eq!(r.hint.as_deref(), Some("onebrain doctor --fix"));
    }

    #[test]
    fn absent_qmd_collection_is_ok() {
        let r = LegacyQmdCollectionCheck.run(Path::new(""), &cfg(None));
        assert_eq!(r.status, onebrain_core::DoctorStatus::Ok);
        assert!(r.message.contains("no legacy qmd_collection"), "{r:?}");
    }
}
