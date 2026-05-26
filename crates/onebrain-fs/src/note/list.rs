//! `note list` — enumerate notes with metadata, sorted.

use super::walker::walk_notes;
use crate::error::{FsError, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Sort order for [`list_notes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSort {
    /// Alphabetical by vault-relative path.
    Name,
    /// Most-recently-modified first (default).
    Mtime,
    /// Most-recently-created first (entries without a created time sort last).
    Created,
}

/// Inputs for [`list_notes`].
pub struct ListOptions {
    pub folder: Option<PathBuf>,
    pub limit: usize,
    pub sort: ListSort,
}

/// A note plus its filesystem metadata. `path` is vault-relative; `title` is
/// the first `# H1` heading, falling back to the filename stem.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct NoteEntry {
    pub path: PathBuf,
    pub title: String,
    pub modified: DateTime<Utc>,
    pub created: Option<DateTime<Utc>>,
    pub size_bytes: u64,
}

/// Result of [`list_notes`].
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ListResult {
    pub notes: Vec<NoteEntry>,
    pub total: usize,
}

/// List notes under the vault (or `opts.folder`), sorted per `opts.sort`,
/// capped to `opts.limit`. `total` counts all notes before the limit.
pub fn list_notes(vault_root: &Path, opts: &ListOptions) -> Result<ListResult> {
    let files = walk_notes(vault_root, opts.folder.as_deref())?;
    let total = files.len();

    let mut entries = Vec::with_capacity(files.len());
    for file in &files {
        let meta = std::fs::metadata(file).map_err(|e| FsError::Io {
            path: file.clone(),
            source: e,
        })?;
        // Content is read only to extract the H1 title; non-UTF-8 falls back to
        // the filename stem.
        let content = std::fs::read_to_string(file).unwrap_or_default();
        let modified: DateTime<Utc> =
            meta.modified()
                .map(DateTime::<Utc>::from)
                .map_err(|e| FsError::Io {
                    path: file.clone(),
                    source: e,
                })?;
        let created = meta.created().ok().map(DateTime::<Utc>::from);
        let rel = file.strip_prefix(vault_root).unwrap_or(file).to_path_buf();
        entries.push(NoteEntry {
            title: title_for(file, &content),
            path: rel,
            modified,
            created,
            size_bytes: meta.len(),
        });
    }

    match opts.sort {
        ListSort::Name => entries.sort_by(|a, b| a.path.cmp(&b.path)),
        // Desc, ties broken by path for determinism.
        ListSort::Mtime => entries.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.path.cmp(&b.path))
        }),
        // Desc; entries without a created time (None) sort last.
        ListSort::Created => {
            entries.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.path.cmp(&b.path)))
        }
    }
    entries.truncate(opts.limit);

    Ok(ListResult {
        notes: entries,
        total,
    })
}

/// First `# H1` heading, else the filename stem.
fn title_for(path: &Path, content: &str) -> String {
    content
        .lines()
        .find_map(|l| {
            l.strip_prefix("# ")
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("untitled")
                .to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use std::fs;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        p
    }

    fn list_opts(sort: ListSort, limit: usize) -> ListOptions {
        ListOptions {
            folder: None,
            limit,
            sort,
        }
    }

    #[test]
    fn sorts_by_mtime_desc_by_default() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let a = write(root, "a.md", "# A\n");
        let b = write(root, "b.md", "# B\n");
        set_file_mtime(&a, FileTime::from_unix_time(1_000, 0)).unwrap();
        set_file_mtime(&b, FileTime::from_unix_time(2_000, 0)).unwrap();

        let res = list_notes(root, &list_opts(ListSort::Mtime, 20)).unwrap();
        assert_eq!(res.total, 2);
        let order: Vec<_> = res.notes.iter().map(|n| n.path.clone()).collect();
        assert_eq!(order, vec![PathBuf::from("b.md"), PathBuf::from("a.md")]);
    }

    #[test]
    fn sorts_by_name() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "c.md", "x");
        write(root, "a.md", "x");
        write(root, "b.md", "x");

        let res = list_notes(root, &list_opts(ListSort::Name, 20)).unwrap();
        let order: Vec<_> = res.notes.iter().map(|n| n.path.clone()).collect();
        assert_eq!(
            order,
            vec![
                PathBuf::from("a.md"),
                PathBuf::from("b.md"),
                PathBuf::from("c.md")
            ]
        );
    }

    #[test]
    fn limit_caps_but_total_accurate() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "x");
        write(root, "b.md", "x");
        write(root, "c.md", "x");
        let res = list_notes(root, &list_opts(ListSort::Name, 2)).unwrap();
        assert_eq!(res.notes.len(), 2);
        assert_eq!(res.total, 3);
    }

    #[test]
    fn title_from_h1_else_filename_stem() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "with-h1.md", "# Real Title\nbody\n");
        write(root, "no-h1.md", "just body\n");
        let res = list_notes(root, &list_opts(ListSort::Name, 20)).unwrap();
        let titles: Vec<_> = res.notes.iter().map(|n| n.title.clone()).collect();
        // sorted by name: no-h1, with-h1
        assert_eq!(titles, vec!["no-h1".to_string(), "Real Title".to_string()]);
    }

    #[test]
    fn entry_reports_size_bytes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "12345"); // 5 bytes
        let res = list_notes(root, &list_opts(ListSort::Name, 20)).unwrap();
        assert_eq!(res.notes[0].size_bytes, 5);
    }
}
