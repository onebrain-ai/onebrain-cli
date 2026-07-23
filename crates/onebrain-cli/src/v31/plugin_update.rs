//! `onebrain plugin update` — v3.1's renamed equivalent of v3.0's
//! `onebrain update`. Pulls the plugin tarball from GitHub, runs migrations,
//! rewrites `.claude/settings.json` hook entries to v3.1 paths, and re-runs
//! `schedule register` so launchd plists point at the new paths too.
//!
//! v3.1 split: `onebrain update` now means "self-update the CLI binary",
//! while `onebrain plugin update` is the plugin-side workflow.

use crate::commands::daemon::stop_slot;
use crate::commands::daemon_client::{
    self, own_version, version_decision, DaemonInfo, SlotResolve, VersionDecision,
};
use crate::v31::hook_rewriter::{self, RewriteWarning};
use anyhow::{Context, Result};
use onebrain_fs::read_plugin_version;
use onebrain_fs::register_hooks::settings::settings_path;
use std::path::PathBuf;

#[derive(Debug, Default)]
pub struct PluginUpdateReport {
    pub vault_synced: bool,
    pub hooks_rewritten: u32,
    pub plists_rewritten: bool,
    /// v3.2.13: count of launchd plists actually written this run. `None`
    /// means the step did not run (dry-run, or a pre-step bailed); `Some(0)`
    /// means the step ran but `onebrain.yml` had no `schedule:` entries to
    /// register (a well-formed no-op, not an error); `Some(N)` is the
    /// normal success case. Separated from `plists_rewritten` so the framed
    /// renderer can distinguish "skipped because dry-run" from "skipped
    /// because nothing to do" — previously both collapsed to a misleading
    /// `✓ launchd plists  done` row.
    pub plists_count: Option<u32>,
    /// v3.2.15: plugin version (`.claude-plugin/plugin.json::version`) BEFORE
    /// the vault-sync step. `None` means no plugin was installed yet (fresh
    /// install) OR the file/field was missing/malformed. Used by the framed
    /// renderer to label the vault-sync step with either `vX → vY` (real
    /// update), `vX · up-to-date` (no version change), or `installed vY`
    /// (fresh install).
    pub version_before: Option<String>,
    /// v3.2.15: plugin version AFTER the vault-sync step (i.e. the version
    /// the downloaded tarball ships). `None` means we couldn't read the
    /// post-sync `plugin.json` (sync failed, or the tarball didn't include
    /// the manifest — neither expected on the happy path). In dry-run the
    /// vault-sync step is skipped, so this stays equal to `version_before`.
    pub version_after: Option<String>,
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
    /// v3.4.15 (#291): `true` when step 5 stopped a LIVE version-skewed warm
    /// daemon for this vault (the user `brew upgrade`d then ran `plugin
    /// update`). `false` on the common path where the running daemon already
    /// matches our version, or none was running.
    pub daemon_retired: bool,
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

    // v3.2.15: capture the installed plugin version BEFORE sync. The renderer
    // uses (version_before, version_after) to label the vault-sync step row
    // as `vX → vY` (real update) / `vX · up-to-date` (no version change) /
    // `installed vY` (fresh install) — per user feedback, the pre-3.2.15
    // `done` / `skipped` collapse hid which version was just applied.
    report.version_before = read_plugin_version(&vault_root);

    // 2. Sync plugin tarball — same backend as v3.0 `vault-sync`.
    //    v3.2.13: invoke via the embedded-progress entry so the orchestrator
    //    skips its "OneBrain Vault Sync" intro frame and "vault-sync: done"
    //    outro.
    //    v3.2.15: ALSO route through `run_embedded` so the per-step `▸ <label>`
    //    TTY lines don't leak above the parent's framed report (user testing
    //    on v3.2.14 flagged "ไม่มี header เลย" — the step lines appeared
    //    BEFORE the parent's `🔄  Plugin Update` header). The framed report's
    //    animated spinner is the only progress signal, matching what
    //    `doctor`/`update` already do.
    if !dry_run {
        let exit = crate::commands::vault_sync::run_embedded(Some(vault_root.clone()), branch)
            .context("plugin update: vault-sync failed")?;
        // `vault_sync::run` returns Ok(0) on success, Ok(1) on critical fail.
        if exit != 0 {
            anyhow::bail!("plugin update: vault-sync returned exit code {exit}");
        }
        report.vault_synced = true;
        // Re-read after sync to capture the (possibly new) installed version.
        report.version_after = read_plugin_version(&vault_root);
    } else {
        // Dry-run skips the tarball fetch, so the "after" version is whatever
        // is currently on disk (== before). Renderer treats this as "no
        // change" and just shows the current version in the verdict.
        report.version_after = report.version_before.clone();
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
    //    v3.2.13: use the `run_embedded` entry so the per-plist `✓ Wrote …`
    //    confirmation lines and the trailing "Use launchctl to load …" hint
    //    don't leak through plugin update's framed report — those belong on
    //    the direct `onebrain schedule register` surface, not embedded.
    if !dry_run {
        match crate::commands::register_schedule::run_embedded(
            Some(vault_root.clone()),
            /* dry_run */ false,
            /* refresh */ true,
        ) {
            Ok(count) => {
                // v3.2.13: `plists_rewritten = true` ONLY when actual writes
                // happened. A vault without `schedule:` entries returns
                // `Ok(0)` — the step ran successfully but produced no work,
                // so the renderer surfaces "no schedule entries" instead of
                // a misleading "done".
                report.plists_rewritten = count > 0;
                report.plists_count = Some(count as u32);
            }
            Err(e) => {
                report.partial_failure = Some(format!(
                    "schedule re-register failed after hook rewrite: {e:#}"
                ));
            }
        }
    }

    // Refresh Codex only for vaults whose explicit managed-install marker is
    // present. A refresh failure is partial: vault sync has already succeeded
    // and must not be rolled back.
    if report.partial_failure.is_none() {
        match crate::commands::codex_plugin::refresh_if_managed(&vault_root, dry_run) {
            Ok(Some(code)) if code != 0 => {
                report.partial_failure = Some(format!(
                    "managed Codex plugin refresh exited with code {code}"
                ));
            }
            Err(error) => {
                report.partial_failure =
                    Some(format!("managed Codex plugin refresh failed: {error:#}"));
            }
            _ => {}
        }
    }

    // 5. Retire a version-skewed warm daemon for this vault (#291). A plugin
    //    update doesn't change the CLI binary, but the user may have
    //    `brew upgrade`d separately then run `plugin update` — the exact flow
    //    that hit #291. A stale-version daemon keeps serving the OLD wire shape
    //    (e.g. the empty gain route) until it idles out, so retire it now; the
    //    next call respawns one at our version. CONDITIONAL: a no-op when the
    //    running daemon already matches our version (no cold-start penalty on a
    //    normal plugin update). Partial-failure convention — the helper never
    //    surfaces an error: steps 2–4 already mutated disk, so a retire hiccup
    //    is absorbed, not surfaced as a failed update.
    if !dry_run {
        report.daemon_retired = retire_skewed_daemon(resolved.root.as_path());
    }

    Ok(report)
}

/// Step-5 core (#291): stop a LIVE version-skewed warm daemon for `vault_root`,
/// returning `true` when one was actually retired. Returns `false` — never an
/// error — when the running daemon already matches our version, none is
/// running, or any of the slot / record lookups fail (partial-failure
/// convention: a plugin update that already mutated disk must not fail on a
/// best-effort daemon cleanup).
fn retire_skewed_daemon(vault_root: &std::path::Path) -> bool {
    let Ok(SlotResolve::Slot { paths, .. }) = daemon_client::resolve_slot(Some(vault_root)) else {
        return false;
    };
    let Ok(Some(info)) = DaemonInfo::read(&paths.json) else {
        return false;
    };
    // CONDITIONAL: matching version → leave the warm daemon alone.
    if version_decision(&info.version, own_version()) != VersionDecision::Restart {
        return false;
    }
    // `stop_slot` returns `(stopped, _)`; `stopped` is true only when a LIVE
    // daemon was signalled. A stale on-disk record is just cleared — not a
    // retire.
    matches!(stop_slot(&paths), Ok((true, _)))
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
        assert!(r.plists_count.is_none());
        assert!(!r.dry_run);
        assert!(r.partial_failure.is_none());
        assert!(r.warnings.is_empty());
        assert!(!r.daemon_retired);
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
            plists_count: None,
            version_before: None,
            version_after: None,
            partial_failure: Some("schedule re-register failed: launchctl exit 1".to_string()),
            warnings: Vec::new(),
            daemon_retired: false,
        };
        assert_eq!(r.hooks_rewritten, 3);
        assert!(r.vault_synced);
        assert!(!r.plists_rewritten);
        assert!(r.partial_failure.as_deref().unwrap().contains("launchctl"));
    }

    #[test]
    fn dry_run_with_existing_plugin_version_mirrors_version_before_to_after() {
        // When dry_run=true the vault-sync tarball fetch is skipped. The
        // `report.version_after = report.version_before.clone()` line must be
        // exercised when a plugin.json already exists (version_before = Some).
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("vault.yml"), "method: onebrain\n").unwrap();
        let claude = root.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        fs::write(claude.join("settings.json"), "{}").unwrap();
        // Write plugin.json so read_plugin_version returns Some("3.2.15").
        let plugin_dir = root
            .join(".claude")
            .join("plugins")
            .join("onebrain")
            .join(".claude-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join("plugin.json"), r#"{"version":"3.2.15"}"#).unwrap();
        let report = run(Some(root.to_path_buf()), None, true).unwrap();
        assert_eq!(report.version_before.as_deref(), Some("3.2.15"));
        // In dry-run, version_after must mirror version_before (not a post-sync read).
        assert_eq!(
            report.version_after, report.version_before,
            "dry-run must set version_after = version_before"
        );
        assert!(
            !report.vault_synced,
            "vault_synced must be false in dry-run"
        );
        assert!(
            report.plists_count.is_none(),
            "plists_count must be None in dry-run (schedule step skipped)"
        );
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

    // ── #291: step-5 warm-daemon retire (`retire_skewed_daemon`) ──────────
    // Unix-only: isolates the daemon run dir via `$HOME`, which
    // `dirs::home_dir()` honours only on Unix (the repo-wide convention for
    // HOME-based daemon tests). A real short-lived child process stands in for
    // the running daemon so `stop_slot`'s liveness probe + SIGTERM exercise
    // for real.

    /// Spawn a long-lived child to stand in for a running daemon. Killed +
    /// reaped by the caller's cleanup. `process_group(0)` puts it in its OWN
    /// process group (pgid == pid) so `daemon::is_alive`'s group-leader check
    /// (the real daemon runs under `setsid()`) treats it as a live daemon.
    #[cfg(unix)]
    fn spawn_fake_daemon_process() -> std::process::Child {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn `sleep` as a fake live daemon")
    }

    /// Plant a per-vault daemon slot (`daemon-<hash>.json` + `.pid`) under
    /// `$HOME/.onebrain/run/` for `vault`, stamped `version` and pointing at
    /// `pid` — the record `retire_skewed_daemon` reads.
    #[cfg(unix)]
    fn plant_slot(vault: &std::path::Path, version: &str, pid: u32) {
        let SlotResolve::Slot { paths, .. } = daemon_client::resolve_slot(Some(vault)).unwrap()
        else {
            panic!("vault slot must resolve for a real tempdir");
        };
        let info = DaemonInfo {
            port: 1, // retire never probes health — pid liveness is what matters
            token: "x".repeat(20),
            pid,
            version: version.to_string(),
            vault: daemon_client::canonical_vault_id(vault),
        };
        info.write(&paths.json).unwrap(); // also creates the 0700 run dir
        std::fs::write(&paths.pid, format!("{pid}\n")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn retire_skewed_daemon_stops_live_skewed_daemon() {
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_var("HOME", home.path());
        let vault = tempfile::tempdir().unwrap();
        let mut child = spawn_fake_daemon_process();
        // A version DIFFERENT from ours — the post-`brew upgrade` skew.
        plant_slot(vault.path(), "0.0.1", child.id());

        assert!(
            retire_skewed_daemon(vault.path()),
            "a live version-skewed daemon must be retired (flag true)"
        );

        // `stop_slot` SIGTERM'd the child — reap it to avoid a lingering zombie.
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn retire_skewed_daemon_leaves_matching_version_untouched() {
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_var("HOME", home.path());
        let vault = tempfile::tempdir().unwrap();
        let mut child = spawn_fake_daemon_process();
        // SAME version as ours → conditional no-op, daemon left running.
        plant_slot(vault.path(), env!("CARGO_PKG_VERSION"), child.id());

        assert!(
            !retire_skewed_daemon(vault.path()),
            "a version-matching daemon must be left running (flag false)"
        );

        // The daemon was NOT stopped — it's still our child; clean it up.
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn retire_skewed_daemon_false_when_no_record() {
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_var("HOME", home.path());
        let vault = tempfile::tempdir().unwrap();
        // No slot json planted → nothing running → nothing retired.
        assert!(!retire_skewed_daemon(vault.path()));
    }
}
