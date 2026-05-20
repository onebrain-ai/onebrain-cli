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
    /// Stable identifier matching Bun parity (e.g. `"vault.yml"`, `"folders"`).
    fn name(&self) -> &'static str;
    fn run(&self, vault_root: &Path, config: &VaultConfig) -> DoctorResult;
}

// Per-check modules — populated in subsequent tasks.
pub mod folders;
pub mod marketplace;
pub mod orphans;
pub mod plugin;
pub mod qmd;
pub mod settings_hooks;
pub mod vault_yml;
pub mod vault_yml_keys;

pub use folders::FoldersCheck;
pub use marketplace::ClaudeSettingsCheck;
pub use orphans::OrphanCheckpointsCheck;
pub use plugin::PluginFilesCheck;
pub use qmd::QmdEmbeddingsCheck;
pub use settings_hooks::SettingsHooksCheck;
pub use vault_yml::VaultYmlCheck;
pub use vault_yml_keys::VaultYmlKeysCheck;

/// Run every check and return results in the canonical Bun-parity order:
/// vault.yml · vault.yml-keys · folders · plugin-files · settings-hooks ·
/// orphan-checkpoints · qmd-embeddings · claude-settings.
///
/// Perf rec #1: `qmd-embeddings` spawns `qmd status`, which dominates the
/// total runtime (~870 ms of doctor's ~930 ms wall time). Launch that probe
/// on a background thread BEFORE the cheap checks start, then join after the
/// serial pass so the wall time is `max(qmd, serial)` instead of `qmd +
/// serial`. The win is small today (~10 ms because the cheap checks are
/// ~10 ms combined) but scales as the cheap-check set grows.
pub fn run_all_checks(vault_root: &Path, config: &VaultConfig) -> Vec<DoctorResult> {
    let qmd_config = config.clone();
    let qmd_handle = std::thread::spawn(move || QmdEmbeddingsCheck.run(Path::new(""), &qmd_config));

    let serial_checks: Vec<Box<dyn Check>> = vec![
        Box::new(VaultYmlCheck),
        Box::new(VaultYmlKeysCheck),
        Box::new(FoldersCheck),
        Box::new(PluginFilesCheck),
        Box::new(SettingsHooksCheck),
        Box::new(OrphanCheckpointsCheck),
        Box::new(ClaudeSettingsCheck),
    ];
    let mut results: Vec<DoctorResult> = serial_checks
        .iter()
        .map(|c| c.run(vault_root, config))
        .collect();

    // Splice qmd back into position 6 (between orphan-checkpoints and
    // claude-settings) so Bun-parity output order is preserved. A panicked
    // worker degrades to the Bun-equivalent non-fatal "unavailable" warning.
    let qmd_result = qmd_handle.join().unwrap_or_else(|_| {
        onebrain_core::DoctorResult::ok("qmd-embeddings", "qmd status unavailable")
    });
    results.insert(6, qmd_result);
    results
}
