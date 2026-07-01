//! Soft-delete a note by moving it into `<vault>/.trash/`, preserving its
//! relative path. On a name collision a timestamp suffix is appended so no
//! trashed note is ever clobbered. Recoverable; mirrors Obsidian's local trash.

use std::path::{Path, PathBuf};

use chrono::Utc;

use onebrain_core::CoreError;

use super::path_out::to_slash;
use crate::error::{FsError, Result};

/// Result of [`delete_note`]. Both fields are vault-relative slash strings.
#[derive(Debug, serde::Serialize, PartialEq)]
pub struct DeleteResult {
    pub path: String,
    pub trashed_to: String,
}

/// Move `rel_path` into `<vault_root>/.trash/<rel_path>`. Errors if the source
/// does not exist.
pub fn delete_note(vault_root: &Path, rel_path: &Path) -> Result<DeleteResult> {
    let src = vault_root.join(rel_path);
    if !src.exists() {
        return Err(FsError::Core(CoreError::InvalidTarget(format!(
            "no such note: {}",
            rel_path.display()
        ))));
    }
    let dest_rel = trash_dest(rel_path, &vault_root.join(".trash"));
    let dest = vault_root.join(&dest_rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|source| FsError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::rename(&src, &dest).map_err(|source| FsError::Io {
        path: dest.clone(),
        source,
    })?;
    Ok(DeleteResult {
        path: to_slash(rel_path),
        trashed_to: to_slash(&dest_rel),
    })
}

/// `.trash/<rel>` — with a ` (YYYYMMDD-HHMMSS)` suffix before the extension if
/// that target already exists.
fn trash_dest(rel_path: &Path, trash_root: &Path) -> PathBuf {
    let plain = trash_root.join(rel_path);
    if !plain.exists() {
        return PathBuf::from(".trash").join(rel_path);
    }
    let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let stem = rel_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("note");
    let ext = rel_path.extension().and_then(|s| s.to_str());
    let fname = match ext {
        Some(ext) => format!("{stem} ({stamp}).{ext}"),
        None => format!("{stem} ({stamp})"),
    };
    let parent = rel_path.parent().unwrap_or_else(|| Path::new(""));
    PathBuf::from(".trash").join(parent).join(fname)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn moves_note_into_trash() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("p")).unwrap();
        std::fs::write(root.join("p/a.md"), "x").unwrap();

        let res = delete_note(root, Path::new("p/a.md")).unwrap();
        assert_eq!(res.path, "p/a.md");
        assert_eq!(res.trashed_to, ".trash/p/a.md");
        assert!(!root.join("p/a.md").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(".trash/p/a.md")).unwrap(),
            "x"
        );
    }

    #[test]
    fn collision_gets_timestamp_suffix() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".trash")).unwrap();
        std::fs::write(root.join(".trash/a.md"), "old").unwrap();
        std::fs::write(root.join("a.md"), "new").unwrap();

        let res = delete_note(root, Path::new("a.md")).unwrap();
        assert_ne!(res.trashed_to, ".trash/a.md"); // suffixed
        assert!(res.trashed_to.starts_with(".trash/a ("));
        assert!(res.trashed_to.ends_with(").md"));
        assert_eq!(
            std::fs::read_to_string(root.join(".trash/a.md")).unwrap(),
            "old"
        );
    }

    #[test]
    fn missing_source_errors() {
        let dir = tempdir().unwrap();
        assert!(delete_note(dir.path(), Path::new("nope.md")).is_err());
    }

    #[test]
    fn collision_extensionless_gets_timestamp_suffix() {
        // Exercises the `None => format!("{stem} ({stamp})")` branch in trash_dest
        // when the colliding file has no extension.
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".trash")).unwrap();
        std::fs::write(root.join(".trash/noext"), "old").unwrap();
        std::fs::write(root.join("noext"), "new").unwrap();

        let res = delete_note(root, Path::new("noext")).unwrap();
        // Should differ from the plain path and carry a timestamp suffix.
        assert_ne!(res.trashed_to, ".trash/noext");
        assert!(
            res.trashed_to.starts_with(".trash/noext ("),
            "expected timestamp suffix, got: {}",
            res.trashed_to
        );
        // No file extension in the suffixed name.
        assert!(
            res.trashed_to.ends_with(')'),
            "expected no extension, got: {}",
            res.trashed_to
        );
        // Original trashed file is untouched.
        assert_eq!(
            std::fs::read_to_string(root.join(".trash/noext")).unwrap(),
            "old"
        );
    }
}
