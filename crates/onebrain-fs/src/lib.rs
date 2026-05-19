//! Vault filesystem operations · frontmatter parsing · orphan checkpoint scanning.
//!
//! The public surface is small by design: `scan_orphans` plus its `OrphanScanResult`
//! type. The `orphan` module composes 5 internal helpers; the `frontmatter` module
//! is crate-private (used transitively by orphan-scan).

pub mod error;
pub(crate) mod frontmatter;
pub mod orphan;

pub use error::{FsError, Result};
pub use orphan::{scan_orphans, OrphanScanResult};
