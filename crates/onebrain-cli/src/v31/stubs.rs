//! Stub handlers for unimplemented v3.1 verbs.
//!
//! Every group's verb list is wired into the dispatcher (the tree shape IS
//! the v3.1 deliverable per the design doc) but most verbs are deferred to
//! later v3.x minors. Stubs return `CoreError::NotImplemented` so the CLI
//! exits 72 with the canonical envelope/exit-code path instead of panicking.

use anyhow::Result;
use onebrain_core::CoreError;
use std::path::PathBuf;

/// Return an unimplemented-verb error. Caller is the dispatcher; the
/// envelope/exit-code layer turns this into stable exit 72 + a clean error
/// message containing the path (`"note search"`, `"memory promote"`, etc.).
pub fn not_implemented(path: &str) -> Result<()> {
    Err(CoreError::NotImplemented(path.to_string()).into())
}

/// R1 C3: vault-required group stubs MUST check for a vault first so the
/// outside-vault path returns exit 64 (E_VAULT_NOT_FOUND) instead of
/// silently short-circuiting on exit 72 (E_NOT_IMPLEMENTED). Inside a
/// vault we still surface "not implemented".
pub fn not_implemented_vault_required(vault_flag: Option<PathBuf>, path: &str) -> Result<()> {
    crate::vault_ctx::require(vault_flag)?;
    Err(CoreError::NotImplemented(path.to_string()).into())
}
