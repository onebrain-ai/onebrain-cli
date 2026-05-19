//! Top-level orchestrator — stitches steps 1–9 into the public
//! `run_vault_sync` entry point. Port of Bun's `runVaultSync`.

use chrono::Utc;
use is_terminal::IsTerminal;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use onebrain_core::Harness;

use super::branch::resolve_branch;
use super::cache_clean::clean_plugin_cache;
use super::docs::copy_root_docs;
use super::download::{default_fetch_fn, download_tarball, extract_tarball};
use super::harness_merge::merge_harness_files;
use super::pin::pin_to_vault;
use super::progress::build_progress;
use super::sync::{default_unlink_fn, sync_gemini_config, sync_obsidian, sync_plugin_files};
use super::types::{NowFn, VaultSyncOptions, VaultSyncResult};
use super::vault_yml::update_vault_yml;
use crate::harness::detect_harness;

/// Top-level entry — `runVaultSync` parity. Always returns a `VaultSyncResult`;
/// callers route `!result.ok` to `process::exit(1)` themselves.
pub fn run_vault_sync(vault_root: &Path, opts: VaultSyncOptions) -> VaultSyncResult {
    // 0. Resolve options
    let fetch = opts.fetch_fn.unwrap_or_else(default_fetch_fn);
    let unlink = opts.unlink_fn.unwrap_or_else(default_unlink_fn);
    let now: NowFn = opts.now_fn.unwrap_or_else(|| Box::new(Utc::now));
    let is_tty = opts
        .is_tty
        .unwrap_or_else(|| std::io::stdout().is_terminal());

    // Load vault.yml for update_channel — best-effort.
    let update_channel = read_update_channel(vault_root).unwrap_or_else(|| "stable".to_string());
    let branch = opts
        .branch
        .unwrap_or_else(|| resolve_branch(Some(&update_channel)).to_string());

    let installed_plugins_path = opts
        .installed_plugins_path
        .or_else(env_override_installed_plugins_path)
        .or_else(default_installed_plugins_path)
        .unwrap_or_else(|| vault_root.join(".isolated-installed_plugins.json"));
    let installed_plugins_cache_dir = opts
        .installed_plugins_cache_dir
        .or_else(env_override_cache_dir);

    let harness = detect_harness(vault_root);

    let mut result = VaultSyncResult::pending(branch.clone());
    let mut progress = build_progress(is_tty, opts.embedded);
    progress.intro("OneBrain Vault Sync");

    // Holds the temp dir until end of function — drop() removes it.
    let tmp_dir = match tempfile::tempdir_in(std::env::temp_dir()) {
        Ok(d) => d,
        Err(e) => {
            result.error = Some(format!("create temp dir: {e}"));
            eprintln!(
                "vault-sync: download failed: {}",
                result.error.as_ref().unwrap()
            );
            return result;
        }
    };

    // ── Step 1 · download + extract ────────────────────────────────────────
    progress.start("📥", "Downloading");
    let extracted_dir = match download_and_extract(&branch, &fetch, tmp_dir.path()) {
        Ok(p) => p,
        Err(msg) => {
            progress.stop("download failed");
            eprintln!("vault-sync: download failed: {msg}");
            result.error = Some(msg);
            return result;
        }
    };

    // Read version from the extracted plugin.json (before sync overwrites it).
    if let Some(v) = read_plugin_version(&extracted_dir) {
        result.version = v;
    }
    progress.stop(&format!(
        "onebrain-ai/onebrain@{branch} (v{})",
        result.version
    ));

    // ── Step 2 · plugin + step 3 · .gemini sync ────────────────────────────
    progress.start("📂", "Syncing files");
    let (added, removed) = match (
        sync_plugin_files(&extracted_dir, vault_root, &unlink),
        sync_gemini_config(&extracted_dir, vault_root, &unlink),
    ) {
        (Ok(p), Ok(g)) => (p.0 + g.0, p.1 + g.1),
        (Err(e), _) | (_, Err(e)) => {
            progress.stop("plugin sync failed");
            eprintln!("vault-sync: plugin sync failed: {e}");
            result.error = Some(e.to_string());
            return result;
        }
    };
    result.files_added = added;
    result.files_removed = removed;
    progress.stop(&format!(
        "{added} file{} synced, {removed} removed",
        if added == 1 { "" } else { "s" }
    ));

    // ── Step 4 · .obsidian (init only) ─────────────────────────────────────
    if opts.include_obsidian {
        sync_obsidian(&extracted_dir, vault_root);
    }

    // ── Step 5 · root docs (non-fatal) ─────────────────────────────────────
    copy_root_docs(&extracted_dir, vault_root);

    // ── Step 6 · harness merge ─────────────────────────────────────────────
    progress.start("🔧", "Updating harness");
    let imports_added = match merge_harness_files(&extracted_dir, vault_root) {
        Ok(n) => n,
        Err(e) => {
            progress.stop("harness merge failed");
            eprintln!("vault-sync: harness merge failed: {e}");
            result.error = Some(e.to_string());
            return result;
        }
    };
    result.imports_added = imports_added;
    if imports_added > 0 {
        progress.stop(&format!(
            "{imports_added} import{} added",
            if imports_added == 1 { "" } else { "s" }
        ));
    } else {
        progress.stop("harness files up-to-date");
    }

    // ── Step 7 · vault.yml update ──────────────────────────────────────────
    if let Err(e) = update_vault_yml(vault_root, &update_channel) {
        eprintln!("vault-sync: vault.yml update failed: {e}");
        result.error = Some(e.to_string());
        return result;
    }

    // ── Steps 8 + 9 · claude-only · non-fatal ──────────────────────────────
    if harness == Harness::Claude {
        progress.start("📌", "Pinning to vault");
        match pin_to_vault(
            vault_root,
            &installed_plugins_path,
            installed_plugins_cache_dir.as_deref(),
            &now,
        ) {
            Ok(pin) => {
                result.pin_skipped = pin.skipped;
                if pin.skipped {
                    progress.stop("pin skipped (not found or marketplace)");
                } else {
                    progress.stop("installPath → .claude/plugins/onebrain");
                }
            }
            Err(e) => {
                eprintln!("vault-sync: pin warning: {e}");
                result.pin_skipped = true;
                progress.stop("pin skipped (error — non-fatal)");
            }
        }

        progress.start("🧹", "Cleaning cache");
        let cache_removed = clean_plugin_cache(
            &installed_plugins_path,
            installed_plugins_cache_dir.as_deref(),
        );
        result.cache_removed = cache_removed;
        if cache_removed > 0 {
            progress.stop(&format!(
                "{cache_removed} cached version{} removed",
                if cache_removed == 1 { "" } else { "s" }
            ));
        } else {
            progress.stop("no cache to clean");
        }
    }

    result.ok = true;
    progress.outro(&format!("Done — v{} synced", result.version));

    // tmp_dir drops here — cleanup is best-effort.
    drop(tmp_dir);
    result
}

fn read_update_channel(vault_root: &Path) -> Option<String> {
    let text = fs::read_to_string(vault_root.join("vault.yml")).ok()?;
    let v: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    v.get("update_channel")
        .and_then(|x| x.as_str())
        .map(str::to_string)
}

fn read_plugin_version(extracted_dir: &Path) -> Option<String> {
    let path = extracted_dir.join(".claude/plugins/onebrain/.claude-plugin/plugin.json");
    let text = fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn default_installed_plugins_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude/plugins/installed_plugins.json"))
}

/// Test escape hatch — `ONEBRAIN_INSTALLED_PLUGINS_PATH` overrides the default
/// `~/.claude/plugins/installed_plugins.json` location. Lets integration tests
/// scope the pin step to a temp dir without touching the real home.
fn env_override_installed_plugins_path() -> Option<PathBuf> {
    std::env::var_os("ONEBRAIN_INSTALLED_PLUGINS_PATH").map(PathBuf::from)
}

/// Test escape hatch — overrides the plugins cache dir. Paired with the
/// `installed_plugins_path` override above.
fn env_override_cache_dir() -> Option<PathBuf> {
    std::env::var_os("ONEBRAIN_INSTALLED_PLUGINS_CACHE_DIR").map(PathBuf::from)
}

fn download_and_extract(
    branch: &str,
    fetch: &super::types::FetchFn,
    tmp_dir: &Path,
) -> Result<PathBuf, String> {
    let bytes = download_tarball(branch, fetch)?;
    extract_tarball(&bytes, tmp_dir).map_err(|e| format!("tar extraction failed: {e}"))
}

// Stash the unused `Write` import so cargo doesn't flag it during refactors.
#[allow(dead_code)]
fn _silence_write<W: Write>(_: W) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault_sync::types::FetchFn;
    use chrono::TimeZone;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::fs;
    use tempfile::tempdir;

    /// Build a minimal mock tarball with a fixed prefix.
    fn build_tarball(prefix: &str, version: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let enc = GzEncoder::new(&mut buf, Compression::default());
            let mut b = tar::Builder::new(enc);
            let files: Vec<(String, String)> = vec![
                (
                    format!("{prefix}/.claude/plugins/onebrain/.claude-plugin/plugin.json"),
                    serde_json::json!({"id":"onebrain","version":version,"name":"OneBrain"})
                        .to_string(),
                ),
                (
                    format!("{prefix}/.claude/plugins/onebrain/INSTRUCTIONS.md"),
                    "# OneBrain Instructions\n".into(),
                ),
                (
                    format!("{prefix}/.claude/plugins/onebrain/skills/example/SKILL.md"),
                    "# Example Skill\n".into(),
                ),
                (
                    format!("{prefix}/CONTRIBUTING.md"),
                    "# Contributing\n".into(),
                ),
                (format!("{prefix}/CHANGELOG.md"), "# Changelog\n".into()),
                (
                    format!("{prefix}/CLAUDE.md"),
                    "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
                ),
                (
                    format!("{prefix}/GEMINI.md"),
                    "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
                ),
                (
                    format!("{prefix}/AGENTS.md"),
                    "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
                ),
            ];
            for (path, content) in files {
                let mut h = tar::Header::new_gnu();
                h.set_size(content.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, &path, content.as_bytes()).unwrap();
            }
            b.finish().unwrap();
        }
        buf
    }

    fn mock_fetch_with(bytes: Vec<u8>) -> FetchFn {
        Box::new(move |_url: &str| Ok(bytes.clone()))
    }

    fn fixed_now() -> NowFn {
        Box::new(|| Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap())
    }

    fn make_vault(tmp: &Path) -> PathBuf {
        let vault = tmp.join("vault");
        fs::create_dir_all(vault.join(".claude")).unwrap();
        fs::write(
            vault.join("vault.yml"),
            "update_channel: stable\nfolders:\n  inbox: 00-inbox\n  logs: 07-logs\n",
        )
        .unwrap();
        vault
    }

    #[test]
    fn fresh_sync_returns_ok_and_writes_plugin_files() {
        let dir = tempdir().unwrap();
        let vault = make_vault(dir.path());
        let bytes = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0");
        let isolated = vault.join(".isolated-installed_plugins.json");

        let result = run_vault_sync(
            &vault,
            VaultSyncOptions {
                fetch_fn: Some(mock_fetch_with(bytes)),
                installed_plugins_path: Some(isolated),
                is_tty: Some(false),
                now_fn: Some(fixed_now()),
                ..Default::default()
            },
        );

        assert!(result.ok, "result: {result:?}");
        assert!(result.files_added > 0);
        assert_eq!(result.files_removed, 0);
        assert_eq!(result.version, "1.11.0");
        let plugin_json = vault.join(".claude/plugins/onebrain/.claude-plugin/plugin.json");
        let pj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&plugin_json).unwrap()).unwrap();
        assert_eq!(pj["version"], "1.11.0");
    }

    #[test]
    fn sync_with_stale_files_removes_them() {
        let dir = tempdir().unwrap();
        let vault = make_vault(dir.path());
        // Pre-populate a stale file.
        let plugin_dir = vault.join(".claude/plugins/onebrain");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join("stale.md"), "# stale").unwrap();

        let bytes = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0");
        let isolated = vault.join(".isolated-installed_plugins.json");
        let result = run_vault_sync(
            &vault,
            VaultSyncOptions {
                fetch_fn: Some(mock_fetch_with(bytes)),
                installed_plugins_path: Some(isolated),
                is_tty: Some(false),
                now_fn: Some(fixed_now()),
                ..Default::default()
            },
        );
        assert!(result.ok);
        assert!(result.files_removed >= 1);
        assert!(!plugin_dir.join("stale.md").exists());
    }

    #[test]
    fn download_failure_returns_not_ok() {
        let dir = tempdir().unwrap();
        let vault = make_vault(dir.path());
        let isolated = vault.join(".isolated-installed_plugins.json");
        let fetch: FetchFn = Box::new(|_url| {
            Err("HTTP 404 downloading tarball from x — repo or branch not found".into())
        });
        let result = run_vault_sync(
            &vault,
            VaultSyncOptions {
                fetch_fn: Some(fetch),
                installed_plugins_path: Some(isolated),
                is_tty: Some(false),
                ..Default::default()
            },
        );
        assert!(!result.ok);
        assert!(result.error.as_ref().unwrap().contains("404"));
    }

    #[test]
    fn corrupted_tarball_returns_not_ok_with_error_set() {
        let dir = tempdir().unwrap();
        let vault = make_vault(dir.path());
        let isolated = vault.join(".isolated-installed_plugins.json");
        let fetch: FetchFn = Box::new(|_| Ok(vec![0u8, 1, 2, 3, 0xff]));
        let result = run_vault_sync(
            &vault,
            VaultSyncOptions {
                fetch_fn: Some(fetch),
                installed_plugins_path: Some(isolated),
                is_tty: Some(false),
                ..Default::default()
            },
        );
        assert!(!result.ok);
        assert!(result.error.is_some());
    }

    #[test]
    fn http_403_carries_through_to_error_field() {
        let dir = tempdir().unwrap();
        let vault = make_vault(dir.path());
        let isolated = vault.join(".isolated-installed_plugins.json");
        let fetch: FetchFn = Box::new(|_| {
            Err("HTTP 403 downloading tarball — check repo permissions or GITHUB_TOKEN".into())
        });
        let result = run_vault_sync(
            &vault,
            VaultSyncOptions {
                fetch_fn: Some(fetch),
                installed_plugins_path: Some(isolated),
                is_tty: Some(false),
                ..Default::default()
            },
        );
        assert!(!result.ok);
        assert!(result.error.as_ref().unwrap().contains("403"));
    }

    #[test]
    fn vault_yml_update_channel_preserved() {
        let dir = tempdir().unwrap();
        let vault = make_vault(dir.path());
        let bytes = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0");
        let isolated = vault.join(".isolated-installed_plugins.json");
        let result = run_vault_sync(
            &vault,
            VaultSyncOptions {
                fetch_fn: Some(mock_fetch_with(bytes)),
                installed_plugins_path: Some(isolated),
                is_tty: Some(false),
                now_fn: Some(fixed_now()),
                ..Default::default()
            },
        );
        assert!(result.ok);
        let yml = fs::read_to_string(vault.join("vault.yml")).unwrap();
        assert!(yml.contains("update_channel: stable"));
        assert!(!yml.contains("onebrain_version"));
    }

    #[test]
    fn harness_merge_injects_new_imports() {
        let dir = tempdir().unwrap();
        let vault = make_vault(dir.path());
        fs::write(
            vault.join("CLAUDE.md"),
            "# My Config\n\n@.claude/plugins/onebrain/INSTRUCTIONS.md\n",
        )
        .unwrap();

        // Tarball CLAUDE.md has an extra import.
        let mut bytes = Vec::new();
        {
            let enc = GzEncoder::new(&mut bytes, Compression::default());
            let mut b = tar::Builder::new(enc);
            let files = [
                (
                    "onebrain-ai-onebrain-abc/.claude/plugins/onebrain/.claude-plugin/plugin.json",
                    serde_json::json!({"id":"onebrain","version":"1.11.0"}).to_string(),
                ),
                (
                    "onebrain-ai-onebrain-abc/CLAUDE.md",
                    "@.claude/plugins/onebrain/INSTRUCTIONS.md\n@.claude/plugins/onebrain/NEW.md\n"
                        .into(),
                ),
                (
                    "onebrain-ai-onebrain-abc/GEMINI.md",
                    "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
                ),
                (
                    "onebrain-ai-onebrain-abc/AGENTS.md",
                    "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
                ),
            ];
            for (p, c) in files {
                let mut h = tar::Header::new_gnu();
                h.set_size(c.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, p, c.as_bytes()).unwrap();
            }
            b.finish().unwrap();
        }
        let isolated = vault.join(".isolated-installed_plugins.json");
        let result = run_vault_sync(
            &vault,
            VaultSyncOptions {
                fetch_fn: Some(mock_fetch_with(bytes)),
                installed_plugins_path: Some(isolated),
                is_tty: Some(false),
                now_fn: Some(fixed_now()),
                ..Default::default()
            },
        );
        assert!(result.ok, "result: {result:?}");
        assert!(result.imports_added >= 1);
        let claude = fs::read_to_string(vault.join("CLAUDE.md")).unwrap();
        assert!(claude.contains("# My Config"));
        assert!(claude.contains("@.claude/plugins/onebrain/NEW.md"));
        let dup = claude
            .matches("@.claude/plugins/onebrain/INSTRUCTIONS.md")
            .count();
        assert_eq!(dup, 1);
    }
}
