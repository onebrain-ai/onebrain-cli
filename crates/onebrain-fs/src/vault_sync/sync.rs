//! Steps 2–4 — overlay plugin files, `.gemini/` project config, and (init only)
//! `.obsidian/` from the extracted release tarball into the vault.
//!
//! Port of Bun's `syncPluginFiles`, `syncGeminiConfig`, and the inlined
//! `.obsidian/` block. Each function returns `(files_added, files_removed)`
//! and is independently fallible; the orchestrator surfaces step-2 errors as
//! critical and step-4 errors as best-effort.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::types::UnlinkFn;
use super::walker::list_files_recursive;

/// Step 2 · sync `.claude/plugins/onebrain/` from extracted tarball to vault.
///
/// Mirrors Bun's `syncPluginFiles`:
/// - copy every source file into the vault (overwrites)
/// - remove every vault file not present in source (counts only successful unlinks)
pub fn sync_plugin_files(
    extracted_dir: &Path,
    vault_root: &Path,
    unlink_fn: &UnlinkFn,
) -> std::io::Result<(u64, u64)> {
    let source_plugin = extracted_dir.join(".claude/plugins/onebrain");
    let dest_plugin = vault_root.join(".claude/plugins/onebrain");
    overlay_directory(&source_plugin, &dest_plugin, unlink_fn, false)
}

/// Step 3 · sync `.gemini/` project config tree. Source-absent → no-op.
pub fn sync_gemini_config(
    extracted_dir: &Path,
    vault_root: &Path,
    unlink_fn: &UnlinkFn,
) -> std::io::Result<(u64, u64)> {
    let source_gemini = extracted_dir.join(".gemini");
    let dest_gemini = vault_root.join(".gemini");
    if !source_gemini.exists() {
        return Ok((0, 0));
    }
    overlay_directory(&source_gemini, &dest_gemini, unlink_fn, false)
}

/// Step 4 · sync `.obsidian/` directory (init only). Errors are non-fatal.
/// Returns the number of files added (no stale removal — init-only).
pub fn sync_obsidian(extracted_dir: &Path, vault_root: &Path) -> u64 {
    let src = extracted_dir.join(".obsidian");
    let dest = vault_root.join(".obsidian");
    if !src.exists() {
        return 0;
    }
    let mut added = 0u64;
    for src_path in list_files_recursive(&src) {
        let rel = match src_path.strip_prefix(&src) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let dest_path = dest.join(rel);
        if let Some(parent) = dest_path.parent() {
            if fs::create_dir_all(parent).is_err() {
                continue;
            }
        }
        if let Ok(content) = fs::read(&src_path) {
            if fs::write(&dest_path, content).is_ok() {
                added += 1;
            }
        }
    }
    added
}

/// Internal — copy everything from `source_dir` into `dest_dir` and remove
/// anything in dest that's not in source. Counts only successful unlinks.
///
/// `_strict` parameter reserved for a future opt-in mode that propagates
/// per-file errors instead of swallowing them. Currently always best-effort.
fn overlay_directory(
    source_dir: &Path,
    dest_dir: &Path,
    unlink_fn: &UnlinkFn,
    _strict: bool,
) -> std::io::Result<(u64, u64)> {
    fs::create_dir_all(dest_dir)?;

    let source_files = list_files_recursive(source_dir);
    let source_rels: HashSet<PathBuf> = source_files
        .iter()
        .filter_map(|p| p.strip_prefix(source_dir).ok().map(|r| r.to_path_buf()))
        .collect();

    let dest_files = list_files_recursive(dest_dir);
    let dest_rels: HashSet<PathBuf> = dest_files
        .iter()
        .filter_map(|p| p.strip_prefix(dest_dir).ok().map(|r| r.to_path_buf()))
        .collect();

    let stale_rels: Vec<&PathBuf> = dest_rels
        .iter()
        .filter(|r| !source_rels.contains(*r))
        .collect();

    let mut files_added: u64 = 0;
    for src_path in &source_files {
        let rel = match src_path.strip_prefix(source_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let dest_path = dest_dir.join(rel);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = fs::read(src_path)?;
        fs::write(&dest_path, content)?;
        files_added += 1;
    }

    let mut files_removed: u64 = 0;
    for rel in stale_rels {
        let dest_path = dest_dir.join(rel);
        // Per Bun: only count successful unlinks; failures are best-effort.
        if unlink_fn(&dest_path).is_ok() {
            files_removed += 1;
        }
    }

    Ok((files_added, files_removed))
}

/// Default unlink — `std::fs::remove_file`.
pub fn default_unlink_fn() -> UnlinkFn {
    Box::new(|p: &Path| fs::remove_file(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(p: &Path, s: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, s).unwrap();
    }

    // ---- sync_plugin_files ----

    #[test]
    fn fresh_dest_copies_all_source_files_no_removals() {
        let dir = tempdir().unwrap();
        let extracted = dir.path().join("extracted");
        let vault = dir.path().join("vault");
        write(
            &extracted.join(".claude/plugins/onebrain/.claude-plugin/plugin.json"),
            r#"{"version":"1.11.0"}"#,
        );
        write(
            &extracted.join(".claude/plugins/onebrain/skills/example/SKILL.md"),
            "x",
        );
        let unlink: UnlinkFn = default_unlink_fn();
        let (added, removed) = sync_plugin_files(&extracted, &vault, &unlink).unwrap();
        assert_eq!(added, 2);
        assert_eq!(removed, 0);
        assert!(vault
            .join(".claude/plugins/onebrain/.claude-plugin/plugin.json")
            .exists());
    }

    #[test]
    fn stale_dest_file_removed() {
        let dir = tempdir().unwrap();
        let extracted = dir.path().join("extracted");
        let vault = dir.path().join("vault");
        write(
            &extracted.join(".claude/plugins/onebrain/.claude-plugin/plugin.json"),
            r#"{"version":"1.11.0"}"#,
        );
        write(&vault.join(".claude/plugins/onebrain/stale.md"), "# stale");
        let unlink: UnlinkFn = default_unlink_fn();
        let (_added, removed) = sync_plugin_files(&extracted, &vault, &unlink).unwrap();
        assert_eq!(removed, 1);
        assert!(!vault.join(".claude/plugins/onebrain/stale.md").exists());
    }

    #[test]
    fn unlink_failure_on_one_of_two_counts_only_successes() {
        let dir = tempdir().unwrap();
        let extracted = dir.path().join("extracted");
        let vault = dir.path().join("vault");
        // Tarball has one file; vault has two stale files.
        write(
            &extracted.join(".claude/plugins/onebrain/keep.md"),
            "keep me",
        );
        write(&vault.join(".claude/plugins/onebrain/stale-a.md"), "a");
        write(&vault.join(".claude/plugins/onebrain/stale-b.md"), "b");

        let partial_unlink: UnlinkFn = Box::new(|p: &Path| {
            if p.ends_with("stale-a.md") {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "EACCES",
                ))
            } else {
                fs::remove_file(p)
            }
        });
        let (_added, removed) = sync_plugin_files(&extracted, &vault, &partial_unlink).unwrap();
        // Only stale-b.md actually deleted.
        assert_eq!(removed, 1);
    }

    #[test]
    fn missing_source_plugin_dir_yields_zero_files_added() {
        let dir = tempdir().unwrap();
        let extracted = dir.path().join("extracted");
        let vault = dir.path().join("vault");
        // No source plugin dir created.
        let unlink: UnlinkFn = default_unlink_fn();
        let (added, removed) = sync_plugin_files(&extracted, &vault, &unlink).unwrap();
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
        // mkdir of dest is best-effort — vault plugin dir should exist now.
        assert!(vault.join(".claude/plugins/onebrain").exists());
    }

    // ---- sync_gemini_config ----

    #[test]
    fn gemini_deploys_settings_and_commands() {
        let dir = tempdir().unwrap();
        let extracted = dir.path().join("extracted");
        let vault = dir.path().join("vault");
        write(&extracted.join(".gemini/settings.json"), r#"{"hooks":{}}"#);
        write(
            &extracted.join(".gemini/commands/braindump.toml"),
            "description = \"x\"\n",
        );
        let unlink: UnlinkFn = default_unlink_fn();
        let (added, _removed) = sync_gemini_config(&extracted, &vault, &unlink).unwrap();
        assert_eq!(added, 2);
        assert!(vault.join(".gemini/settings.json").exists());
        assert!(vault.join(".gemini/commands/braindump.toml").exists());
    }

    #[test]
    fn gemini_resync_removes_stale_files() {
        let dir = tempdir().unwrap();
        let extracted = dir.path().join("extracted");
        let vault = dir.path().join("vault");
        write(&vault.join(".gemini/commands/old-name.toml"), "stale");
        write(
            &extracted.join(".gemini/commands/braindump.toml"),
            "description = \"x\"\n",
        );
        let unlink: UnlinkFn = default_unlink_fn();
        let (_added, removed) = sync_gemini_config(&extracted, &vault, &unlink).unwrap();
        assert_eq!(removed, 1);
        assert!(!vault.join(".gemini/commands/old-name.toml").exists());
    }

    #[test]
    fn gemini_absent_in_tarball_is_silent_noop() {
        let dir = tempdir().unwrap();
        let extracted = dir.path().join("extracted");
        let vault = dir.path().join("vault");
        // Create plugin dir to confirm we don't accidentally create .gemini
        write(&extracted.join("README.md"), "");
        let unlink: UnlinkFn = default_unlink_fn();
        let (added, removed) = sync_gemini_config(&extracted, &vault, &unlink).unwrap();
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
        assert!(!vault.join(".gemini").exists());
    }

    // ---- sync_obsidian ----

    #[test]
    fn obsidian_copies_files_when_source_present() {
        let dir = tempdir().unwrap();
        let extracted = dir.path().join("extracted");
        let vault = dir.path().join("vault");
        write(&extracted.join(".obsidian/app.json"), "{}");
        write(&extracted.join(".obsidian/workspace.json"), "{}");
        let added = sync_obsidian(&extracted, &vault);
        assert_eq!(added, 2);
        assert!(vault.join(".obsidian/app.json").exists());
    }

    #[test]
    fn obsidian_skipped_silently_when_source_absent() {
        let dir = tempdir().unwrap();
        let extracted = dir.path().join("extracted");
        let vault = dir.path().join("vault");
        assert_eq!(sync_obsidian(&extracted, &vault), 0);
        assert!(!vault.join(".obsidian").exists());
    }
}
