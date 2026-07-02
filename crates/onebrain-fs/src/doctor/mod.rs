//! Doctor checks — each implementor returns a `DoctorResult` describing one
//! aspect of vault health. Composed via `run_all_checks` into the full report.
//!
//! Each check is a zero-sized unit struct implementing `Check`, so the
//! orchestrator can build a `Vec<Box<dyn Check>>` and run them sequentially.
//! Sync by design: every check is either a direct filesystem read or a
//! short-lived child process — no async runtime needed.

use onebrain_core::{DoctorResult, VaultConfig};
use std::path::Path;

/// Sync trait — all I/O is direct filesystem reads or short-lived spawn
/// (no async runtime · matches the rest of v3 CLI).
pub trait Check {
    /// Stable identifier surfaced in doctor output (e.g. `"onebrain.yml"`, `"folders"`).
    fn name(&self) -> &'static str;
    fn run(&self, vault_root: &Path, config: &VaultConfig) -> DoctorResult;
}

// Per-check modules — populated in subsequent tasks.
pub mod folders;
pub mod legacy_qmd_collection;
pub mod marketplace;
pub mod orphans;
pub mod plugin;
pub mod plugin_cache;
pub mod settings_hooks;
pub mod vault_config_migration;
pub mod vault_yml;
pub mod vault_yml_keys;

pub use folders::FoldersCheck;
pub use legacy_qmd_collection::LegacyQmdCollectionCheck;
pub use marketplace::ClaudeSettingsCheck;
pub use orphans::OrphanCheckpointsCheck;
pub use plugin::PluginFilesCheck;
pub use plugin_cache::PluginCacheCheck;
pub use settings_hooks::SettingsHooksCheck;
pub use vault_config_migration::VaultConfigMigrationCheck;
pub use vault_yml::VaultYmlCheck;
pub use vault_yml_keys::VaultYmlKeysCheck;

/// Run every filesystem/config check and return results in the canonical
/// order:
/// onebrain.yml · onebrain.yml-keys · **vault-config-migration** ·
/// **legacy-qmd-collection** · folders · plugin-files · settings-hooks ·
/// claude-settings · orphan-checkpoints · plugin-cache.
///
/// `vault-config-migration` sits between `onebrain.yml-keys` and
/// `legacy-qmd-collection` so all the config-shape checks land together in the
/// report's Config section.
///
/// v3.4: the legacy `qmd-embeddings` probe (which spawned `qmd status`) is gone
/// — native search replaced it. The native-search index check lives in the CLI
/// layer (`commands::doctor`) because it needs the `onebrain-search` engine,
/// which this crate doesn't depend on; the CLI appends its row after these
/// checks. Dropping the qmd `qmd status` subprocess also removes doctor's
/// former dominant cost (~870 ms), so the whole pass is now a fast serial run
/// with no background thread.
pub fn run_all_checks(vault_root: &Path, config: &VaultConfig) -> Vec<DoctorResult> {
    let serial_checks: Vec<Box<dyn Check>> = vec![
        Box::new(VaultYmlCheck),
        Box::new(VaultYmlKeysCheck),
        Box::new(VaultConfigMigrationCheck),
        Box::new(LegacyQmdCollectionCheck),
        Box::new(FoldersCheck),
        Box::new(PluginFilesCheck),
        Box::new(SettingsHooksCheck),
        Box::new(ClaudeSettingsCheck),
        Box::new(OrphanCheckpointsCheck),
        Box::new(PluginCacheCheck),
    ];

    serial_checks
        .iter()
        .map(|c| c.run(vault_root, config))
        .collect()
}
