//! OneBrain core types · `onebrain.yml` config · path resolution · error
//! taxonomy.
//!
//! Zero runtime dependencies on filesystem walks or external tools — pure types
//! and parsing. Imported by every other onebrain-* crate.

pub mod config;
pub mod error;
pub mod path;
pub mod scheduler;
pub mod types;

pub use config::{
    load_vault_config, load_vault_config_at, CheckpointPolicy, VaultConfig, VaultFolders,
};
pub use error::{CoreError, Result};
pub use path::{
    emit_legacy_deprecation_warning_once, find_config_file, find_vault_root,
    legacy_warning_was_emitted, require_vault, resolve_vault, ResolvedVault, VaultResolveInputs,
    VaultRoot, VaultSource, CONFIG_FILENAME, LEGACY_CONFIG_FILENAME,
};
pub use types::{DoctorResult, DoctorStatus, Harness, SessionToken};
