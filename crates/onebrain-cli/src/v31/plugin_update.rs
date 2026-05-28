//! `onebrain plugin update` — v3.1's renamed equivalent of v3.0's
//! `onebrain update`. Pulls the plugin tarball from GitHub, runs migrations,
//! rewrites `.claude/settings.json` hook entries to v3.1 paths, and re-runs
//! `schedule register` so launchd plists point at the new paths too.
//!
//! v3.1 split: `onebrain update` now means "self-update the CLI binary",
//! while `onebrain plugin update` is the plugin-side workflow.

use crate::v31::hook_rewriter::{self, RewriteWarning};
use anyhow::{Context, Result};
use onebrain_fs::register_hooks::settings::settings_path;
use std::path::PathBuf;

#[derive(Debug, Default)]
pub struct PluginUpdateReport {
    pub vault_synced: bool,
    pub hooks_rewritten: u32,
    pub plists_rewritten: bool,
    pub dry_run: bool,
    /// When `Some(reason)`, the run failed midway. Fields above reflect
    /// whatever progress was made before the failure. The caller emits a
    /// partial-report envelope with `ok: false` + error code
    /// `E_PLUGIN_UPDATE_PARTIAL`.
    pub partial_failure: Option<String>,
    /// Soft warnings surfaced by the hook rewriter (e.g.
    /// `W_MALFORMED_HOOK_ENTRY` for entries the rewriter had to skip). The
    /// dispatcher appends these to the envelope's `warnings[]` array so
    /// machine consumers and humans both see them. R2-H1: the rewriter has
    /// always populated these, but they used to be dropped on the floor
    /// before reaching the user.
    pub warnings: Vec<RewriteWarning>,
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
///
/// Partial-failure model: when step 3 succeeds but step 4 fails the
/// returned report's `partial_failure` field is `Some(reason)`, the success
/// fields reflect actual on-disk progress, and the function returns
/// `Ok(report)` so the dispatcher can render the partial envelope. Earlier
/// step failures (1, 2) bubble up as `Err`, since no on-disk state has
/// changed yet from this command.
pub fn run(
    vault_dir: Option<PathBuf>,
    branch: Option<String>,
    dry_run: bool,
) -> Result<PluginUpdateReport> {
    let mut report = PluginUpdateReport {
        dry_run,
        ..Default::default()
    };

    // 1. Resolve vault — vault-required, errors with exit 64 when not found.
    let resolved = crate::vault_ctx::require(vault_dir.clone())?;
    let vault_root = resolved.root.as_path().to_path_buf();

    // 2. Sync plugin tarball — same backend as v3.0 `vault-sync`.
    //    v3.2.13: invoke via the embedded-progress entry so the orchestrator
    //    skips its "OneBrain Vault Sync" intro frame and "vault-sync: done"
    //    outro — those are redundant under plugin update's own framed report
    //    and were a key part of the "weird" mixed-styles UX the user flagged.
    //    Step spinners still emit (they're transient) so the user sees
    //    download/sync activity during a long fetch.
    if !dry_run {
        let exit = crate::commands::vault_sync::run_embedded(Some(vault_root.clone()), branch)
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
    // R2-H1: surface any soft warnings from the rewriter (e.g. malformed
    // hook entries that were skipped). Previously these were silently
    // dropped before reaching the envelope.
    report.warnings = rewrite_report.warnings;

    // 4. Re-register launchd plists.
    //    register_schedule's `--refresh` flag re-emits plists with the
    //    current vault path. It's idempotent. A failure here leaves hooks
    //    pointing at v3.1 paths while plists may still reference v3.0 —
    //    we surface this as a partial failure rather than bubbling Err, so
    //    the dispatcher can render the partial state in the envelope.
    //
    //    v3.2.13: use the `run_quiet` entry so the per-plist `✓ Wrote …`
    //    confirmation lines and the trailing "Use launchctl to load …" hint
    //    don't leak through plugin update's framed report — those belong on
    //    the direct `onebrain schedule register` surface, not embedded.
    if !dry_run {
        match crate::commands::register_schedule::run_quiet(
            Some(vault_root),
            /* dry_run */ false,
            /* refresh */ true,
        ) {
            Ok(()) => report.plists_rewritten = true,
            Err(e) => {
                report.partial_failure = Some(format!(
                    "schedule re-register failed after hook rewrite: {e:#}"
                ));
            }
        }
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
        assert!(r.partial_failure.is_none());
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn partial_failure_field_is_settable_and_preserves_progress() {
        // R1 B3: when step 4 (schedule re-register) fails after step 3
        // (hook rewriter) succeeded, the report MUST preserve the
        // hooks_rewritten count so the user knows what's already on disk.
        let r = PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 3,
            plists_rewritten: false,
            partial_failure: Some("schedule re-register failed: launchctl exit 1".to_string()),
            warnings: Vec::new(),
        };
        assert_eq!(r.hooks_rewritten, 3);
        assert!(r.vault_synced);
        assert!(!r.plists_rewritten);
        assert!(r.partial_failure.as_deref().unwrap().contains("launchctl"));
    }

    #[test]
    fn rewriter_warnings_plumbed_into_report() {
        // R2-H1: a malformed `.claude/settings.json` hook entry should
        // surface as a W_MALFORMED_HOOK_ENTRY warning inside the report's
        // `warnings` vec, so the dispatcher can append it to the envelope.
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("vault.yml"), "method: onebrain\n").unwrap();
        let claude = dir.path().join(".claude");
        fs::create_dir_all(&claude).unwrap();
        // Malformed: `command` is an array (skill-alignment §4.5 expects a
        // string). The rewriter must skip it and emit W_MALFORMED_HOOK_ENTRY.
        let body = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            { "type": "command", "command": ["onebrain", "session-init"] }
                        ]
                    }
                ]
            }
        });
        fs::write(
            claude.join("settings.json"),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();
        // dry_run=true to avoid the network vault-sync + schedule-register.
        let report = run(Some(dir.path().to_path_buf()), None, true).unwrap();
        assert_eq!(
            report.warnings.len(),
            1,
            "expected one rewriter warning to plumb through to the report"
        );
        assert_eq!(report.warnings[0].code, "W_MALFORMED_HOOK_ENTRY");
    }
}
