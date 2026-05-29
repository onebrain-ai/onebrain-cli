//! Step 9 · detect + clean obsolete OneBrain cache versions under
//! `<cacheDir>/<marketplace>/onebrain/`. `clean_plugin_cache` ports Bun's
//! `cleanPluginCache` (non-fatal — returns a [`CacheCleanOutcome`] of the
//! successful + failed removals); `detect_stale_plugin_cache` is the read-only
//! sibling added for the `doctor` plugin-cache check.
//!
//! Two public entry points share ONE discovery + enumeration pass:
//! - [`detect_stale_plugin_cache`] — **read-only**; lists the orphan version
//!   dirs that exist. Used by `onebrain doctor` to *report* drift.
//! - [`clean_plugin_cache`] — **destructive**; removes those same dirs. Used by
//!   vault-sync (Step 9) and `onebrain doctor --fix`.
//!
//! Both walk the identical set of dirs (`discover_onebrain_cache_dirs` →
//! `version_dirs_under`), so detection and removal can only diverge on the
//! *removal step itself*: a `remove_dir_all` that fails (permission, race) is
//! counted in `CacheCleanOutcome::failed` (not `removed`), so `clean`'s
//! `removed` count may be **less than** `detect`'s "found" when a removal
//! fails. Callers that need to confirm the cache is actually clean should
//! re-run `detect_stale_plugin_cache` after.
//!
//! Why removing *every* cached version dir is safe: when OneBrain is pinned to
//! the vault-local copy (`installPath` → `<vault>/.claude/plugins/onebrain`),
//! the marketplace cache holds only *stale* copies — none of them is the active
//! plugin. So any version dir under the cache is an orphan we can prune.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// One stale cached version found under `<cache>/<marketplace>/onebrain/<version>`.
/// Returned by [`detect_stale_plugin_cache`] so the doctor report can name the
/// marketplace + version without re-walking the tree. `pub` (not `pub(crate)`)
/// because the CLI `doctor --fix` recipe re-runs detection across the crate
/// boundary to confirm the sweep actually emptied the cache before reporting
/// success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleCacheEntry {
    /// Marketplace directory name (e.g. `"onebrain"` for `onebrain@onebrain`).
    pub marketplace: String,
    /// Version directory name (e.g. `"2.2.4"`).
    pub version: String,
}

/// Reject a marketplace name that could escape the cache root when joined.
/// `installed_plugins.json` keys are normally well-formed (`onebrain@<mkt>`),
/// but a corrupted/hostile key like `onebrain@../../etc` would make
/// `cache_dir.join(marketplace)` resolve OUTSIDE the cache tree — and this is
/// the only path that feeds an attacker-influenced string into a
/// `remove_dir_all` target. Defense in depth: drop anything with a path
/// separator or a `..` component.
fn is_safe_marketplace(name: &str) -> bool {
    !name.is_empty() && name != ".." && !name.contains('/') && !name.contains('\\')
}

/// Resolve the `<cache>` root. Callers may pass an explicit dir (tests +
/// env-override); otherwise we derive `<installed_plugins.json dir>/cache`,
/// falling back to a bare `cache` if the path has no parent.
///
/// Returns `None` when the resolved cache dir does not exist — both public
/// entry points treat that as "nothing to do".
fn resolve_cache_dir(
    installed_plugins_path: &Path,
    installed_plugins_cache_dir: Option<&Path>,
) -> Option<PathBuf> {
    let cache_dir = installed_plugins_cache_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            installed_plugins_path
                .parent()
                .map(|p| p.join("cache"))
                .unwrap_or_else(|| PathBuf::from("cache"))
        });
    // `.exists()` here keeps both entry points cheap when no cache exists.
    cache_dir.exists().then_some(cache_dir)
}

/// Discover every `<cache>/<marketplace>/onebrain/` directory.
///
/// Unions two sources (deduped):
/// 1. Marketplaces registered in `installed_plugins.json` (keys shaped
///    `onebrain@<marketplace>`), sanitized via [`is_safe_marketplace`].
/// 2. An UNCONDITIONAL glob of `<cache>/*/onebrain/`, so an orphan under an
///    unregistered marketplace is found even when a registered marketplace also
///    exists. The glob only ever joins real `read_dir` entries, so it cannot
///    traverse outside the cache.
fn discover_onebrain_cache_dirs(installed_plugins_path: &Path, cache_dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // `if let Ok(..) = ..` quietly skips a missing/unreadable file — discovery
    // is best-effort, never an error.
    if let Ok(text) = fs::read_to_string(installed_plugins_path) {
        if let Ok(data) = serde_json::from_str::<Value>(&text) {
            if let Some(plugins) = data.get("plugins").and_then(Value::as_object) {
                for key in plugins.keys() {
                    // Only keys like `onebrain@<marketplace>` are ours.
                    if !key.starts_with("onebrain@") {
                        continue;
                    }
                    let marketplace = key.split('@').nth(1).unwrap_or("");
                    if !is_safe_marketplace(marketplace) {
                        continue;
                    }
                    let candidate = cache_dir.join(marketplace).join("onebrain");
                    if candidate.exists() {
                        dirs.push(candidate);
                    }
                }
            }
        }
    }

    // ALWAYS also glob the cache: covers orphans whose marketplace is no longer
    // registered in installed_plugins.json (the 2.2.4 case) EVEN WHEN a
    // registered marketplace also exists. Previously gated on `dirs.is_empty()`,
    // which missed an unregistered orphan whenever any registered marketplace
    // was present.
    if let Ok(read) = fs::read_dir(cache_dir) {
        // `.flatten()` drops `Err` entries (e.g. a racing unlink) and yields
        // only the readable `DirEntry`s.
        for entry in read.flatten() {
            let candidate = entry.path().join("onebrain");
            if candidate.exists() {
                dirs.push(candidate);
            }
        }
    }

    // A registered marketplace also surfaces in the glob — dedup so each
    // `.../onebrain/` dir is enumerated once.
    dirs.sort();
    dirs.dedup();

    dirs
}

/// Enumerate the version directories directly under one `.../onebrain/` dir.
/// Shared by detect + clean so both operate on the *identical* set (the only
/// divergence is the removal step). Uses `symlink_metadata` (does NOT follow
/// links) so a symlinked entry is classified as a symlink and skipped — never
/// recursed into by a later `remove_dir_all`. Cache dirs should never contain
/// symlinks; this is defensive given the recursive delete.
fn version_dirs_under(plugin_dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let Ok(read) = fs::read_dir(plugin_dir) else {
        return dirs;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            dirs.push(path);
        }
    }
    dirs
}

/// **Read-only.** List the stale cached version dirs that exist, so `doctor`
/// can report drift (cache vs the pinned vault-local copy) and offer `--fix`.
/// Returns an empty vec when the cache is absent or already clean.
pub fn detect_stale_plugin_cache(
    installed_plugins_path: &Path,
    installed_plugins_cache_dir: Option<&Path>,
) -> Vec<StaleCacheEntry> {
    let Some(cache_dir) = resolve_cache_dir(installed_plugins_path, installed_plugins_cache_dir)
    else {
        return Vec::new();
    };

    let mut stale: Vec<StaleCacheEntry> = Vec::new();
    for plugin_dir in discover_onebrain_cache_dirs(installed_plugins_path, &cache_dir) {
        // `<cache>/<marketplace>/onebrain` → the marketplace name is the parent
        // of `onebrain`; recover it for the report.
        let marketplace = plugin_dir
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        for path in version_dirs_under(&plugin_dir) {
            if let Some(version) = path.file_name().map(|s| s.to_string_lossy().into_owned()) {
                stale.push(StaleCacheEntry {
                    marketplace: marketplace.clone(),
                    version,
                });
            }
        }
    }
    stale
}

/// Outcome of a destructive cache sweep. `removed` + `failed` together account
/// for every version dir discovered, so a caller can report partial success
/// instead of silently dropping failures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheCleanOutcome {
    pub removed: u64,
    pub failed: u64,
}

/// **Destructive.** Step 9 entry point + `doctor --fix` worker. Removes every
/// version dir under the discovered onebrain cache dirs. Returns a
/// [`CacheCleanOutcome`] whose `removed` + `failed` together account for every
/// version dir found, so `removed` may be less than [`detect_stale_plugin_cache`]
/// found when a removal fails. Non-fatal — a failed removal is counted in
/// `failed` (and an stderr warning is emitted), never silently swallowed.
pub fn clean_plugin_cache(
    installed_plugins_path: &Path,
    installed_plugins_cache_dir: Option<&Path>,
) -> CacheCleanOutcome {
    let Some(cache_dir) = resolve_cache_dir(installed_plugins_path, installed_plugins_cache_dir)
    else {
        return CacheCleanOutcome::default();
    };

    let mut out = CacheCleanOutcome::default();
    for plugin_dir in discover_onebrain_cache_dirs(installed_plugins_path, &cache_dir) {
        for path in version_dirs_under(&plugin_dir) {
            // `remove_dir_all` is recursive; the failure branch counts + warns so
            // a permission error on one dir doesn't abort the rest and isn't lost.
            match fs::remove_dir_all(&path) {
                Ok(()) => out.removed += 1,
                Err(e) => {
                    out.failed += 1;
                    // Surface, don't swallow: a stale cache dir we couldn't
                    // remove will keep shadowing the vault-local plugin.
                    eprintln!(
                        "vault-sync: failed to remove stale cache dir {}: {e}",
                        path.display()
                    );
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_json(p: &Path, v: &serde_json::Value) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, serde_json::to_string_pretty(v).unwrap()).unwrap();
    }

    #[test]
    fn typical_layout_removes_version_dirs() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let v_a = cache_dir.join("marketplace/onebrain/1.10.0");
        let v_b = cache_dir.join("marketplace/onebrain/1.10.1");
        fs::create_dir_all(&v_a).unwrap();
        fs::create_dir_all(&v_b).unwrap();
        let installed = dir.path().join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({
                "plugins": {
                    "onebrain@marketplace": [{"id":"onebrain"}]
                }
            }),
        );
        let out = clean_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(out.removed, 2);
        assert_eq!(out.failed, 0);
        assert!(!v_a.exists());
        assert!(!v_b.exists());
    }

    #[test]
    fn missing_cache_dir_returns_zero() {
        let dir = tempdir().unwrap();
        let installed = dir.path().join("installed_plugins.json");
        fs::write(&installed, "{}").unwrap();
        let out = clean_plugin_cache(&installed, Some(&dir.path().join("nonexistent-cache")));
        assert_eq!(out.removed, 0);
        assert_eq!(out.failed, 0);
    }

    #[test]
    fn fallback_glob_when_installed_plugins_unreadable() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let v = cache_dir.join("anything/onebrain/0.0.1");
        fs::create_dir_all(&v).unwrap();
        let installed = dir.path().join("installed_plugins.json");
        // No file written — fallback glob path.
        let out = clean_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(out.removed, 1);
    }

    // ── detection (read-only) ───────────────────────────────────────────────

    #[test]
    fn detect_lists_orphan_versions_without_removing() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        // Reproduce the real bug: an orphan `onebrain/onebrain/2.2.4` whose
        // marketplace is NOT registered in installed_plugins.json.
        let orphan = cache_dir.join("onebrain/onebrain/2.2.4");
        fs::create_dir_all(&orphan).unwrap();
        let installed = dir.path().join("installed_plugins.json");
        // installed_plugins.json has no `onebrain@` entry → forces the fallback
        // glob, exactly like the live orphan.
        fs::write(&installed, r#"{"plugins":{}}"#).unwrap();

        let stale = detect_stale_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(
            stale,
            vec![StaleCacheEntry {
                marketplace: "onebrain".to_string(),
                version: "2.2.4".to_string(),
            }]
        );
        // Read-only: the dir must still exist after detection.
        assert!(orphan.exists());
    }

    #[test]
    fn detect_returns_empty_when_cache_clean() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        let installed = dir.path().join("installed_plugins.json");
        fs::write(&installed, r#"{"plugins":{}}"#).unwrap();
        assert!(detect_stale_plugin_cache(&installed, Some(&cache_dir)).is_empty());
    }

    #[test]
    fn detect_then_clean_enumerate_same_dirs() {
        // Both entry points walk the identical version-dir set, so on a
        // writable tree (every removal succeeds) the removed count equals the
        // detected count. (On a removal failure clean's count is lower — see
        // the module doc; not exercised here.)
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(cache_dir.join("mkt/onebrain/3.0.0")).unwrap();
        fs::create_dir_all(cache_dir.join("mkt/onebrain/3.1.0")).unwrap();
        let installed = dir.path().join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({ "plugins": { "onebrain@mkt": [{"id":"onebrain"}] } }),
        );
        let detected = detect_stale_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(detected.len(), 2);
        let out = clean_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(out.removed as usize, detected.len());
    }

    #[test]
    fn stray_file_under_onebrain_is_ignored() {
        // A non-dir entry (e.g. `.DS_Store`, a manifest) alongside version dirs
        // must be neither reported nor removed — only version *directories*.
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let onebrain = cache_dir.join("mkt/onebrain");
        fs::create_dir_all(onebrain.join("3.0.0")).unwrap();
        let stray = onebrain.join(".DS_Store");
        fs::write(&stray, b"x").unwrap();
        let installed = dir.path().join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({ "plugins": { "onebrain@mkt": [{"id":"onebrain"}] } }),
        );

        let detected = detect_stale_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(detected.len(), 1, "only the version dir is stale");
        assert_eq!(detected[0].version, "3.0.0");
        let out = clean_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(out.removed, 1);
        assert!(stray.exists(), "stray file must survive the sweep");
    }

    #[test]
    fn traversal_marketplace_key_is_rejected() {
        // A hostile/corrupted `onebrain@../../escape` key must NOT make the
        // candidate resolve outside the cache tree. With no safe `onebrain@`
        // entry, discovery falls back to the glob (which only sees real cache
        // children), so the escape target is never touched.
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        // A dir OUTSIDE the cache that a traversal key would point at.
        let escape = dir.path().join("escape/onebrain/9.9.9");
        fs::create_dir_all(&escape).unwrap();
        let installed = dir.path().join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({ "plugins": { "onebrain@../escape": [{"id":"onebrain"}] } }),
        );

        let out = clean_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(
            out.removed, 0,
            "traversal key must not reach outside the cache"
        );
        assert!(escape.exists(), "the escape target must be untouched");
    }

    #[test]
    fn glob_catches_orphan_under_unregistered_marketplace() {
        // Registered marketplace "mktA" has a version dir; an ORPHAN lives under
        // unregistered "mktB". Pre-fix, the `if dirs.is_empty()` gate skipped the
        // glob (dirs already had mktA), so mktB's orphan was never cleaned.
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let registered = cache_dir.join("mktA/onebrain/3.2.0");
        let orphan = cache_dir.join("mktB/onebrain/2.2.4");
        fs::create_dir_all(&registered).unwrap();
        fs::create_dir_all(&orphan).unwrap();
        let installed = dir.path().join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({ "plugins": { "onebrain@mktA": [{"id":"onebrain"}] } }),
        );
        let out = clean_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(
            out.removed, 2,
            "both registered + orphan version dirs removed"
        );
        assert!(!registered.exists());
        assert!(
            !orphan.exists(),
            "orphan under unregistered marketplace must be cleaned"
        );
    }

    #[test]
    #[cfg(unix)]
    fn remove_failure_is_counted_and_surfaced_not_swallowed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let onebrain = cache_dir.join("mkt/onebrain");
        let version = onebrain.join("9.9.9");
        fs::create_dir_all(&version).unwrap();
        let installed = dir.path().join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({ "plugins": { "onebrain@mkt": [{"id":"onebrain"}] } }),
        );
        // Make the parent `onebrain/` non-writable so removing the child dir fails
        // (can't unlink within a read-only directory on Unix).
        fs::set_permissions(&onebrain, fs::Permissions::from_mode(0o500)).unwrap();

        let out = clean_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(
            out.removed, 0,
            "removal failed → nothing counted as removed"
        );
        assert_eq!(
            out.failed, 1,
            "the failure is COUNTED, not silently swallowed"
        );

        // Restore perms so tempdir cleanup succeeds.
        fs::set_permissions(&onebrain, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn symlinked_version_dir_is_skipped() {
        // A symlink under `.../onebrain/` is classified as a symlink (not a
        // dir) and skipped — never followed into a recursive delete.
        #[cfg(unix)]
        {
            let dir = tempdir().unwrap();
            let cache_dir = dir.path().join("cache");
            let onebrain = cache_dir.join("mkt/onebrain");
            fs::create_dir_all(&onebrain).unwrap();
            let real_target = dir.path().join("real-target");
            fs::create_dir_all(real_target.join("keep")).unwrap();
            std::os::unix::fs::symlink(&real_target, onebrain.join("3.0.0")).unwrap();
            let installed = dir.path().join("installed_plugins.json");
            write_json(
                &installed,
                &serde_json::json!({ "plugins": { "onebrain@mkt": [{"id":"onebrain"}] } }),
            );

            assert!(detect_stale_plugin_cache(&installed, Some(&cache_dir)).is_empty());
            let out = clean_plugin_cache(&installed, Some(&cache_dir));
            assert_eq!(out.removed, 0);
            // The symlink target's contents must be untouched.
            assert!(real_target.join("keep").exists());
        }
    }
}
