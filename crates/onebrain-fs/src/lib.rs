//! Vault filesystem operations · walk trees · count files · scan orphans.
//!
//! Slice 1 left this crate intentionally light: the only export is the error
//! taxonomy used by future slices. Tree-walking helpers (orphan checkpoint
//! scan, tree counts, etc.) arrive in Slice 2.

pub mod error;
pub mod frontmatter;
pub mod orphan;

pub use error::{FsError, Result};
pub use orphan::{scan_orphans, OrphanScanResult};
