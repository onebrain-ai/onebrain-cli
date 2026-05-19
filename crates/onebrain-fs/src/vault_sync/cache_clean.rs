//! Step 9 · clean obsolete OneBrain cache versions under `<cacheDir>/<marketplace>/onebrain/`.
//! Port of Bun's `cleanPluginCache`. Non-fatal — any error returns the count
//! of successful removals.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Step 9 entry point. Returns the number of version directories actually removed.
pub fn clean_plugin_cache(
    installed_plugins_path: &Path,
    installed_plugins_cache_dir: Option<&Path>,
) -> u64 {
    let cache_dir_owned = installed_plugins_cache_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            installed_plugins_path
                .parent()
                .map(|p| p.join("cache"))
                .unwrap_or_else(|| PathBuf::from("cache"))
        });
    if !cache_dir_owned.exists() {
        return 0;
    }

    let mut onebrain_dirs: Vec<PathBuf> = Vec::new();

    // First try to discover marketplaces via installed_plugins.json.
    if let Ok(text) = fs::read_to_string(installed_plugins_path) {
        if let Ok(data) = serde_json::from_str::<Value>(&text) {
            if let Some(plugins) = data.get("plugins").and_then(Value::as_object) {
                for key in plugins.keys() {
                    if !key.starts_with("onebrain@") {
                        continue;
                    }
                    let marketplace = key.split('@').nth(1).unwrap_or("");
                    if marketplace.is_empty() {
                        continue;
                    }
                    let candidate = cache_dir_owned.join(marketplace).join("onebrain");
                    if candidate.exists() {
                        onebrain_dirs.push(candidate);
                    }
                }
            }
        }
    }

    // Fallback — glob for any cache/*/onebrain/ directories.
    if onebrain_dirs.is_empty() {
        if let Ok(read) = fs::read_dir(&cache_dir_owned) {
            for entry in read.flatten() {
                let candidate = entry.path().join("onebrain");
                if candidate.exists() {
                    onebrain_dirs.push(candidate);
                }
            }
        }
    }

    let mut removed = 0u64;
    for plugin_dir in onebrain_dirs {
        let Ok(read) = fs::read_dir(&plugin_dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() && fs::remove_dir_all(&path).is_ok() {
                removed += 1;
            }
        }
    }

    removed
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
        let n = clean_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(n, 2);
        assert!(!v_a.exists());
        assert!(!v_b.exists());
    }

    #[test]
    fn missing_cache_dir_returns_zero() {
        let dir = tempdir().unwrap();
        let installed = dir.path().join("installed_plugins.json");
        fs::write(&installed, "{}").unwrap();
        let n = clean_plugin_cache(&installed, Some(&dir.path().join("nonexistent-cache")));
        assert_eq!(n, 0);
    }

    #[test]
    fn fallback_glob_when_installed_plugins_unreadable() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let v = cache_dir.join("anything/onebrain/0.0.1");
        fs::create_dir_all(&v).unwrap();
        let installed = dir.path().join("installed_plugins.json");
        // No file written — fallback glob path.
        let n = clean_plugin_cache(&installed, Some(&cache_dir));
        assert_eq!(n, 1);
    }
}
