//! Shared filesystem primitives for the `note` write verbs.
//!
//! Extracted so `append`, `new`, and `move` share ONE atomic-write
//! implementation instead of three copy-pasted ones (each of which previously
//! orphaned its `{path}.tmp` on a rename failure).

use crate::error::{FsError, Result};
use std::path::Path;

/// Atomic write via `{path}.tmp` + `rename`.
///
/// Writes `bytes` to a sibling temp file (`{path}.tmp`), then renames it over
/// `path` so readers never observe a partially-written file. On a rename
/// failure the temp file is removed best-effort before the error is returned,
/// so a failed write never leaves a stray `.tmp` behind.
pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.to_path_buf();
    let new_ext = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    };
    tmp.set_extension(new_ext);
    std::fs::write(&tmp, bytes).map_err(|source| FsError::Io {
        path: tmp.clone(),
        source,
    })?;
    if let Err(source) = std::fs::rename(&tmp, path) {
        // Rename failed → the temp file is now orphaned. Clean it up
        // best-effort before surfacing the original rename error.
        let _ = std::fs::remove_file(&tmp);
        return Err(FsError::Io {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn happy_path_writes_and_leaves_no_tmp() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("note.md");
        atomic_write(&path, b"hello\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
        assert!(!dir.path().join("note.tmp").exists());
        assert!(!dir.path().join("note.md.tmp").exists());
    }

    /// Force the `rename` to fail by making `path` an existing DIRECTORY:
    /// you can't rename a file over a directory. Assert the orphaned `.tmp`
    /// is cleaned up.
    #[test]
    fn rename_failure_cleans_up_tmp() {
        let dir = tempdir().unwrap();
        // `path` is a directory → rename(tmp, path) fails (ENOTDIR/EISDIR/etc).
        let path = dir.path().join("target");
        fs::create_dir(&path).unwrap();

        let err = atomic_write(&path, b"data").unwrap_err();
        assert!(matches!(err, FsError::Io { .. }));

        // The temp file must NOT be left behind.
        let tmp = dir.path().join("target.tmp");
        assert!(!tmp.exists(), "orphaned tmp must be cleaned up");
    }
}
