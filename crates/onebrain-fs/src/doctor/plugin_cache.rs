//! `plugin-cache` doctor check — flags stale OneBrain plugin versions left in
//! the Claude Code marketplace cache (`~/.claude/plugins/cache/<mkt>/onebrain/`).
//!
//! Why this exists: OneBrain is pinned to the vault-local copy
//! (`<vault>/.claude/plugins/onebrain`), so any version dir under the
//! marketplace cache is an orphan. An orphan can shadow the vault-local copy
//! and make Claude Code load *old* skills (the 2.2.4-vs-3.1.3 split-brain).
//! vault-sync already prunes these (Step 9), but only while it runs — an orphan
//! created by a later `claude plugin install` survives until the next
//! `onebrain plugin update`. This check surfaces that drift any time `doctor`
//! runs, and `--fix` prunes it via the shared `clean_plugin_cache`.

use crate::doctor::Check;
use crate::vault_sync::cache_clean::detect_stale_plugin_cache;
use crate::vault_sync::default_installed_plugins_path;
use onebrain_core::{DoctorResult, VaultConfig};
use std::path::Path;

pub struct PluginCacheCheck;

/// Core logic, separated from `Check::run` so tests can pass explicit temp
/// paths instead of poking process-global env vars (which race under parallel
/// `cargo test`). `run` is the thin shell that resolves the real home paths.
///
/// `cache_dir = None` lets `detect_stale_plugin_cache` derive `<installed dir>/cache`.
fn check_plugin_cache(installed_plugins_path: &Path, cache_dir: Option<&Path>) -> DoctorResult {
    let stale = detect_stale_plugin_cache(installed_plugins_path, cache_dir);

    if stale.is_empty() {
        return DoctorResult::ok("plugin-cache", "no stale plugin cache");
    }

    // One detail line per orphan, e.g. "stale: onebrain/2.2.4".
    let details: Vec<String> = stale
        .iter()
        .map(|e| format!("stale: {}/{}", e.marketplace, e.version))
        .collect();

    let msg = format!(
        "{} stale cached plugin version{}",
        stale.len(),
        if stale.len() == 1 { "" } else { "s" }
    );
    // Mentioning "--fix" marks this auto-fixable (the doctor command treats a
    // hint containing "doctor --fix" as a recipe it can run after confirmation).
    DoctorResult::warn("plugin-cache", msg)
        .with_hint(
            "Run onebrain doctor --fix to remove the stale plugin cache \
             (or onebrain plugin update); restart / run /reload-plugins after",
        )
        .with_details(details)
}

impl Check for PluginCacheCheck {
    fn name(&self) -> &'static str {
        "plugin-cache"
    }

    fn run(&self, _vault_root: &Path, _config: &VaultConfig) -> DoctorResult {
        // The cache lives under $HOME, not the vault — so `vault_root` is unused
        // here (unlike every other check). Resolve home ourselves.
        let Some(installed) = default_installed_plugins_path() else {
            return DoctorResult::ok("plugin-cache", "home dir unresolved — skipped");
        };
        // `None` → detect derives `<home>/.claude/plugins/cache`.
        check_plugin_cache(&installed, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn no_cache_reports_ok() {
        let d = tempdir().unwrap();
        let installed = d.path().join("installed_plugins.json");
        fs::write(&installed, r#"{"plugins":{}}"#).unwrap();
        // cache dir does not exist → detect returns empty → ok.
        let r = check_plugin_cache(&installed, Some(&d.path().join("cache")));
        assert_eq!(r.message, "no stale plugin cache");
    }

    #[test]
    fn orphan_version_reports_warn_with_fix_hint() {
        let d = tempdir().unwrap();
        let cache = d.path().join("cache");
        // Reproduce the live bug: orphan onebrain/onebrain/2.2.4.
        fs::create_dir_all(cache.join("onebrain/onebrain/2.2.4")).unwrap();
        let installed = d.path().join("installed_plugins.json");
        fs::write(&installed, r#"{"plugins":{}}"#).unwrap();

        let r = check_plugin_cache(&installed, Some(&cache));
        assert_eq!(r.message, "1 stale cached plugin version");
        assert!(r.hint.as_deref().unwrap().contains("doctor --fix"));
        assert!(r.details.iter().any(|s| s.contains("onebrain/2.2.4")));
    }

    #[test]
    fn pluralizes_multiple_orphans() {
        let d = tempdir().unwrap();
        let cache = d.path().join("cache");
        fs::create_dir_all(cache.join("mkt/onebrain/2.2.4")).unwrap();
        fs::create_dir_all(cache.join("mkt/onebrain/3.0.0")).unwrap();
        let installed = d.path().join("installed_plugins.json");
        fs::write(
            &installed,
            r#"{"plugins":{"onebrain@mkt":[{"id":"onebrain"}]}}"#,
        )
        .unwrap();

        let r = check_plugin_cache(&installed, Some(&cache));
        assert_eq!(r.message, "2 stale cached plugin versions");
    }
}
