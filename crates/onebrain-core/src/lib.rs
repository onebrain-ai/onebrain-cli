//! OneBrain core types · vault.yml config · path resolution · error taxonomy.
//!
//! Zero runtime dependencies on filesystem walks or external tools — pure types
//! and parsing. Imported by every other onebrain-* crate.

pub mod error;

pub use error::{CoreError, Result};
