//! v3.1 command implementations.
//!
//! Each v3.1 entry point lives here. Legacy v3.0 modules under
//! `crate::commands::*` remain the underlying implementations for working
//! commands; v3.1 handlers wrap them with the envelope/dispatcher when
//! `--json` is requested, and add new commands (`vault current`,
//! `plugin update`).

pub mod dispatch;
pub mod hook_rewriter;
pub mod plugin_update;
pub mod stubs;
pub mod vault_current;
