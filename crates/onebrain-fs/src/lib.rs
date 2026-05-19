//! Vault filesystem operations · frontmatter parsing · orphan checkpoint scanning · harness detection.
//!
//! The public surface is small by design: `scan_orphans` plus its `OrphanScanResult`
//! type, and `detect_harnesses` / `detect_harness` for harness detection.
//! The `orphan` module composes 5 internal helpers; the `frontmatter` module
//! is crate-private (used transitively by orphan-scan).

pub mod doctor;
pub mod error;
pub(crate) mod frontmatter;
pub mod harness;
pub mod migrate;
pub mod orphan;
pub mod register_hooks;
pub mod run_skill;
pub mod update;
pub mod vault_sync;

pub use error::{FsError, Result};
pub use harness::{detect_harness, detect_harnesses};
pub use migrate::{run_backfill_recapped, MigrateResult};
pub use orphan::{scan_orphans, OrphanScanResult};
pub use run_skill::{build_prompt, resolve_claude_bin, ClaudeBinResolution, RunSkillError};
pub use update::{run_update, UpdateOptions, UpdateResult};
pub use vault_sync::{
    build_tar_spawn_overrides, normalize_path, resolve_branch, run_vault_sync, VaultSyncOptions,
    VaultSyncResult,
};
