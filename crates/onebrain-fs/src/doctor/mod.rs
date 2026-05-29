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
pub mod marketplace;
pub mod orphans;
pub mod plugin;
pub mod plugin_cache;
pub mod qmd;
pub mod settings_hooks;
pub mod vault_config_migration;
pub mod vault_yml;
pub mod vault_yml_keys;

pub use folders::FoldersCheck;
pub use marketplace::ClaudeSettingsCheck;
pub use orphans::OrphanCheckpointsCheck;
pub use plugin::PluginFilesCheck;
pub use plugin_cache::PluginCacheCheck;
pub use qmd::QmdEmbeddingsCheck;
pub use settings_hooks::SettingsHooksCheck;
pub use vault_config_migration::VaultConfigMigrationCheck;
pub use vault_yml::VaultYmlCheck;
pub use vault_yml_keys::VaultYmlKeysCheck;

/// Run every check and return results in the canonical order:
/// onebrain.yml · onebrain.yml-keys · **vault-config-migration** · folders ·
/// plugin-files · settings-hooks · orphan-checkpoints · qmd-embeddings ·
/// claude-settings.
///
/// `vault-config-migration` (v3.1 addition) sits between `onebrain.yml-keys`
/// and `folders` so it lands near the other config-shape checks.
///
/// Perf rec #1: `qmd-embeddings` spawns `qmd status`, which dominates the
/// total runtime (~870 ms of doctor's ~930 ms wall time). Launch that probe
/// on a background thread BEFORE the cheap checks start, then join after the
/// serial pass so the wall time is `max(qmd, serial)` instead of `qmd +
/// serial`. The win is small today (~10 ms because the cheap checks are
/// ~10 ms combined) but scales as the cheap-check set grows.
pub fn run_all_checks(vault_root: &Path, config: &VaultConfig) -> Vec<DoctorResult> {
    /// Splice position of the qmd row in the final results vector. Must
    /// stay in sync with the canonical order (onebrain.yml ·
    /// onebrain.yml-keys · vault-config-migration · folders · plugin-files ·
    /// settings-hooks · orphan-checkpoints · **qmd-embeddings** ·
    /// claude-settings) — adding or reordering a check in `serial_checks`
    /// requires updating this constant. The `debug_assert_eq!` below
    /// catches misalignment in CI.
    const QMD_SPLICE_POSITION: usize = 7;
    const EXPECTED_SERIAL_LEN: usize = 9;

    let qmd_config = config.clone();
    let qmd_handle = std::thread::spawn(move || QmdEmbeddingsCheck.run(Path::new(""), &qmd_config));

    let serial_checks: Vec<Box<dyn Check>> = vec![
        Box::new(VaultYmlCheck),
        Box::new(VaultYmlKeysCheck),
        Box::new(VaultConfigMigrationCheck),
        Box::new(FoldersCheck),
        Box::new(PluginFilesCheck),
        Box::new(SettingsHooksCheck),
        Box::new(OrphanCheckpointsCheck),
        Box::new(ClaudeSettingsCheck),
        // plugin-cache lands last (after the qmd splice at index 7) so
        // QMD_SPLICE_POSITION stays valid. Final order: … qmd · claude-settings
        // · plugin-cache.
        Box::new(PluginCacheCheck),
    ];
    debug_assert_eq!(
        serial_checks.len(),
        EXPECTED_SERIAL_LEN,
        "serial_checks order changed — update QMD_SPLICE_POSITION + Bun parity test",
    );

    let mut results: Vec<DoctorResult> = serial_checks
        .iter()
        .map(|c| c.run(vault_root, config))
        .collect();

    // Splice qmd back into the canonical position so Bun-parity output order
    // is preserved. A panicked worker is surfaced as a `warn` row (not `ok`)
    // so the user sees the failure rather than mistaking it for a healthy
    // check — Bun returns warn on a hard probe failure too.
    let qmd_result = qmd_handle.join().unwrap_or_else(|_| {
        onebrain_core::DoctorResult::warn(
            "qmd-embeddings",
            "qmd status worker panicked — check qmd install",
        )
    });
    results.insert(QMD_SPLICE_POSITION, qmd_result);
    results
}
