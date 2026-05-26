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
use std::path::{Path, PathBuf};

/// Subdirectory (under the vault root) where config backups are kept. Hidden
/// so it never clutters the vault root or shows up in the PARA folder listing.
pub const BACKUP_DIR: &str = ".onebrain-backups";

/// Copy `config_path` into
/// `<vault>/.onebrain-backups/<filename>.<YYYYMMDD-HHMMSS>.bak` before it is
/// overwritten or migrated.
///
/// Returns `Ok(Some(backup_path))` on success, or `Ok(None)` when
/// `config_path` does not exist (a fresh write has nothing to back up).
/// Errors propagate so the caller can refuse to write when the backup can't
/// be made.
///
/// Timestamp granularity is per-second; a second backup within the same
/// second gets a `-N` uniquifier so no prior backup is ever overwritten.
pub fn backup_config_file(config_path: &Path) -> Result<Option<PathBuf>> {
    if !config_path.is_file() {
        return Ok(None);
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
    // produced one.
    let mut target = backup_dir.join(format!("{filename}.{stamp}.bak"));
    let mut n = 1;
    while target.exists() {
        target = backup_dir.join(format!("{filename}.{stamp}-{n}.bak"));
        n += 1;
    }

    std::fs::copy(config_path, &target).map_err(|source| FsError::Io {
        path: target.clone(),
        source,
    })?;
    Ok(Some(target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
}
