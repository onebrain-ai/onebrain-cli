//! Stub — real implementation lands in Slice 6 Task 4.
//!
//! Returns `DoctorResult::ok` so it doesn't pollute the orchestrator output
//! while Tasks 4-11 are still in flight.

use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use std::path::Path;

pub struct VaultYmlCheck;

impl Check for VaultYmlCheck {
    fn name(&self) -> &'static str {
        "vault.yml"
    }
    fn run(&self, _vault_root: &Path, _config: &VaultConfig) -> DoctorResult {
        DoctorResult::ok("vault.yml", "stub")
    }
}
