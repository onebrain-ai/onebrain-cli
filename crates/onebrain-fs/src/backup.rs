//! Timestamped config backups — defense-in-depth against accidental config
//! loss.
//!
//! Any operation that overwrites or migrates a vault config file
//! (`onebrain.yml` / legacy `vault.yml`) copies the current contents into
//! `<vault>/.onebrain-backups/` first, so a botched write is always
//! recoverable. Backup is a **hard precondition**: callers propagate a backup
//! failure rather than writing — the contract is "never touch the config
//! without a backup."

use crate::error::{FsError, Result};
use chrono::Local;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Atomic text write: write to `path.tmp` then rename over `path`. Creates
/// parent dirs as needed. Shared home for the helper formerly duplicated in
/// `onebrain-cli`'s `commands::doctor` and `commands::search_model`, which
/// both already depend on this crate (via [`backup_config_file`]).
pub fn atomic_write_text(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.to_path_buf();
    let new_ext = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    };
    tmp.set_extension(new_ext);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Upper bound on the same-second `-N` uniquifier search. Generous enough that
/// it can't be hit by real usage, but bounds the loop so a wedged backup dir
/// can't spin forever.
const MAX_BACKUP_ATTEMPTS: u32 = 10_000;

/// Subdirectory (under the vault root) where config backups are kept. Hidden
/// so it never clutters the vault root or shows up in the PARA folder listing.
pub const BACKUP_DIR: &str = ".onebrain-backups";

/// Copy `config_path` into
/// `<vault>/.onebrain-backups/<filename>.<YYYYMMDD-HHMMSS>.bak` before it is
/// overwritten or migrated.
///
/// Returns `Ok(Some(backup_path))` on success, or `Ok(None)` ONLY when
/// `config_path` is genuinely absent (a fresh write has nothing to back up).
/// Any other stat failure — unreadable parent, EACCES, etc. — propagates so
/// the caller refuses to write past a backup it couldn't make. (`is_file()`
/// would mis-report all of those as "absent" and silently skip the backup,
/// defeating the hard precondition.)
///
/// Timestamp granularity is per-second; a second backup within the same
/// second gets a `-N` uniquifier so no prior backup is ever overwritten.
pub fn backup_config_file(config_path: &Path) -> Result<Option<PathBuf>> {
    // Distinguish "truly absent" (skip) from "present-but-unreadable / symlink"
    // (must back up — or surface the error). `symlink_metadata` doesn't follow
    // links, so a broken symlink registers as present and the later `fs::copy`
    // surfaces the real failure instead of being silently treated as absent.
    match std::fs::symlink_metadata(config_path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(FsError::Io {
                path: config_path.to_path_buf(),
                source,
            })
        }
        Ok(_) => {}
    }
    let vault_root = config_path.parent().unwrap_or_else(|| Path::new("."));
    let backup_dir = vault_root.join(BACKUP_DIR);
    std::fs::create_dir_all(&backup_dir).map_err(|source| FsError::Io {
        path: backup_dir.clone(),
        source,
    })?;

    let filename = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onebrain.yml");
    let stamp = Local::now().format("%Y%m%d-%H%M%S");

    // Never overwrite an existing backup: append `-N` if this second already
    // produced one. Bounded so a wedged backup dir can't spin forever.
    let mut target = backup_dir.join(format!("{filename}.{stamp}.bak"));
    let mut n = 1;
    while target.exists() {
        if n > MAX_BACKUP_ATTEMPTS {
            return Err(FsError::Io {
                path: backup_dir.clone(),
                source: std::io::Error::new(
                    ErrorKind::AlreadyExists,
                    format!("exhausted {MAX_BACKUP_ATTEMPTS} backup name slots for {filename}"),
                ),
            });
        }
        target = backup_dir.join(format!("{filename}.{stamp}-{n}.bak"));
        n += 1;
    }

    std::fs::copy(config_path, &target).map_err(|source| FsError::Io {
        path: target.clone(),
        source,
    })?;
    Ok(Some(target))
}

/// Persist a single `search.<key> = value` into the vault's config file
/// (`onebrain.yml`, or legacy `vault.yml` if that's what's present),
/// preserving every other key. Reads the file (treating a missing/empty file
/// as an empty mapping), ensures a `search:` mapping exists, sets `key`,
/// backs the file up ([`backup_config_file`] is a hard precondition), then
/// atomically writes it back ([`atomic_write_text`]).
///
/// This is the shared home for the read → mutate `search.*` → backup →
/// atomic-write pattern formerly duplicated in `onebrain-cli`'s
/// `search_model::persist_embed_model` and `search_common::persist_collection`.
pub fn persist_search_key(vault_root: &Path, key: &str, value: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    use onebrain_core::{find_config_file, CONFIG_FILENAME};

    let path = find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let mut yaml: serde_yaml::Value = if text.trim().is_empty() {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    } else {
        serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
    };
    if !yaml.is_mapping() {
        yaml = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let mapping = yaml.as_mapping_mut().expect("normalized to mapping above");

    let search_key = serde_yaml::Value::String("search".to_string());
    let needs_replace = match mapping.get(&search_key) {
        Some(v) => !v.is_mapping(),
        None => true,
    };
    if needs_replace {
        mapping.insert(
            search_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let search = mapping
        .get_mut(&search_key)
        .and_then(|v| v.as_mapping_mut())
        .expect("search key was just ensured to be a mapping");
    search.insert(
        serde_yaml::Value::String(key.to_string()),
        serde_yaml::Value::String(value.to_string()),
    );

    let serialized = serde_yaml::to_string(&yaml).context("serializing updated config")?;

    // Defense-in-depth: back up the existing config before overwriting it.
    // Hard precondition — refuse the write if the backup couldn't be made.
    backup_config_file(&path)
        .with_context(|| format!("backing up {} before write", path.display()))?;

    atomic_write_text(&path, &serialized).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn persist_search_key_creates_fresh_config() {
        let dir = tempdir().unwrap();
        persist_search_key(dir.path(), "embed_model", "bge-m3").unwrap();
        let yaml = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("embed_model: bge-m3"));
    }

    #[test]
    fn persist_search_key_preserves_other_keys() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: my-vault\n  embed_model: multilingual-e5-small\nfolders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        persist_search_key(dir.path(), "embed_model", "bge-m3").unwrap();
        let yaml = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("embed_model: bge-m3"));
        assert!(yaml.contains("collection: my-vault"));
        assert!(yaml.contains("inbox: 00-inbox"));
    }

    #[test]
    fn persist_search_key_writes_backup() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  embed_model: multilingual-e5-small\n",
        )
        .unwrap();
        persist_search_key(dir.path(), "embed_model", "bge-m3").unwrap();
        let backup_dir = dir.path().join(BACKUP_DIR);
        assert!(
            backup_dir.is_dir(),
            "expected a config backup to be written"
        );
        assert!(
            std::fs::read_dir(&backup_dir).unwrap().next().is_some(),
            "expected at least one backup file"
        );
    }

    #[test]
    fn persist_search_key_replaces_non_mapping_search_value() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "search: not-a-mapping\n").unwrap();
        persist_search_key(dir.path(), "collection", "c1").unwrap();
        let yaml = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("collection: c1"));
    }

    #[test]
    fn no_file_is_a_noop() {
        let d = tempdir().unwrap();
        let r = backup_config_file(&d.path().join("onebrain.yml")).unwrap();
        assert!(r.is_none());
        assert!(!d.path().join(BACKUP_DIR).exists());
    }

    #[test]
    fn copies_contents_into_backup_dir() {
        let d = tempdir().unwrap();
        let cfg = d.path().join("onebrain.yml");
        std::fs::write(&cfg, "qmd_collection: ob-1\n").unwrap();
        let backup = backup_config_file(&cfg).unwrap().expect("backup made");
        assert!(backup.starts_with(d.path().join(BACKUP_DIR)));
        let name = backup.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("onebrain.yml."), "name: {name}");
        assert!(name.ends_with(".bak"), "name: {name}");
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "qmd_collection: ob-1\n"
        );
        // Original is left in place — backup is a copy, not a move.
        assert!(cfg.is_file());
    }

    #[test]
    fn same_second_backups_do_not_clobber() {
        let d = tempdir().unwrap();
        let cfg = d.path().join("onebrain.yml");
        std::fs::write(&cfg, "v: 1\n").unwrap();
        let b1 = backup_config_file(&cfg).unwrap().unwrap();
        std::fs::write(&cfg, "v: 2\n").unwrap();
        let b2 = backup_config_file(&cfg).unwrap().unwrap();
        assert_ne!(b1, b2, "second backup must not overwrite the first");
        assert_eq!(std::fs::read_to_string(&b1).unwrap(), "v: 1\n");
        assert_eq!(std::fs::read_to_string(&b2).unwrap(), "v: 2\n");
    }

    #[test]
    fn preserves_legacy_filename_in_backup_name() {
        let d = tempdir().unwrap();
        let cfg = d.path().join("vault.yml");
        std::fs::write(&cfg, "method: onebrain\n").unwrap();
        let backup = backup_config_file(&cfg).unwrap().unwrap();
        let name = backup.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("vault.yml."), "name: {name}");
    }

    /// A present-but-uncopyable config (here: a broken symlink) must NOT be
    /// silently treated as absent — that would skip the backup and let the
    /// caller write past a precondition it never satisfied. It must error.
    #[cfg(unix)]
    #[test]
    fn present_but_uncopyable_config_errors_not_skipped() {
        let d = tempdir().unwrap();
        let cfg = d.path().join("onebrain.yml");
        std::os::unix::fs::symlink(d.path().join("does-not-exist.yml"), &cfg).unwrap();
        // Broken symlink: `symlink_metadata` sees it (present), `fs::copy`
        // then fails following it → Err, never Ok(None).
        let r = backup_config_file(&cfg);
        assert!(r.is_err(), "broken-symlink config must error, got {r:?}");
    }

    /// A regular file placed where `.onebrain-backups/` must be created
    /// prevents `create_dir_all` from succeeding — the error must propagate
    /// rather than being silently swallowed.
    #[test]
    fn backup_dir_blocked_by_existing_file_propagates_error() {
        let d = tempdir().unwrap();
        let cfg = d.path().join("onebrain.yml");
        std::fs::write(&cfg, "key: val\n").unwrap();
        // Drop a regular file at the backup-dir path so create_dir_all fails.
        std::fs::write(d.path().join(BACKUP_DIR), "obstacle").unwrap();
        let result = backup_config_file(&cfg);
        assert!(
            result.is_err(),
            "expected Err when backup_dir is a file, got {result:?}"
        );
    }

    /// `symlink_metadata` returning a non-NotFound error (e.g. EACCES on the
    /// parent directory) must surface as an FsError, NOT be treated as "absent".
    #[cfg(unix)]
    #[test]
    fn stat_error_non_notfound_propagates() {
        extern "C" {
            fn geteuid() -> u32;
        }
        if unsafe { geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let restricted = d.path().join("restricted");
        std::fs::create_dir_all(&restricted).unwrap();
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o000)).unwrap();
        // stat("restricted/onebrain.yml") → EACCES, not ENOENT.
        let cfg = restricted.join("onebrain.yml");
        let result = backup_config_file(&cfg);
        // Restore before asserting so tempdir cleanup succeeds.
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            result.is_err(),
            "EACCES on parent must propagate as Err, not become Ok(None); got {result:?}"
        );
    }

    /// `fs::copy` failing on an unreadable source must surface as an error;
    /// `symlink_metadata` (lstat) succeeds even when the file's read-bit is
    /// cleared, so the failure is isolated to the copy step.
    #[cfg(unix)]
    #[test]
    fn copy_fails_on_unreadable_source_propagates_error() {
        extern "C" {
            fn geteuid() -> u32;
        }
        if unsafe { geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let cfg = d.path().join("onebrain.yml");
        std::fs::write(&cfg, "key: val\n").unwrap();
        // symlink_metadata (lstat) only needs parent x-bit — succeeds.
        // fs::copy opens the source for reading — fails on mode 000.
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = backup_config_file(&cfg);
        // Restore before asserting so tempdir cleanup succeeds.
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            result.is_err(),
            "unreadable source must propagate error, got {result:?}"
        );
    }

    #[test]
    fn atomic_write_text_writes_and_cleans_tmp() {
        let d = tempdir().unwrap();
        // No-extension path exercises the "tmp" fallback branch.
        let noext = d.path().join("noextfile");
        atomic_write_text(&noext, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&noext).unwrap(), "hello");
        assert!(!d.path().join("noextfile.tmp").exists(), ".tmp left behind");

        // With-extension path + missing parent dirs are created.
        let nested = d.path().join("deep/nested/file.yml");
        atomic_write_text(&nested, "content").unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "content");
    }
}
