//! `onebrain vault-sync` — pull the upstream `onebrain-ai/onebrain` release
//! tarball and overlay the bundled plugin / harness / docs onto the current
//! vault. Mirrors Bun's `vaultSyncCommand`: exit 1 on `!result.ok`, otherwise 0.

use crate::safety::refuse_dangerous_vault_path;
use anyhow::{anyhow, Context, Result};
use onebrain_core::find_vault_root;
use onebrain_fs::{run_vault_sync, VaultSyncOptions};
use std::env;
use std::io;
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
    run_with(vault_root_override, branch, false)
}

/// Same as [`run`] but routes the per-step progress reporter to
/// `std::io::sink()` — so neither the "OneBrain Vault Sync" intro/outro
/// frame nor the per-step `▸ <label>` lines leak. This is the entry point
/// `onebrain plugin update` (v3.2.15+) calls to embed vault-sync inside its
/// own framed doctor-style report; the framed report's animated spinner is
/// the only progress signal the user sees, matching `doctor`/`update`. The
/// `register_schedule` sibling shares the `run_embedded` name (v3.2.18) so
/// both plugin-update entry points read consistently.
///
/// (History: v3.2.13 had a separate intro-only-suppression variant under this
/// name; v3.2.15 folded it into the fully-silent path because no remaining
/// caller wanted the in-between "embedded but with step lines" mode, and
/// round-2 dropped the now-dead `embedded` flag from the inner shape — the
/// orchestrator only consults it when `progress_writer` is `None`, and this
/// path always sets the writer to a sink.)
pub fn run_embedded(vault_root_override: Option<PathBuf>, branch: Option<String>) -> Result<i32> {
    run_with(vault_root_override, branch, true)
}

fn run_with(
    vault_root_override: Option<PathBuf>,
    branch: Option<String>,
    silent: bool,
) -> Result<i32> {
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

    // `silent` overrides any TTY-vs-non-TTY heuristic: orchestrator routes
    // `progress_writer = Some(io::sink())` through `PlainProgress`, which
    // discards every step line. The orchestrator's `embedded` flag only
    // affects the `progress_writer = None` branch, so leaving it at default
    // (`false`) when silent is fine — the sink doesn't care which mode
    // claims to write to it.
    let progress_writer: Option<Box<dyn io::Write + Send>> = if silent {
        Some(Box::new(io::sink()))
    } else {
        None
    };
    let result = run_vault_sync(
        vault_root_path.as_path(),
        VaultSyncOptions {
            branch,
            progress_writer,
            ..VaultSyncOptions::default()
        },
    );

    if !result.ok {
        if let Some(err) = result.error.as_ref() {
            eprintln!(
                "✗ vault-sync: failed: {err}\n\
                 💡 check your network connection, then retry `onebrain vault-sync`; run \
                 `onebrain doctor` afterward if you're unsure whether the sync partially applied"
            );
        } else {
            // No structured error captured — still surface non-zero with a
            // generic hint so schedulers/CI can see something happened.
            eprintln!(
                "✗ vault-sync: failed (no error detail captured)\n\
                 💡 retry `onebrain vault-sync`; if it keeps failing, run `onebrain doctor` to \
                 check the vault's overall health"
            );
        }
        return Ok(1);
    }
    Ok(0)
}
