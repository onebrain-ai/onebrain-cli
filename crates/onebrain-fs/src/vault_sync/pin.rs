//! Step 8 · refresh `installed_plugins.json` entries for this vault.
//! Port of Bun's `pinToVault` — by far the gnarliest function in vault-sync.ts.
//!
//! Responsibilities, in order:
//!   1. If any `onebrain@*` entry has `source: 'marketplace'`, skip the entire
//!      pin step (Claude Code owns marketplace plugins).
//!   2. For `onebrain@onebrain` only — orphan dedup: drop entries whose
//!      `projectPath` ENOENTs. Non-ENOENT stat errors (EACCES, EIO, …) preserve
//!      the entry + emit a stderr warning.
//!   3. Per-entry: if it belongs to this vault (matched via installPath ==
//!      vaultPluginDir, OR installPath under cacheDir, OR projectPath ==
//!      vaultRoot), canonicalize installPath to vaultPluginDir and refresh
//!      version + lastUpdated. lastUpdated only bumps when version changes.
//!   4. Non-string installPath/projectPath → stderr warning, skip entry.
//!   5. Write atomically only if anything changed.

use chrono::Utc;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};

use super::types::NowFn;
use super::vault_yml::atomic_write;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinResult {
    pub skipped: bool,
}

/// Plugin metadata read from the vault's deployed `.claude-plugin/plugin.json`.
pub(crate) struct PluginMeta {
    pub version: String,
    pub last_updated: Option<String>,
}

pub(crate) fn read_plugin_metadata(vault_root: &Path) -> PluginMeta {
    let path = vault_root.join(".claude/plugins/onebrain/.claude-plugin/plugin.json");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            return PluginMeta {
                version: "unknown".into(),
                last_updated: None,
            }
        }
    };
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return PluginMeta {
                version: "unknown".into(),
                last_updated: None,
            }
        }
    };
    let version = parsed
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let last_updated = parsed
        .get("lastUpdated")
        .and_then(Value::as_str)
        .map(str::to_string);
    PluginMeta {
        version,
        last_updated,
    }
}

/// Normalize a filesystem path for entry-belongs-to-this-vault comparison.
/// Mirrors Bun's `normalizePath`: `path.resolve` + strip trailing separator.
/// Deliberately does NOT realpath — that would touch fs per entry.
pub fn normalize_path(p: &Path) -> PathBuf {
    // `std::path::absolute` is stable since 1.79; fall back to canonicalize if
    // missing (we keep `path-absolutize` out of the dep tree).
    let mut buf: PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    // Strip trailing separator if it's longer than 1 char (preserve "/" root).
    let s = buf.to_string_lossy().into_owned();
    let sep_str = MAIN_SEPARATOR.to_string();
    if s.ends_with(&sep_str) && s.len() > sep_str.len() {
        let trimmed = &s[..s.len() - sep_str.len()];
        buf = PathBuf::from(trimmed);
    }
    buf
}

/// Step 8 · entry point.
pub fn pin_to_vault(
    vault_root: &Path,
    installed_plugins_path: &Path,
    installed_plugins_cache_dir: Option<&Path>,
    now_fn: &NowFn,
) -> std::io::Result<PinResult> {
    pin_to_vault_inner(
        vault_root,
        installed_plugins_path,
        installed_plugins_cache_dir,
        now_fn,
        &mut std::io::stderr(),
    )
}

/// Inner driver — takes an injected stderr writer for testability (test 21).
pub(crate) fn pin_to_vault_inner<W: Write>(
    vault_root: &Path,
    installed_plugins_path: &Path,
    installed_plugins_cache_dir: Option<&Path>,
    now_fn: &NowFn,
    stderr: &mut W,
) -> std::io::Result<PinResult> {
    // 1. Read installed_plugins.json — missing → skipped no-op.
    let text = match fs::read_to_string(installed_plugins_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PinResult { skipped: true });
        }
        Err(e) => return Err(e),
    };
    let mut data: Value = serde_json::from_str(&text).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "installed_plugins.json is not valid JSON: {}: {}",
                installed_plugins_path.display(),
                e
            ),
        )
    })?;

    let Some(plugins) = data.get_mut("plugins").and_then(Value::as_object_mut) else {
        return Ok(PinResult { skipped: true });
    };

    let onebrain_keys: Vec<String> = plugins
        .keys()
        .filter(|k| k.starts_with("onebrain@"))
        .cloned()
        .collect();
    if onebrain_keys.is_empty() {
        return Ok(PinResult { skipped: true });
    }

    let vault_plugin_dir = vault_root.join(".claude/plugins/onebrain");
    let normalized_vault_root = normalize_path(vault_root);
    let normalized_vault_plugin_dir = normalize_path(&vault_plugin_dir);

    let meta = read_plugin_metadata(vault_root);
    let updated_at = meta
        .last_updated
        .clone()
        .unwrap_or_else(|| now_fn().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));

    let cache_dir_owned = installed_plugins_cache_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            installed_plugins_path
                .parent()
                .map(|p| p.join("cache"))
                .unwrap_or_else(|| PathBuf::from("cache"))
        });
    let normalized_cache_dir = normalize_path(&cache_dir_owned);
    let cache_dir_prefix = format!("{}{}", normalized_cache_dir.display(), MAIN_SEPARATOR);

    // 2. Marketplace short-circuit — any onebrain entry with source: marketplace.
    let has_marketplace = onebrain_keys.iter().any(|k| {
        plugins
            .get(k)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .any(|e| e.get("source").and_then(Value::as_str) == Some("marketplace"))
            })
            .unwrap_or(false)
    });
    if has_marketplace {
        return Ok(PinResult { skipped: true });
    }

    let mut changed = false;

    // 3. Orphan dedup — `onebrain@onebrain` only, ENOENT-only.
    const ONEBRAIN_KEY: &str = "onebrain@onebrain";
    if let Some(Value::Array(before)) = plugins.get(ONEBRAIN_KEY).cloned() {
        if let Some(arr) = plugins.get_mut(ONEBRAIN_KEY).and_then(Value::as_array_mut) {
            let mut keep: Vec<Value> = Vec::with_capacity(arr.len());
            for entry in arr.drain(..) {
                let project_path = entry.get("projectPath").and_then(Value::as_str);
                let Some(pp) = project_path else {
                    keep.push(entry);
                    continue;
                };
                match fs::metadata(pp) {
                    Ok(_) => keep.push(entry),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Drop — genuine orphan.
                    }
                    Err(e) => {
                        // Preserve + warn (test 18).
                        let _ = writeln!(
                            stderr,
                            "vault-sync: pin warning: stat {}: {}",
                            pp,
                            io_err_code(&e)
                        );
                        keep.push(entry);
                    }
                }
            }
            if keep.len() != before.len() {
                changed = true;
            }
            *arr = keep;
        }
    }

    // 4. Per-entry refresh.
    for key in &onebrain_keys {
        let Some(entries) = plugins.get_mut(key).and_then(Value::as_array_mut) else {
            continue;
        };
        for entry in entries.iter_mut() {
            let Value::Object(map) = entry else {
                continue;
            };

            // Malformed: non-string installPath / projectPath.
            let install_bad = matches!(
                map.get("installPath"),
                Some(v) if !v.is_string() && !v.is_null()
            );
            let project_bad = matches!(
                map.get("projectPath"),
                Some(v) if !v.is_string() && !v.is_null()
            );
            if install_bad || project_bad {
                let _ = writeln!(
                    stderr,
                    "vault-sync: pin warning: malformed entry — non-string installPath/projectPath in installed_plugins.json[{key}]"
                );
                continue;
            }

            let install_path = map.get("installPath").and_then(Value::as_str);
            let project_path = map.get("projectPath").and_then(Value::as_str);

            let normalized_install_path = install_path.map(|s| normalize_path(Path::new(s)));
            let normalized_project_path = project_path.map(|s| normalize_path(Path::new(s)));

            let in_cache = normalized_install_path
                .as_ref()
                .map(|p| {
                    let s = p.to_string_lossy();
                    *p == normalized_cache_dir || s.starts_with(&cache_dir_prefix)
                })
                .unwrap_or(false);
            let is_this_vault =
                normalized_install_path.as_ref() == Some(&normalized_vault_plugin_dir);
            let is_this_project = normalized_project_path.as_ref() == Some(&normalized_vault_root);

            if !is_this_vault && !in_cache && !is_this_project {
                continue;
            }

            // Canonicalize installPath when stale.
            if normalized_install_path.as_ref() != Some(&normalized_vault_plugin_dir) {
                map.insert(
                    "installPath".to_string(),
                    Value::String(vault_plugin_dir.to_string_lossy().into_owned()),
                );
                changed = true;
            }

            // Refresh version + lastUpdated only when version actually changes.
            let current_version = map
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string);
            if current_version.as_deref() != Some(meta.version.as_str()) {
                map.insert("version".to_string(), Value::String(meta.version.clone()));
                map.insert("lastUpdated".to_string(), Value::String(updated_at.clone()));
                changed = true;
            }
        }
    }

    if !changed {
        return Ok(PinResult { skipped: false });
    }

    let serialized = serialize_pretty(&data);
    atomic_write(installed_plugins_path, serialized.as_bytes())?;
    Ok(PinResult { skipped: false })
}

/// Match Bun's `JSON.stringify(data, null, 4)` — 4-space indent.
fn serialize_pretty(data: &Value) -> String {
    // serde_json with a 4-space pretty formatter.
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(data, &mut ser).expect("serde_json should not fail on Value");
    String::from_utf8(out).expect("serde_json emits valid utf-8")
}

fn io_err_code(e: &std::io::Error) -> String {
    // Best-effort: include the OS code if present, else the kind.
    if let Some(code) = e.raw_os_error() {
        format!("os error {code}")
    } else {
        format!("{:?}", e.kind())
    }
}

/// Helper for tests that want to bypass the file write — used to verify in-memory
/// transformations without touching disk.
#[allow(dead_code)]
pub(crate) fn default_now_fn() -> NowFn {
    Box::new(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs;
    use tempfile::tempdir;

    fn write_json(path: &Path, v: &Value) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
    }

    fn write_plugin_json(vault_root: &Path, version: &str) {
        let path = vault_root.join(".claude/plugins/onebrain/.claude-plugin/plugin.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "id": "onebrain",
                "version": version,
                "name": "OneBrain",
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn fixed_now() -> NowFn {
        Box::new(|| Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap())
    }

    // ---- normalize_path ----

    #[test]
    fn normalize_strips_trailing_slash() {
        let absolute = if cfg!(windows) {
            "C:\\foo\\bar\\"
        } else {
            "/foo/bar/"
        };
        let got = normalize_path(Path::new(absolute));
        let expected = if cfg!(windows) {
            "C:\\foo\\bar"
        } else {
            "/foo/bar"
        };
        assert_eq!(got, PathBuf::from(expected));
    }

    #[test]
    fn normalize_preserves_root_separator() {
        let got = normalize_path(Path::new("/"));
        assert_eq!(got, PathBuf::from("/"));
    }

    #[test]
    fn normalize_is_idempotent() {
        let absolute = if cfg!(windows) {
            "C:\\foo\\bar"
        } else {
            "/foo/bar"
        };
        let once = normalize_path(Path::new(absolute));
        let twice = normalize_path(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn normalize_resolves_relative_to_cwd() {
        // Even a relative path should come out absolute.
        let rel = Path::new("relative-thing");
        let got = normalize_path(rel);
        assert!(got.is_absolute() || got == rel.to_path_buf());
    }

    #[test]
    fn normalize_handles_complex_paths() {
        let absolute = if cfg!(windows) {
            "C:\\foo\\\\bar"
        } else {
            "/foo/bar/"
        };
        // Just ensure it doesn't panic.
        let _ = normalize_path(Path::new(absolute));
    }

    // ---- pin_to_vault ----

    #[test]
    fn missing_installed_plugins_json_returns_skipped() {
        let dir = tempdir().unwrap();
        let result = pin_to_vault(
            dir.path(),
            &dir.path().join("nonexistent.json"),
            None,
            &fixed_now(),
        )
        .unwrap();
        assert!(result.skipped);
    }

    #[test]
    fn marketplace_entry_short_circuits_pin() {
        let dir = tempdir().unwrap();
        write_plugin_json(dir.path(), "1.11.0");
        let installed = dir.path().join(".fake/installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({
                "plugins": {
                    "onebrain@marketplace": [{
                        "id": "onebrain",
                        "source": "marketplace",
                        "installPath": "/some/cache/marketplace/onebrain/1.10.0",
                    }]
                }
            }),
        );
        let result = pin_to_vault(dir.path(), &installed, None, &fixed_now()).unwrap();
        assert!(result.skipped);
        let after: Value = serde_json::from_str(&fs::read_to_string(&installed).unwrap()).unwrap();
        assert_eq!(
            after["plugins"]["onebrain@marketplace"][0]["installPath"],
            "/some/cache/marketplace/onebrain/1.10.0"
        );
    }

    #[test]
    fn pin_canonicalizes_install_path_for_cache_entry() {
        let dir = tempdir().unwrap();
        write_plugin_json(dir.path(), "1.11.0");
        let fake = dir.path().join(".fake");
        let cache_dir = fake.join("cache");
        let entry_path = cache_dir.join("onebrain-local/onebrain/1.10.0");
        fs::create_dir_all(&entry_path).unwrap();
        let installed = fake.join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({
                "plugins": {
                    "onebrain@onebrain-local": [{
                        "id": "onebrain",
                        "source": "local",
                        "installPath": entry_path.to_string_lossy(),
                    }]
                }
            }),
        );
        let result = pin_to_vault(dir.path(), &installed, Some(&cache_dir), &fixed_now()).unwrap();
        assert!(!result.skipped);
        let after: Value = serde_json::from_str(&fs::read_to_string(&installed).unwrap()).unwrap();
        assert_eq!(
            after["plugins"]["onebrain@onebrain-local"][0]["installPath"],
            dir.path()
                .join(".claude/plugins/onebrain")
                .to_string_lossy()
                .to_string()
        );
    }

    #[test]
    fn pin_writes_last_updated_via_now_fn_when_plugin_json_omits_it() {
        let dir = tempdir().unwrap();
        write_plugin_json(dir.path(), "1.11.0");
        let fake = dir.path().join(".fake");
        let cache_dir = fake.join("cache");
        let entry_path = cache_dir.join("onebrain/onebrain/1.10.0");
        fs::create_dir_all(&entry_path).unwrap();
        let installed = fake.join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({
                "plugins": {
                    "onebrain@onebrain": [{
                        "id": "onebrain",
                        "source": "project",
                        "installPath": entry_path.to_string_lossy(),
                        "version": "1.10.0",
                    }]
                }
            }),
        );
        let now: NowFn = Box::new(|| Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap());
        let _ = pin_to_vault(dir.path(), &installed, Some(&cache_dir), &now).unwrap();
        let after: Value = serde_json::from_str(&fs::read_to_string(&installed).unwrap()).unwrap();
        let entry = &after["plugins"]["onebrain@onebrain"][0];
        assert_eq!(entry["version"], "1.11.0");
        let lu = entry["lastUpdated"].as_str().unwrap();
        assert!(lu.starts_with("2026-05-06T12:00:00"));
    }

    #[test]
    fn orphan_dedup_drops_enoent_projectpath_entries() {
        let dir = tempdir().unwrap();
        write_plugin_json(dir.path(), "1.11.0");
        let valid_project = dir.path().join("valid-vault");
        fs::create_dir_all(&valid_project).unwrap();
        let fake = dir.path().join(".fake");
        let cache_dir = fake.join("cache");
        let installed = fake.join("installed_plugins.json");
        let ghost_path = "/tmp/ghost-nonexistent-vault-vs13";
        write_json(
            &installed,
            &serde_json::json!({
                "plugins": {
                    "onebrain@onebrain": [
                        {
                            "id": "onebrain",
                            "source": "project",
                            "installPath": dir.path().join(".claude/plugins/onebrain").to_string_lossy(),
                            "projectPath": valid_project.to_string_lossy(),
                            "version": "1.10.0",
                        },
                        {
                            "id": "onebrain",
                            "source": "project",
                            "installPath": dir.path().join(".claude/plugins/onebrain").to_string_lossy(),
                            "projectPath": ghost_path,
                            "version": "1.10.0",
                        }
                    ],
                    "other-plugin@somewhere": [{
                        "id": "other-plugin",
                        "source": "project",
                        "projectPath": "/tmp/another-ghost-vs13",
                    }]
                }
            }),
        );
        pin_to_vault(dir.path(), &installed, Some(&cache_dir), &fixed_now()).unwrap();
        let after: Value = serde_json::from_str(&fs::read_to_string(&installed).unwrap()).unwrap();
        let arr = after["plugins"]["onebrain@onebrain"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "ghost entry should be dropped");
        // Other plugin untouched.
        assert_eq!(
            after["plugins"]["other-plugin@somewhere"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn cross_vault_isolation_other_vault_entry_byte_identical() {
        let dir = tempdir().unwrap();
        write_plugin_json(dir.path(), "1.11.0");
        let fake = dir.path().join(".fake");
        let cache_dir = fake.join("cache");
        let other_install = "/path/to/other/vault/.claude/plugins/onebrain";
        let installed = fake.join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({
                "plugins": {
                    "onebrain@onebrain": [
                        {
                            "id": "onebrain",
                            "source": "project",
                            "installPath": dir.path().join(".claude/plugins/onebrain").to_string_lossy(),
                            "version": "1.10.0",
                            "lastUpdated": "2026-01-01T00:00:00.000Z",
                        },
                        {
                            "id": "onebrain",
                            "source": "project",
                            "installPath": other_install,
                            "version": "2.0.0",
                            "lastUpdated": "2025-12-31T23:59:59.000Z",
                        }
                    ]
                }
            }),
        );
        pin_to_vault(dir.path(), &installed, Some(&cache_dir), &fixed_now()).unwrap();
        let after: Value = serde_json::from_str(&fs::read_to_string(&installed).unwrap()).unwrap();
        let arr = after["plugins"]["onebrain@onebrain"].as_array().unwrap();
        let this_vault = arr
            .iter()
            .find(|e| {
                e["installPath"].as_str()
                    == Some(
                        &dir.path()
                            .join(".claude/plugins/onebrain")
                            .to_string_lossy(),
                    )
            })
            .unwrap();
        assert_eq!(this_vault["version"], "1.11.0");
        let other = arr
            .iter()
            .find(|e| e["installPath"] == other_install)
            .unwrap();
        assert_eq!(other["version"], "2.0.0");
        assert_eq!(other["lastUpdated"], "2025-12-31T23:59:59.000Z");
    }

    #[test]
    fn pin_idempotent_for_repeat_syncs_at_same_version() {
        let dir = tempdir().unwrap();
        write_plugin_json(dir.path(), "1.11.0");
        let fake = dir.path().join(".fake");
        let cache_dir = fake.join("cache");
        let installed = fake.join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({
                "plugins": {
                    "onebrain@onebrain": [{
                        "id": "onebrain",
                        "source": "project",
                        "installPath": dir.path().join(".claude/plugins/onebrain").to_string_lossy(),
                        "version": "1.11.0",
                        "lastUpdated": "2026-01-01T00:00:00.000Z",
                    }]
                }
            }),
        );
        let now1: NowFn = Box::new(|| Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap());
        pin_to_vault(dir.path(), &installed, Some(&cache_dir), &now1).unwrap();
        let first = fs::read_to_string(&installed).unwrap();
        let now2: NowFn = Box::new(|| Utc.with_ymd_and_hms(2027, 6, 15, 12, 0, 0).unwrap());
        pin_to_vault(dir.path(), &installed, Some(&cache_dir), &now2).unwrap();
        let second = fs::read_to_string(&installed).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn stale_install_path_matched_via_projectpath_rewrites_install() {
        let dir = tempdir().unwrap();
        write_plugin_json(dir.path(), "1.11.0");
        let fake = dir.path().join(".fake");
        let cache_dir = fake.join("cache");
        let installed = fake.join("installed_plugins.json");
        let stale = "/tmp/gone-vault-vs13-stale";
        write_json(
            &installed,
            &serde_json::json!({
                "plugins": {
                    "onebrain@onebrain": [{
                        "id": "onebrain",
                        "source": "project",
                        "installPath": stale,
                        "projectPath": dir.path().to_string_lossy(),
                        "version": "1.10.0",
                        "lastUpdated": "2025-01-01T00:00:00.000Z",
                    }]
                }
            }),
        );
        pin_to_vault(dir.path(), &installed, Some(&cache_dir), &fixed_now()).unwrap();
        let after: Value = serde_json::from_str(&fs::read_to_string(&installed).unwrap()).unwrap();
        let entry = &after["plugins"]["onebrain@onebrain"][0];
        assert_eq!(
            entry["installPath"],
            dir.path()
                .join(".claude/plugins/onebrain")
                .to_string_lossy()
                .to_string()
        );
        assert_eq!(entry["version"], "1.11.0");
    }

    #[test]
    fn trailing_slash_on_projectpath_still_matches() {
        let dir = tempdir().unwrap();
        write_plugin_json(dir.path(), "1.11.0");
        let fake = dir.path().join(".fake");
        let cache_dir = fake.join("cache");
        let installed = fake.join("installed_plugins.json");
        let stale = "/tmp/gone-vault-vs13-ts";
        write_json(
            &installed,
            &serde_json::json!({
                "plugins": {
                    "onebrain@onebrain": [{
                        "id": "onebrain",
                        "source": "project",
                        "installPath": stale,
                        "projectPath": format!("{}/", dir.path().to_string_lossy()),
                        "version": "1.10.0",
                    }]
                }
            }),
        );
        pin_to_vault(dir.path(), &installed, Some(&cache_dir), &fixed_now()).unwrap();
        let after: Value = serde_json::from_str(&fs::read_to_string(&installed).unwrap()).unwrap();
        let entry = &after["plugins"]["onebrain@onebrain"][0];
        assert_eq!(
            entry["installPath"],
            dir.path()
                .join(".claude/plugins/onebrain")
                .to_string_lossy()
                .to_string()
        );
        assert_eq!(entry["version"], "1.11.0");
    }

    #[test]
    fn malformed_entry_warns_and_skips_processes_siblings() {
        let dir = tempdir().unwrap();
        write_plugin_json(dir.path(), "1.11.0");
        let fake = dir.path().join(".fake");
        let cache_dir = fake.join("cache");
        let installed = fake.join("installed_plugins.json");
        write_json(
            &installed,
            &serde_json::json!({
                "plugins": {
                    "onebrain@onebrain": [
                        {
                            "id": "onebrain",
                            "source": "project",
                            "installPath": 12345,
                            "version": "0.0.0",
                        },
                        {
                            "id": "onebrain",
                            "source": "project",
                            "installPath": dir.path().join(".claude/plugins/onebrain").to_string_lossy(),
                            "version": "1.10.0",
                            "lastUpdated": "2025-01-01T00:00:00.000Z",
                        }
                    ]
                }
            }),
        );
        let mut stderr = Vec::new();
        pin_to_vault_inner(
            dir.path(),
            &installed,
            Some(&cache_dir),
            &fixed_now(),
            &mut stderr,
        )
        .unwrap();
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("malformed entry"));
        assert!(stderr_str.contains("onebrain@onebrain"));
        let after: Value = serde_json::from_str(&fs::read_to_string(&installed).unwrap()).unwrap();
        let arr = after["plugins"]["onebrain@onebrain"].as_array().unwrap();
        // Malformed entry preserved exactly.
        assert_eq!(arr[0]["installPath"], 12345);
        assert_eq!(arr[0]["version"], "0.0.0");
        // Sibling refreshed.
        assert_eq!(
            arr[1]["installPath"],
            dir.path()
                .join(".claude/plugins/onebrain")
                .to_string_lossy()
                .to_string()
        );
        assert_eq!(arr[1]["version"], "1.11.0");
    }

    #[test]
    fn invalid_json_returns_error() {
        let dir = tempdir().unwrap();
        let installed = dir.path().join("installed_plugins.json");
        fs::write(&installed, "{not valid json").unwrap();
        let err = pin_to_vault(dir.path(), &installed, None, &fixed_now()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
