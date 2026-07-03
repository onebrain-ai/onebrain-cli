//! Vault filesystem operations · frontmatter parsing · orphan checkpoint scanning · harness detection.
//!
//! The public surface is small by design: `scan_orphans` plus its `OrphanScanResult`
//! type, and `detect_harnesses` / `detect_harness` for harness detection.
//! The `orphan` module composes 5 internal helpers; the `frontmatter` module
//! is crate-private (used transitively by orphan-scan).

pub mod backup;
pub mod doctor;
pub mod error;
pub(crate) mod frontmatter;
pub mod harness;
pub mod init;
pub mod migrate;
pub mod note;
pub mod orphan;
pub mod register_hooks;
pub mod run_skill;
pub mod task;
pub mod update;
pub mod vault_sync;

pub use backup::{atomic_write_text, backup_config_file, persist_search_key, remove_search_key};
pub use error::{FsError, Result};
pub use harness::{detect_harness, detect_harnesses};
pub use init::{run_init, InitOptions, InitResult, ScheduleEntry, SchedulePreset};
pub use migrate::{run_backfill_recapped, MigrateResult};
pub use orphan::{scan_orphans, OrphanScanResult};
pub use run_skill::{
    build_prompt, resolve_claude_bin, resolve_gemini_bin, HarnessBinResolution, RunSkillError,
};
pub use update::{run_update, UpdateOptions, UpdateResult};
pub use vault_sync::{
    build_tar_spawn_overrides, normalize_path, read_plugin_version, resolve_branch, run_vault_sync,
    VaultSyncOptions, VaultSyncResult,
};
