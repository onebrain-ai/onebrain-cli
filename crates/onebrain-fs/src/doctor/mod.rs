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
pub mod vault_yml;
pub mod vault_yml_keys;
// pub mod orphans;        // Task 7
// pub mod qmd;            // Task 8
// pub mod plugin;         // Task 9
// pub mod settings_hooks; // Task 10
// pub mod marketplace;    // Task 11

pub use folders::FoldersCheck;
pub use vault_yml::VaultYmlCheck;
pub use vault_yml_keys::VaultYmlKeysCheck;

/// Run every check in declaration order and return all results.
pub fn run_all_checks(vault_root: &Path, config: &VaultConfig) -> Vec<DoctorResult> {
    // Order matches Bun output: vault.yml, vault.yml-keys, folders, plugin-files,
    // settings-hooks, orphan-checkpoints, qmd-embeddings, claude-settings.
    // Tasks 7-11 will insert remaining checks at the correct positions.
    let checks: Vec<Box<dyn Check>> = vec![
        Box::new(VaultYmlCheck),
        Box::new(VaultYmlKeysCheck),
        Box::new(FoldersCheck),
    ];
    checks
        .iter()
        .map(|c| c.run(vault_root, config))
        .collect()
}
