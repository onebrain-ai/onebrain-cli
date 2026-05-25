//! CLI-side wiring for `onebrain_core::resolve_vault`.
//!
//! Snapshots `--vault` flag + `ONEBRAIN_VAULT` env + cwd, then delegates to
//! the pure resolver in `onebrain_core::path`. Provides three convenience
//! helpers tied to the three vault-dependency classes from
//! skill-alignment §4.7:
//!
//! - [`resolve`] — vault-free commands that still want to know which vault
//!   is active (e.g. for the envelope's `vault` field). Never errors.
//! - [`require`] — vault-required commands. Errors with
//!   `CoreError::VaultNotFound` (exit 64) when no vault.
//! - [`resolve_for_hook`] — hook-protocol commands. Returns `Option<...>` so
//!   the caller can emit the `{"decision":"block",...}` JSON and exit 0
//!   instead of erroring.

use anyhow::{Context, Result};
use onebrain_core::{require_vault, resolve_vault, ResolvedVault, VaultResolveInputs};
use std::path::{Path, PathBuf};

/// Snapshot the live process env + cwd into a [`VaultResolveInputs`].
///
/// Reads the `ONEBRAIN_VAULT` env var separately from the `--vault` flag so
/// the resolver enforces the documented priority chain (flag > env > walk-up).
/// In clap 4.6 the `env =` attribute on a `global = true` flag stops
/// propagating to nested subcommands' `--help` output, so we read the env
/// var here in plain Rust instead.
pub fn snapshot_inputs(flag: Option<PathBuf>) -> Result<VaultResolveInputs> {
    let cwd = std::env::current_dir().context("read current working directory")?;
    let env = std::env::var_os("ONEBRAIN_VAULT").map(PathBuf::from);
    Ok(VaultResolveInputs { flag, env, cwd })
}

/// Vault-free / informational use. Never errors — `Ok(None)` when nothing
/// found. Use for commands that want to *report* the active vault but don't
/// strictly require one (e.g. `vault current`, `harness detect`).
pub fn resolve(flag: Option<PathBuf>) -> Result<Option<ResolvedVault>> {
    let inputs = snapshot_inputs(flag)?;
    Ok(resolve_vault(&inputs)?)
}

/// Vault-required. Returns `CoreError::VaultNotFound` (exit 64) when no
/// vault discovered. Use for `task *`, `memory *`, `note *`, etc. v3.1
/// stubs out those commands at `not_implemented`; v3.2+ commands wire
/// through this helper.
#[allow(dead_code)]
pub fn require(flag: Option<PathBuf>) -> Result<ResolvedVault> {
    let inputs = snapshot_inputs(flag)?;
    Ok(require_vault(&inputs)?)
}

/// Hook-protocol commands (`session init`, `checkpoint *`, `qmd reindex`).
/// Returns `Ok(None)` when no vault — caller emits `{"decision":"block"}`
/// JSON and exits 0. Returns `Ok(Some(...))` when found; caller proceeds.
///
/// v3.1's hook-protocol commands (`session_init`, `qmd_reindex`,
/// `checkpoint`) inherit the legacy v3.0 implementations which already
/// emit the block JSON via their own `find_vault_root` checks; this
/// helper is exposed so v3.2+ hook-protocol commands can use the
/// canonical resolver. Hence not yet called by any dispatcher arm.
#[allow(dead_code)]
pub fn resolve_for_hook(flag: Option<PathBuf>) -> Result<Option<ResolvedVault>> {
    let inputs = snapshot_inputs(flag)?;
    Ok(resolve_vault(&inputs)?)
}

/// Build a [`crate::output::VaultInfo`] from a resolved vault. Convenience
/// for command handlers building envelopes. v3.1 `vault current` inlines
/// the equivalent; v3.2+ vault-required commands centralise on this.
#[allow(dead_code)]
pub fn info_from(resolved: &ResolvedVault) -> crate::output::VaultInfo {
    crate::output::VaultInfo {
        name: resolved.root.name(),
        path: resolved.root.as_path().to_path_buf(),
    }
}

/// Print the canonical "no vault found" error message to stderr, with the
/// helpful quickfix block from skill-alignment §4.7. Used by vault-required
/// commands in text mode (JSON mode renders the envelope's `error` field).
/// Not wired in v3.1 (vault-required commands are stubs); v3.2 `task` /
/// `memory` / `note` handlers will call this in their text-mode error path.
#[allow(dead_code)]
pub fn print_vault_not_found_help(cwd: &Path) {
    eprintln!("Error · E_VAULT_NOT_FOUND (exit 64)");
    eprintln!();
    eprintln!(
        "  No OneBrain vault found by walking up from {}",
        cwd.display()
    );
    eprintln!();
    eprintln!("  Quick fixes:");
    eprintln!("    cd into a vault                  →  cd ~/Documents/ob-1");
    eprintln!(
        "    target a specific vault          →  onebrain task list --vault ~/Documents/ob-1"
    );
    eprintln!("    set default vault for shell      →  export ONEBRAIN_VAULT=~/Documents/ob-1");
    eprintln!("    create a new vault here          →  onebrain init");
    eprintln!();
    eprintln!("  See: onebrain doctor · onebrain init --help");
}
