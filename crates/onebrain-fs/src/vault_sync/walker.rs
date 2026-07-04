//! Recursive file listing helper · shared by `sync_plugin_files`
//! and `sync_gemini_config`.
//!
//! Port of Bun's `listFilesRecursive` (vault-sync.ts) which:
//!   - returns full paths (not relative)
//!   - swallows readdir / stat errors at any level (best-effort traversal)
//!   - does not follow symlinks (Bun's behavior — `stat` follows but `isDirectory`
//!     on a symlink-to-dir returns true, so it would; we keep parity with the
//!     test fixtures by following symlinks the same way `WalkDir::follow_links(true)`
//!     does, but in practice the OneBrain release tarball contains no symlinks).

use std::path::{Path, PathBuf};

/// List all regular files under `dir` recursively. Returns full paths.
/// Missing or unreadable directory → empty Vec (no panic, no error).
pub fn list_files_recursive(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.exists() {
        return out;
    }
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            out.push(entry.into_path());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn nested_files_collected() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        fs::write(dir.path().join("a/x.md"), "1").unwrap();
        fs::write(dir.path().join("a/b/y.md"), "2").unwrap();
        fs::write(dir.path().join("a/b/c/z.md"), "3").unwrap();
        let mut got = list_files_recursive(dir.path());
        got.sort();
        assert_eq!(got.len(), 3);
        assert!(
            got[0].ends_with("a/b/c/z.md")
                || got[0].ends_with("a/b/y.md")
                || got[0].ends_with("a/x.md")
        );
    }

    #[test]
    fn missing_dir_returns_empty_vec() {
        let dir = tempdir().unwrap();
        let nonexistent = dir.path().join("ghost");
        assert!(list_files_recursive(&nonexistent).is_empty());
    }

    #[test]
    fn empty_dir_returns_empty_vec() {
        let dir = tempdir().unwrap();
        assert!(list_files_recursive(dir.path()).is_empty());
    }

    #[test]
    fn excludes_directories_returns_only_files() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("file.md"), "x").unwrap();
        let got = list_files_recursive(dir.path());
        assert_eq!(got.len(), 1);
        assert!(got[0].ends_with("file.md"));
    }
}
