//! Stub — real implementation lands in Slice 6 Task 5.
//!
//! Returns `DoctorResult::ok` so it doesn't pollute the orchestrator output
//! while Tasks 4-11 are still in flight.

use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use std::path::Path;

pub struct FoldersCheck;

impl Check for FoldersCheck {
    fn name(&self) -> &'static str {
        "folders"
    }
    fn run(&self, _vault_root: &Path, _config: &VaultConfig) -> DoctorResult {
        DoctorResult::ok("folders", "stub")
    }
}
