//! Stub handlers for unimplemented v3.1 verbs.
//!
//! Every group's verb list is wired into the dispatcher (the tree shape IS
//! the v3.1 deliverable per the design doc) but most verbs are deferred to
//! later v3.x minors. Stubs return `CoreError::NotImplemented` so the CLI
//! exits 72 with the canonical envelope/exit-code path instead of panicking.

use anyhow::Result;
use onebrain_core::CoreError;

/// Return an unimplemented-verb error. Caller is the dispatcher; the
/// envelope/exit-code layer turns this into stable exit 72 + a clean error
/// message containing the path (`"note search"`, `"memory promote"`, etc.).
pub fn not_implemented(path: &str) -> Result<()> {
    Err(CoreError::NotImplemented(path.to_string()).into())
}
