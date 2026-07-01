//! Standard PARA folder creation.
//!
//! Mirrors `STANDARD_FOLDERS` from `~/projects/onebrain/src/commands/init.ts`
//! plus the `00-inbox/imports` subdirectory. Path joins use `Path::join` so
//! the code works on Windows.

use crate::error::FsError;
use std::path::Path;

/// The eight PARA top-level folders, in display order. Order matters: the
/// Bun source emits them in this sequence and tests pin the assertion.
pub const STANDARD_FOLDERS: [&str; 8] = [
    "00-inbox",
    "01-projects",
    "02-areas",
    "03-knowledge",
    "04-resources",
    "05-agent",
    "06-archive",
    "07-logs",
];

/// `00-inbox/imports` is the only subdirectory init creates eagerly.
pub const INBOX_IMPORTS_SUBDIR: &str = "imports";

/// Idempotent: creates each standard folder + `00-inbox/imports/` if absent.
/// Returns the count of *newly created* paths (skipping ones that already
/// exist). Errors are wrapped in [`FsError::Io`] tagged with the offending
/// path.
pub fn create_folders(vault_dir: &Path) -> Result<usize, FsError> {
    let mut created = 0_usize;

    for folder in STANDARD_FOLDERS {
        let path = vault_dir.join(folder);
        if !path.exists() {
            std::fs::create_dir_all(&path).map_err(|e| FsError::Io {
                path: path.clone(),
                source: e,
            })?;
            created += 1;
        }
    }

    let imports = vault_dir
        .join(STANDARD_FOLDERS[0])
        .join(INBOX_IMPORTS_SUBDIR);
    if !imports.exists() {
        std::fs::create_dir_all(&imports).map_err(|e| FsError::Io {
            path: imports.clone(),
            source: e,
        })?;
        created += 1;
    }

    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn fresh_vault_creates_all_nine_paths() {
        let d = tempdir().unwrap();
        let n = create_folders(d.path()).unwrap();
        assert_eq!(n, 9);
        for folder in STANDARD_FOLDERS {
            assert!(d.path().join(folder).is_dir(), "missing {folder}");
        }
        assert!(d.path().join("00-inbox").join("imports").is_dir());
    }

    #[test]
    fn idempotent_second_call_creates_zero() {
        let d = tempdir().unwrap();
        create_folders(d.path()).unwrap();
        let n = create_folders(d.path()).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn partial_existing_folders_not_double_counted() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("00-inbox")).unwrap();
        fs::create_dir_all(d.path().join("01-projects")).unwrap();
        let n = create_folders(d.path()).unwrap();
        // 8 standard + imports = 9, minus 2 pre-existing = 7
        assert_eq!(n, 7);
    }

    #[test]
    fn pre_existing_imports_subdir_not_double_counted() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("00-inbox").join("imports")).unwrap();
        let n = create_folders(d.path()).unwrap();
        // 8 standard created (inbox already existed because we made imports
        // under it), imports already existed → 7 new.
        assert_eq!(n, 7);
    }

    #[test]
    fn standard_folders_len_is_eight() {
        // Guard against accidental edit to STANDARD_FOLDERS.
        assert_eq!(STANDARD_FOLDERS.len(), 8);
    }

    /// When `vault_dir` is a regular file, `path.exists()` on any sub-path
    /// returns false (ENOTDIR is treated as not-found), so the loop attempts
    /// `create_dir_all` and fails. The error must surface rather than be
    /// swallowed.
    #[test]
    fn create_folders_errors_when_vault_dir_is_a_regular_file() {
        let d = tempdir().unwrap();
        let vault = d.path().join("vault");
        std::fs::write(&vault, "not-a-dir").unwrap();
        // "vault/00-inbox" → path.exists() = false → create_dir_all(vault/00-inbox) → ENOTDIR
        let result = create_folders(&vault);
        assert!(
            result.is_err(),
            "expected Err when vault is a file, got {result:?}"
        );
        let err = result.unwrap_err();
        let debug = format!("{err:?}");
        assert!(
            debug.contains("00-inbox"),
            "expected 00-inbox in error path, got: {debug}"
        );
    }

    /// When `00-inbox` exists as a regular file, the loop skips it
    /// (`path.exists()` → true), but the subsequent `00-inbox/imports`
    /// creation fails because the parent is not a directory. The error must
    /// surface via the imports error path.
    #[test]
    fn imports_error_when_inbox_is_a_file() {
        let d = tempdir().unwrap();
        // A file at the inbox path: the loop skips it, but imports fails.
        std::fs::write(d.path().join(STANDARD_FOLDERS[0]), "block").unwrap();
        let result = create_folders(d.path());
        assert!(
            result.is_err(),
            "expected Err when inbox is a file, got {result:?}"
        );
        let err = result.unwrap_err();
        let debug = format!("{err:?}");
        assert!(
            debug.contains(INBOX_IMPORTS_SUBDIR),
            "expected imports in error path, got: {debug}"
        );
    }
}
