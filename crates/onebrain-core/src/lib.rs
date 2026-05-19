//! OneBrain core types · vault.yml config · path resolution · error taxonomy.
//!
//! Zero runtime dependencies on filesystem walks or external tools — pure types
//! and parsing. Imported by every other onebrain-* crate.

pub mod config;
pub mod error;
pub mod path;
pub mod types;

pub use config::{load_vault_config, VaultConfig};
pub use error::{CoreError, Result};
pub use path::{find_vault_root, VaultRoot};
pub use types::SessionToken;
