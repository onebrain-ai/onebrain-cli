//! Folder operations for the web UI explorer: create a folder, and soft-delete
//! a folder by moving the whole subtree into `<vault>/.trash/`.

use std::path::Path;

use onebrain_core::CoreError;

use crate::error::{FsError, Result};
use crate::note::delete::DeleteResult; // reuse the {path, trashed_to} shape
use crate::note::path_out::to_slash;

/// Result of [`create_folder`].
#[derive(Debug, serde::Serialize, PartialEq)]
pub struct FolderResult {
    pub path: String,
}

/// Create `rel_path` (and any missing parents). Errors if it already exists.
pub fn create_folder(vault_root: &Path, rel_path: &Path) -> Result<FolderResult> {
    let abs = vault_root.join(rel_path);
    if abs.exists() {
        return Err(FsError::Core(CoreError::InvalidTarget(format!(
            "already exists: {}",
            rel_path.display()
        ))));
    }
    std::fs::create_dir_all(&abs).map_err(|source| FsError::Io {
        path: abs.clone(),
        source,
    })?;
    Ok(FolderResult {
        path: to_slash(rel_path),
    })
}

/// Move a folder (recursively) into `<vault_root>/.trash/<rel_path>`.
pub fn delete_folder(vault_root: &Path, rel_path: &Path) -> Result<DeleteResult> {
    let src = vault_root.join(rel_path);
    if !src.is_dir() {
        return Err(FsError::Core(CoreError::InvalidTarget(format!(
            "no such folder: {}",
            rel_path.display()
        ))));
    }
    let dest_rel = std::path::PathBuf::from(".trash").join(rel_path);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn creates_folder() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let res = create_folder(root, Path::new("01-projects/new")).unwrap();
        assert_eq!(res.path, "01-projects/new");
        assert!(root.join("01-projects/new").is_dir());
    }

    #[test]
    fn create_existing_errors() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("p")).unwrap();
        assert!(create_folder(root, Path::new("p")).is_err());
    }

    #[test]
    fn deletes_folder_into_trash() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("p/sub")).unwrap();
        std::fs::write(root.join("p/sub/a.md"), "x").unwrap();

        let res = delete_folder(root, Path::new("p")).unwrap();
        assert_eq!(res.trashed_to, ".trash/p");
        assert!(!root.join("p").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(".trash/p/sub/a.md")).unwrap(),
            "x"
        );
    }
}
