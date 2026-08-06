//! Stub handler for the one remaining unimplemented dispatch path.
//!
//! v3.4.24 (#334) removed the 63 verbs that only ever returned
//! `E_NOT_IMPLEMENTED` — they no longer parse, so they no longer need a stub.
//! What remains is a single HYBRID arm: `plugin uninstall` runs a real
//! implementation for `--harness codex` and falls through to here for every
//! other harness. That is a genuinely-unimplemented branch of a real command,
//! not a placeholder for an absent one.
//!
//! `not_implemented_vault_required` was deleted with the verbs: all 34 of its
//! callers were among the removed 63, and no surviving verb needs the
//! "check the vault first so outside-vault is 64, not 72" ordering — a real
//! vault-required verb reaches `vault_ctx::require` through its own handler.

use anyhow::Result;
use onebrain_core::CoreError;

/// Return an unimplemented-verb error. Caller is the dispatcher; the
/// envelope/exit-code layer turns this into stable exit 72 + a clean error
/// message containing the path (`"plugin uninstall"`).
pub fn not_implemented(path: &str) -> Result<()> {
    Err(CoreError::NotImplemented(path.to_string()).into())
}
