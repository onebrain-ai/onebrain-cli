//! Vault filesystem operations · walk trees · count files · scan orphans.

pub mod error;
pub mod scan;

pub use error::{FsError, Result};
pub use scan::count_unembedded;
