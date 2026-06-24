//! Verbatim note writes — create or overwrite a note's full content. Unlike
//! `new_note` (templates + frontmatter machinery) this writes the bytes it is
//! given, byte-for-byte, so the web editor is the single source of formatting.

use std::path::Path;

use crate::error::Result;
use crate::note::io::atomic_write;
use crate::note::path_out::to_slash; // mirror the import style used in note/new.rs

/// Result of [`write_note`]. `path` is a vault-relative forward-slash string.
#[derive(Debug, serde::Serialize, PartialEq)]
pub struct WriteResult {
    pub path: String,
    /// `true` if the file did not exist before this write.
    pub created: bool,
}

/// Write `content` verbatim to `rel_path` under `vault_root`, creating parent
/// directories as needed. Overwrites unconditionally — existence/conflict
/// policy is the caller's concern (the HTTP layer enforces it).
pub fn write_note(vault_root: &Path, rel_path: &Path, content: &str) -> Result<WriteResult> {
    let abs = vault_root.join(rel_path);
    let created = !abs.exists();
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|source| crate::error::FsError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    atomic_write(&abs, content.as_bytes())?;
    Ok(WriteResult {
        path: to_slash(rel_path),
        created,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    fn read(root: &Path, rel: &str) -> String {
        std::fs::read_to_string(root.join(rel)).unwrap()
    }

    #[test]
    fn writes_new_note_and_reports_created() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let res = write_note(root, Path::new("03-knowledge/a.md"), "# A\nbody\n").unwrap();
        assert_eq!(res.path, "03-knowledge/a.md");
        assert!(res.created);
        assert_eq!(read(root, "03-knowledge/a.md"), "# A\nbody\n");
    }

    #[test]
    fn overwrites_existing_and_reports_not_created() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_note(root, Path::new("a.md"), "old").unwrap();
        let res = write_note(root, Path::new("a.md"), "new").unwrap();
        assert!(!res.created);
        assert_eq!(read(root, "a.md"), "new");
    }
}
