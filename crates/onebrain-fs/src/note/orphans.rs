//! `note orphans` — notes with ZERO incoming wikilinks.
//!
//! An orphan is a candidate note whose basename (file stem) appears in NO
//! wikilink anywhere in the vault. We first build the LINK SET by scanning
//! every note for `[[basename]]` / `[[basename|alias]]` / `[[basename#sec]]`
//! targets, then report candidates whose stem is absent from that set.
//!
//! Pre-organization and index/dashboard files are never reported as orphans:
//! anything under `00-inbox/` or `07-logs/`, the archive bucket (already
//! pruned by [`walk_notes`]), and top-level `TASKS.md` / `MOC.md`.

use super::walker::walk_notes;
use crate::error::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Result of [`orphans`]. `orphans` holds vault-relative paths (sorted),
/// capped to the requested limit; `total` counts every orphan before the cap.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct OrphansData {
    pub orphans: Vec<PathBuf>,
    pub total: usize,
    pub truncated: bool,
}

/// Find notes (under `folder`, or the whole vault) that have no incoming
/// wikilink from any note in the vault. Up to `limit` orphans are returned,
/// sorted by vault-relative path; `total` reflects the full count.
pub fn orphans(vault_root: &Path, folder: Option<&Path>, limit: usize) -> Result<OrphansData> {
    // 1. Build the LINK SET from every note in the vault.
    let all = walk_notes(vault_root, None)?;
    let mut linked: HashSet<String> = HashSet::new();
    for file in &all {
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        collect_link_targets(&content, &mut linked);
    }

    // 2. CANDIDATES = notes under `folder`, minus the exclusion set.
    let candidates = walk_notes(vault_root, folder)?;
    let mut orphans: Vec<PathBuf> = Vec::new();
    for file in &candidates {
        let rel = file.strip_prefix(vault_root).unwrap_or(file);
        if is_excluded(rel) {
            continue;
        }
        let stem = file
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        // 3. Orphan = stem absent from the link set.
        if !linked.contains(&stem) {
            orphans.push(rel.to_path_buf());
        }
    }

    // 4. Sort, count, cap.
    orphans.sort();
    let total = orphans.len();
    let truncated = total > limit;
    orphans.truncate(limit);
    Ok(OrphansData {
        orphans,
        total,
        truncated,
    })
}

/// True if a candidate's vault-relative path should never be reported as an
/// orphan: under `00-inbox/` or `07-logs/`, or exactly `TASKS.md` / `MOC.md`.
fn is_excluded(rel: &Path) -> bool {
    if let Some(first) = rel.components().next() {
        let name = first.as_os_str();
        if name == "00-inbox" || name == "07-logs" {
            return true;
        }
    }
    matches!(rel.to_str(), Some("TASKS.md") | Some("MOC.md"))
}

/// Add every wikilink target found in `content` to `set`. A wikilink is
/// `[[...]]`; its target is the inner text up to the first `|` (alias) or `#`
/// (section), trimmed. Empty targets are ignored.
fn collect_link_targets(content: &str, set: &mut HashSet<String>) {
    for line in content.lines() {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'[' && bytes[i + 1] == b'[' {
                let inner_start = i + 2;
                if let Some(rel_end) = line[inner_start..].find("]]") {
                    let inner = &line[inner_start..inner_start + rel_end];
                    let target = inner.split(['|', '#']).next().unwrap_or("").trim();
                    if !target.is_empty() {
                        set.insert(target.to_string());
                    }
                    i = inner_start + rel_end + 2;
                    continue;
                }
            }
            i += 1;
        }
    }
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
    fn note_with_no_backlinks_is_orphan() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "lonely.md", "# Lonely\nnobody links here\n");

        let res = orphans(root, None, 50).unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.orphans, vec![PathBuf::from("lonely.md")]);
        assert!(!res.truncated);
    }

    #[test]
    fn linked_note_is_not_orphan() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "target.md", "# Target\n");
        write(root, "source.md", "see [[target]] for details\n");

        let res = orphans(root, None, 50).unwrap();
        // `source.md` is the only orphan; `target.md` has a backlink.
        assert_eq!(res.orphans, vec![PathBuf::from("source.md")]);
    }

    #[test]
    fn aliased_link_counts_as_incoming() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "target.md", "# Target\n");
        write(root, "source.md", "jump to [[target|the target]]\n");

        let res = orphans(root, None, 50).unwrap();
        assert!(!res.orphans.contains(&PathBuf::from("target.md")));
    }

    #[test]
    fn section_link_counts_as_incoming() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "target.md", "# Target\n");
        write(root, "source.md", "deep [[target#Heading]]\n");

        let res = orphans(root, None, 50).unwrap();
        assert!(!res.orphans.contains(&PathBuf::from("target.md")));
    }

    #[test]
    fn inbox_logs_tasks_moc_excluded_even_if_unlinked() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "00-inbox/2026-05-26-thought.md", "raw\n");
        write(root, "07-logs/session/2026/05/s.md", "log\n");
        write(root, "TASKS.md", "# Tasks\n");
        write(root, "MOC.md", "# Map\n");
        write(root, "real-orphan.md", "# Real\n");

        let res = orphans(root, None, 50).unwrap();
        assert_eq!(res.orphans, vec![PathBuf::from("real-orphan.md")]);
        assert_eq!(res.total, 1);
    }

    #[test]
    fn limit_truncates_and_total_is_full_count() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "x");
        write(root, "b.md", "x");
        write(root, "c.md", "x");

        let res = orphans(root, None, 2).unwrap();
        assert_eq!(res.total, 3);
        assert_eq!(res.orphans.len(), 2);
        assert!(res.truncated);
        // Sorted by path → first two.
        assert_eq!(
            res.orphans,
            vec![PathBuf::from("a.md"), PathBuf::from("b.md")]
        );
    }

    #[test]
    fn folder_scopes_candidates_but_link_set_is_global() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Candidate in scope, no backlinks → orphan.
        write(root, "03-knowledge/solo.md", "# Solo\n");
        // Candidate in scope, linked from OUTSIDE the folder → not orphan.
        write(root, "03-knowledge/cited.md", "# Cited\n");
        write(root, "01-projects/p.md", "ref [[cited]]\n");
        // Out-of-scope orphan must not appear.
        write(root, "02-areas/elsewhere.md", "# Elsewhere\n");

        let res = orphans(root, Some(Path::new("03-knowledge")), 50).unwrap();
        assert_eq!(res.orphans, vec![PathBuf::from("03-knowledge/solo.md")]);
    }

    #[test]
    fn archive_notes_are_not_candidates() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "06-archive/2026/old.md", "# Old\n");
        write(root, "live.md", "# Live\n");

        let res = orphans(root, None, 50).unwrap();
        // walk_notes prunes 06-archive, so only `live.md` is a candidate.
        assert_eq!(res.orphans, vec![PathBuf::from("live.md")]);
    }
}
