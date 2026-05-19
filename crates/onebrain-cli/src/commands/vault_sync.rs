//! `onebrain vault-sync` — pull the upstream `onebrain-ai/onebrain` release
//! tarball and overlay the bundled plugin / harness / docs onto the current
//! vault. Mirrors Bun's `vaultSyncCommand`: exit 1 on `!result.ok`, otherwise 0.

use anyhow::{anyhow, Context, Result};
use onebrain_core::find_vault_root;
use onebrain_fs::{run_vault_sync, VaultSyncOptions};
use std::env;

/// Entry point — returns `Ok(0)` on success, `Ok(1)` on any critical failure.
/// Side-effects: writes to the vault filesystem; prints `vault-sync: …` lines
/// to stdout in non-TTY mode (Bun parity).
pub fn run() -> Result<i32> {
    let cwd = env::current_dir().context("read current directory")?;
    let vault_root =
        find_vault_root(&cwd).ok_or_else(|| anyhow!("not inside a vault (no vault.yml found)"))?;

    let result = run_vault_sync(vault_root.as_path(), VaultSyncOptions::default());

    if !result.ok {
        if let Some(err) = result.error.as_ref() {
            eprintln!("vault-sync: failed: {err}");
        }
        return Ok(1);
    }
    Ok(0)
}
