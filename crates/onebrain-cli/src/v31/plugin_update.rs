//! `onebrain plugin update` — v3.1's renamed equivalent of v3.0's
//! `onebrain update`. Pulls the plugin tarball from GitHub, runs migrations,
//! rewrites `.claude/settings.json` hook entries to v3.1 paths, and re-runs
//! `schedule register` so launchd plists point at the new paths too.
//!
//! v3.1 split: `onebrain update` now means "self-update the CLI binary",
//! while `onebrain plugin update` is the plugin-side workflow.

use crate::v31::hook_rewriter;
use anyhow::{Context, Result};
use onebrain_core::find_vault_root;
use onebrain_fs::register_hooks::settings::settings_path;
use std::path::PathBuf;

#[derive(Debug, Default)]
pub struct PluginUpdateReport {
    pub vault_synced: bool,
    pub hooks_rewritten: u32,
    pub plists_rewritten: bool,
    pub dry_run: bool,
}

/// Run the v3.1 plugin update workflow.
///
/// Steps:
/// 1. Resolve vault (required).
/// 2. Pull plugin tarball via the existing `vault-sync` orchestrator.
/// 3. Rewrite `.claude/settings.json` hook args (v3.0 → v3.1).
/// 4. Re-run `schedule register` to refresh launchd plists.
///
/// Idempotent — running twice does not duplicate hook entries or plists.
pub fn run(
    vault_dir: Option<PathBuf>,
    branch: Option<String>,
    dry_run: bool,
) -> Result<PluginUpdateReport> {
    let mut report = PluginUpdateReport {
        dry_run,
        ..Default::default()
    };

    // 1. Resolve vault.
    let cwd = std::env::current_dir().context("read current directory")?;
    let resolved = crate::vault_ctx::resolve(vault_dir.clone())?.unwrap_or_else(|| {
        // Tolerant: fall back to walking from cwd one more time so the
        // error message is consistent with vault-required commands.
        // In practice resolve() already does this — falling through here
        // means truly no vault, so synthesize a not-found result.
        onebrain_core::ResolvedVault {
            root: find_vault_root(&cwd).unwrap_or_else(|| {
                panic!("plugin update requires a vault — none found from {cwd:?}");
            }),
            source: onebrain_core::VaultSource::WalkUp,
        }
    });
    let vault_root = resolved.root.as_path().to_path_buf();

    // 2. Sync plugin tarball — same backend as v3.0 `vault-sync`.
    if !dry_run {
        let exit = crate::commands::vault_sync::run(Some(vault_root.clone()), branch)
            .context("plugin update: vault-sync failed")?;
        // `vault_sync::run` returns Ok(0) on success, Ok(1) on critical fail.
        if exit != 0 {
            anyhow::bail!("plugin update: vault-sync returned exit code {exit}");
        }
        report.vault_synced = true;
    }

    // 3. Rewrite hook entries.
    let settings = settings_path(&vault_root);
    let rewrite_report = hook_rewriter::rewrite_settings_file(&settings, dry_run)
        .context("plugin update: hook rewrite failed")?;
    report.hooks_rewritten = rewrite_report.total;

    // 4. Re-register launchd plists.
    //    register_schedule's `--refresh` flag re-emits plists with the
    //    current vault path. It's idempotent.
    if !dry_run {
        crate::commands::register_schedule::run(
            Some(vault_root),
            /* dry_run    */ false,
            /* remove     */ false,
            /* refresh    */ true,
            /* resume     */ None,
            /* status     */ false,
            /* test       */ None,
        )
        .context("plugin update: schedule re-register failed")?;
        report.plists_rewritten = true;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The full happy-path test lives in
    // `tests/v31_integration.rs` — it needs a temp vault with a real
    // `.claude/settings.json` and stubs the vault-sync network step. Here we
    // verify just the report's defaults so refactors don't silently change
    // the shape consumed by the dispatcher.
    #[test]
    fn report_defaults() {
        let r = PluginUpdateReport::default();
        assert!(!r.vault_synced);
        assert_eq!(r.hooks_rewritten, 0);
        assert!(!r.plists_rewritten);
        assert!(!r.dry_run);
    }
}
