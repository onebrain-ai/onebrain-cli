//! `note backlinks` — find every note that links TO a target note.
//!
//! The target's identity is its basename without extension (e.g.
//! `MEMORY-INDEX.md` → `MEMORY-INDEX`). A backlink is any line in any OTHER
//! note containing a wikilink whose link target equals that basename:
//! `[[MEMORY-INDEX]]`, `[[MEMORY-INDEX|alias]]`, `[[MEMORY-INDEX#section]]`.

use super::walker::{walk_notes, walk_notes_with_archive};
use crate::error::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// A single matching line. `source` is vault-relative; `line` is 1-based.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct BacklinkEntry {
    pub source: PathBuf,
    pub line: usize,
    pub context: String,
}

/// Result of [`backlinks`].
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct BacklinksData {
    pub target: PathBuf,
    pub backlinks: Vec<BacklinkEntry>,
    pub count: usize,
}

/// Find every note that links to `rel_target` (vault-relative path). When
/// `include_archive` is true, notes under `06-archive` are scanned too.
pub fn backlinks(
    vault_root: &Path,
    rel_target: &Path,
    include_archive: bool,
) -> Result<BacklinksData> {
    let target_basename = basename_no_ext(rel_target);

    let files = if include_archive {
        walk_notes_with_archive(vault_root)?
    } else {
        walk_notes(vault_root, None)?
    };

    let target_abs = vault_root.join(rel_target);
    let mut entries = Vec::new();
    for file in &files {
        // Don't count the target file linking to itself.
        if *file == target_abs {
            continue;
        }
        // Best-effort: skip notes that can't be read as UTF-8 text rather than
        // aborting on one odd file.
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        let rel = file.strip_prefix(vault_root).unwrap_or(file).to_path_buf();
        for (idx, line) in content.lines().enumerate() {
            if line_links_to(line, &target_basename) {
                entries.push(BacklinkEntry {
                    source: rel.clone(),
                    line: idx + 1,
                    context: line.trim().to_string(),
                });
            }
        }
    }

    let count = entries.len();
    Ok(BacklinksData {
        target: rel_target.to_path_buf(),
        backlinks: entries,
        count,
    })
}

/// File stem (basename without extension) as an owned `String`.
fn basename_no_ext(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// True if `line` contains at least one wikilink whose target equals
/// `basename`. A wikilink is `[[...]]`; the link target is the inner text up
/// to the first `|` (alias) or `#` (section), trimmed.
fn line_links_to(line: &str, basename: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let inner_start = i + 2;
            if let Some(rel_end) = line[inner_start..].find("]]") {
                let inner = &line[inner_start..inner_start + rel_end];
                let link_target = inner.split(['|', '#']).next().unwrap_or("").trim();
                if link_target == basename {
                    return true;
                }
                // Advance past this wikilink's closing `]]`.
                i = inner_start + rel_end + 2;
                continue;
            }
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn plain_wikilink_is_a_backlink() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "MEMORY-INDEX.md", "# Index\n");
        write(root, "a.md", "see [[MEMORY-INDEX]] for more\n");

        let res = backlinks(root, Path::new("MEMORY-INDEX.md"), false).unwrap();
        assert_eq!(res.count, 1);
        assert_eq!(res.target, PathBuf::from("MEMORY-INDEX.md"));
        let e = &res.backlinks[0];
        assert_eq!(e.source, PathBuf::from("a.md"));
        assert_eq!(e.line, 1);
        assert_eq!(e.context, "see [[MEMORY-INDEX]] for more");
    }

    #[test]
    fn aliased_wikilink_is_a_backlink() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "MEMORY-INDEX.md", "# Index\n");
        write(root, "a.md", "jump to [[MEMORY-INDEX|the index]]\n");

        let res = backlinks(root, Path::new("MEMORY-INDEX.md"), false).unwrap();
        assert_eq!(res.count, 1);
        assert_eq!(res.backlinks[0].source, PathBuf::from("a.md"));
    }

    #[test]
    fn section_wikilink_is_a_backlink() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "MEMORY-INDEX.md", "# Index\n");
        write(root, "a.md", "deep link [[MEMORY-INDEX#Topics]]\n");

        let res = backlinks(root, Path::new("MEMORY-INDEX.md"), false).unwrap();
        assert_eq!(res.count, 1);
    }

    #[test]
    fn non_matching_link_is_ignored() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "MEMORY-INDEX.md", "# Index\n");
        write(root, "a.md", "unrelated [[other-note]] here\n");

        let res = backlinks(root, Path::new("MEMORY-INDEX.md"), false).unwrap();
        assert_eq!(res.count, 0);
        assert!(res.backlinks.is_empty());
    }

    #[test]
    fn target_linking_to_itself_is_not_counted() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // The target references its own basename — must NOT count.
        write(
            root,
            "MEMORY-INDEX.md",
            "# Index\nself ref [[MEMORY-INDEX]]\n",
        );
        write(root, "a.md", "external [[MEMORY-INDEX]]\n");

        let res = backlinks(root, Path::new("MEMORY-INDEX.md"), false).unwrap();
        assert_eq!(res.count, 1);
        assert_eq!(res.backlinks[0].source, PathBuf::from("a.md"));
    }

    #[test]
    fn archive_backlink_hidden_by_default() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "MEMORY-INDEX.md", "# Index\n");
        write(
            root,
            "06-archive/2026/old.md",
            "archived [[MEMORY-INDEX]]\n",
        );

        let res = backlinks(root, Path::new("MEMORY-INDEX.md"), false).unwrap();
        assert_eq!(res.count, 0);
    }

    #[test]
    fn archive_backlink_shown_with_flag() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "MEMORY-INDEX.md", "# Index\n");
        write(
            root,
            "06-archive/2026/old.md",
            "archived [[MEMORY-INDEX]]\n",
        );

        let res = backlinks(root, Path::new("MEMORY-INDEX.md"), true).unwrap();
        assert_eq!(res.count, 1);
        assert_eq!(
            res.backlinks[0].source,
            PathBuf::from("06-archive/2026/old.md")
        );
    }

    #[test]
    fn multiple_links_on_one_line_count_once() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "MEMORY-INDEX.md", "# Index\n");
        write(
            root,
            "a.md",
            "both [[other]] and [[MEMORY-INDEX]] on this line\n",
        );

        let res = backlinks(root, Path::new("MEMORY-INDEX.md"), false).unwrap();
        assert_eq!(res.count, 1);
        assert_eq!(res.backlinks[0].line, 1);
    }
}
