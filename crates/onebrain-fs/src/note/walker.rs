//! Vault `.md` enumeration shared by every `note` verb.

use crate::error::{FsError, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// VCS / editor / plugin-internal directories that never hold user content —
/// pruned by every walk.
const TOOLING_DIRS: &[&str] = &[".git", ".obsidian", ".claude", ".trash", "node_modules"];

/// Additional dirs pruned by [`walk_notes`] (live-note verbs): binary
/// attachments and the archive bucket. `find` walks these (it's a raw finder).
const NOTE_EXTRA_SKIP: &[&str] = &["attachments", "06-archive"];

/// Enumerate Markdown (`.md`) files under `vault_root`, optionally scoped to
/// `folder` (relative to the vault root). Tooling + archive directories
/// (`TOOLING_DIRS` + `NOTE_EXTRA_SKIP`) are pruned at any depth. Results are
/// sorted for deterministic output.
pub fn walk_notes(vault_root: &Path, folder: Option<&Path>) -> Result<Vec<PathBuf>> {
    let start = match folder {
        Some(f) => vault_root.join(f),
        None => vault_root.to_path_buf(),
    };
    let mut out = Vec::new();
    for entry in WalkDir::new(&start)
        .into_iter()
        .filter_entry(|e| !is_skipped_note_dir(e))
    {
        let entry = entry.map_err(|e| map_walk_err(e, &start))?;
        let path = entry.path();
        if entry.file_type().is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
            out.push(path.to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

/// Enumerate Markdown (`.md`) files under `vault_root` INCLUDING the archive
/// bucket (`06-archive`), pruning only tooling dirs ([`TOOLING_DIRS`]) and
/// binary attachments. Used by `note backlinks --include-archive`.
///
/// Note: [`walk_notes`] prunes any dir named `06-archive` at any depth — even
/// when passed as the start dir — so it can't be used to include the archive.
/// This walks everything via [`walk_all`] and filters down to `.md` files.
pub(super) fn walk_notes_with_archive(vault_root: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = walk_all(vault_root)?
        .into_iter()
        .filter(|(path, is_dir)| {
            !is_dir
                && path.extension().and_then(|s| s.to_str()) == Some("md")
                && !path.components().any(|c| c.as_os_str() == "attachments")
        })
        .map(|(path, _)| path)
        .collect();
    out.sort();
    Ok(out)
}

/// Walk every file and directory under `vault_root`, pruning only tooling dirs
/// ([`TOOLING_DIRS`]). Returns `(absolute_path, is_dir)` pairs (excluding the
/// root itself), sorted. Used by `note find`, which — unlike [`walk_notes`] —
/// is not limited to `.md` and includes archive / logs.
pub(super) fn walk_all(vault_root: &Path) -> Result<Vec<(PathBuf, bool)>> {
    let mut out = Vec::new();
    for entry in WalkDir::new(vault_root)
        .into_iter()
        .filter_entry(|e| !is_tooling_dir(e))
    {
        let entry = entry.map_err(|e| map_walk_err(e, vault_root))?;
        if entry.depth() == 0 {
            continue; // skip the vault root itself
        }
        out.push((entry.path().to_path_buf(), entry.file_type().is_dir()));
    }
    out.sort();
    Ok(out)
}

fn dir_name_in(entry: &walkdir::DirEntry, list: &[&str]) -> bool {
    entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .map(|n| list.contains(&n))
            .unwrap_or(false)
}

fn is_skipped_note_dir(entry: &walkdir::DirEntry) -> bool {
    dir_name_in(entry, TOOLING_DIRS) || dir_name_in(entry, NOTE_EXTRA_SKIP)
}

fn is_tooling_dir(entry: &walkdir::DirEntry) -> bool {
    dir_name_in(entry, TOOLING_DIRS)
}

fn map_walk_err(e: walkdir::Error, fallback: &Path) -> FsError {
    let path = e
        .path()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| fallback.to_path_buf());
    FsError::Io {
        path,
        source: e.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "# note\n").unwrap();
    }

    fn rel_strings(root: &Path, paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn walk_notes_lists_md_skipping_system_and_archive_dirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch(root, "note1.md");
        touch(root, "03-knowledge/ml/note2.md");
        touch(root, "06-archive/2026/old.md");
        touch(root, ".obsidian/workspace.md");
        touch(root, ".git/config.md");
        touch(root, "attachments/scan.md");
        touch(root, "notes.txt"); // non-md ignored

        let got = walk_notes(root, None).unwrap();
        assert_eq!(
            rel_strings(root, &got),
            vec!["03-knowledge/ml/note2.md", "note1.md"]
        );
    }

    #[test]
    fn walk_notes_with_archive_includes_archive_skips_attachments_and_nonmd() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch(root, "note1.md");
        touch(root, "06-archive/2026/old.md");
        touch(root, ".git/config.md"); // tooling pruned
        touch(root, "attachments/scan.md"); // attachments pruned
        touch(root, "notes.txt"); // non-md ignored

        let got = walk_notes_with_archive(root).unwrap();
        assert_eq!(
            rel_strings(root, &got),
            vec!["06-archive/2026/old.md", "note1.md"]
        );
    }

    #[test]
    fn walk_notes_scopes_to_folder() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch(root, "note1.md");
        touch(root, "03-knowledge/note2.md");
        touch(root, "01-projects/p.md");

        let got = walk_notes(root, Some(Path::new("03-knowledge"))).unwrap();
        assert_eq!(rel_strings(root, &got), vec!["03-knowledge/note2.md"]);
    }
}
