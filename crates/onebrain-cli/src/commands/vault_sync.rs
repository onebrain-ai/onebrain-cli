//! `onebrain vault-sync` — pull the upstream `onebrain-ai/onebrain` release
//! tarball and overlay the bundled plugin / harness / docs onto the current
//! vault. Mirrors Bun's `vaultSyncCommand`: exit 1 on `!result.ok`, otherwise 0.

use crate::safety::refuse_dangerous_vault_path;
use anyhow::{anyhow, Context, Result};
use onebrain_core::find_vault_root;
use onebrain_fs::{run_vault_sync, VaultSyncOptions};
use std::env;
use std::path::PathBuf;

/// Entry point — returns `Ok(0)` on success, `Ok(1)` on any critical failure.
/// Side-effects: writes to the vault filesystem; prints `vault-sync: …` lines
/// to stdout in non-TTY mode (Bun parity).
///
/// `branch` mirrors Bun v2.3.3's `--branch <branch>` CLI flag and overrides the
/// branch resolution that otherwise reads `vault.yml::update_channel`. When
/// `None`, the orchestrator falls back to `resolve_branch(update_channel)`.
///
/// The orchestrator already prints one of `vault-sync: download failed: …` /
/// `vault-sync: plugin sync failed: …` / `vault-sync: harness merge failed:
/// …` / `vault-sync: vault.yml update failed: …` to stderr on every known
/// failure path, so the handler keeps its own output minimal (Bun's
/// `vaultSyncCommand` does `process.exit(1)` with no extra logging). We
/// still re-print `result.error` as a defensive backstop for any future
/// failure path that sets `result.error` without logging to stderr — better
/// to occasionally duplicate one line than leave the user with a silent
/// non-zero exit.
pub fn run(vault_root_override: Option<PathBuf>, branch: Option<String>) -> Result<i32> {
    // Bun v2.3.3 parity: optional positional `<vault_root>` lets the caller
    // supply the vault path directly (skipping the cwd walk-up). When absent,
    // walk up from cwd as before.
    let vault_root_path = match vault_root_override {
        Some(p) => p,
        None => {
            let cwd = env::current_dir().context("read current directory")?;
            find_vault_root(&cwd)
                .ok_or_else(|| anyhow!("not inside a vault (no onebrain.yml or vault.yml found)"))?
                .as_path()
                .to_path_buf()
        }
    };

    // Safety guard — vault-sync writes destructively into the target directory
    // (`.claude/plugins/onebrain/`, root markdown files, etc.). Refusing the
    // obvious foot-cannons (`/`, `$HOME`) prevents accidental splattering when
    // a user runs `onebrain vault-sync ~` by mistake. The list is deliberately
    // narrow — any other path is the caller's responsibility.
    refuse_dangerous_vault_path(&vault_root_path)?;

    let result = run_vault_sync(
        vault_root_path.as_path(),
        VaultSyncOptions {
            branch,
            ..VaultSyncOptions::default()
        },
    );

    if !result.ok {
        if let Some(err) = result.error.as_ref() {
            eprintln!("vault-sync: failed: {err}");
        } else {
            // No structured error captured — still surface non-zero with a
            // generic hint so schedulers/CI can see something happened.
            eprintln!("vault-sync: failed (no error detail captured)");
        }
        return Ok(1);
    }
    Ok(0)
}
