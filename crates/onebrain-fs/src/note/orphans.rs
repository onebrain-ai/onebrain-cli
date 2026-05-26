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

use super::path_out::rel_slash;
use super::walker::walk_notes;
use crate::error::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;

/// Result of [`orphans`]. `orphans` holds vault-relative forward-slash paths
/// (sorted), capped to the requested limit; `total` counts every orphan before
/// the cap.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct OrphansData {
    pub orphans: Vec<String>,
    pub total: usize,
    pub truncated: bool,
    /// Vault-relative forward-slash paths of notes that could not be read as
    /// UTF-8 text while building the link set. Non-empty means a note here
    /// might hold the ONLY inbound link to a candidate, so a result could be a
    /// FALSE orphan. Omitted from JSON when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

/// Find notes (under `folder`, or the whole vault) that have no incoming
/// wikilink from any note in the vault. Up to `limit` orphans are returned,
/// sorted by vault-relative path; `total` reflects the full count.
pub fn orphans(vault_root: &Path, folder: Option<&Path>, limit: usize) -> Result<OrphansData> {
    // 1. Build the LINK SET from every note in the vault.
    let all = walk_notes(vault_root, None)?;
    let mut linked: HashSet<String> = HashSet::new();
    let mut skipped: Vec<String> = Vec::new();
    for file in &all {
        // Record unreadable files: one might hold the ONLY inbound link to a
        // candidate, which would otherwise be wrongly reported as an orphan.
        let Ok(content) = std::fs::read_to_string(file) else {
            skipped.push(rel_slash(vault_root, file));
            continue;
        };
        collect_link_targets(&content, &mut linked);
    }

    // 2. CANDIDATES = notes under `folder`, minus the exclusion set.
    let candidates = walk_notes(vault_root, folder)?;
    let mut orphans: Vec<String> = Vec::new();
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
            orphans.push(rel_slash(vault_root, file));
        }
    }

    // 4. Sort, count, cap.
    orphans.sort();
    let total = orphans.len();
    let truncated = total > limit;
    orphans.truncate(limit);
    skipped.sort();
    Ok(OrphansData {
        orphans,
        total,
        truncated,
        skipped,
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
        assert_eq!(res.orphans, vec!["lonely.md".to_string()]);
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
        assert_eq!(res.orphans, vec!["source.md".to_string()]);
    }

    #[test]
    fn aliased_link_counts_as_incoming() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "target.md", "# Target\n");
        write(root, "source.md", "jump to [[target|the target]]\n");

        let res = orphans(root, None, 50).unwrap();
        assert!(!res.orphans.contains(&"target.md".to_string()));
    }

    #[test]
    fn section_link_counts_as_incoming() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "target.md", "# Target\n");
        write(root, "source.md", "deep [[target#Heading]]\n");

        let res = orphans(root, None, 50).unwrap();
        assert!(!res.orphans.contains(&"target.md".to_string()));
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
        assert_eq!(res.orphans, vec!["real-orphan.md".to_string()]);
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
        assert_eq!(res.orphans, vec!["a.md".to_string(), "b.md".to_string()]);
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
        assert_eq!(res.orphans, vec!["03-knowledge/solo.md".to_string()]);
    }

    #[test]
    fn archive_notes_are_not_candidates() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "06-archive/2026/old.md", "# Old\n");
        write(root, "live.md", "# Live\n");

        let res = orphans(root, None, 50).unwrap();
        // walk_notes prunes 06-archive, so only `live.md` is a candidate.
        assert_eq!(res.orphans, vec!["live.md".to_string()]);
    }

    /// An unreadable note (which might hold the only inbound link to a
    /// candidate) is recorded in `skipped` rather than silently dropped. We
    /// only assert the reporting — not which orphans result — because whether a
    /// candidate is a false orphan depends on the unreadable file's contents.
    #[cfg(unix)]
    #[test]
    fn unreadable_note_is_skipped_and_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "target.md", "# Target\n");
        // `linker.md` is the ONLY note linking `target` — but it's unreadable,
        // so without the skipped record `target` would look like a false orphan.
        write(root, "linker.md", "see [[target]]\n");
        let blocked = root.join("linker.md");
        let mut p = fs::metadata(&blocked).unwrap().permissions();
        p.set_mode(0o000);
        fs::set_permissions(&blocked, p).unwrap();

        let res = orphans(root, None, 50).unwrap();

        let mut p = fs::metadata(&blocked).unwrap().permissions();
        p.set_mode(0o644);
        fs::set_permissions(&blocked, p).unwrap();

        // The unreadable file is named so the caller knows the orphan list may
        // contain false positives (here `target.md`, since its only linker was
        // unreadable).
        assert_eq!(res.skipped, vec!["linker.md".to_string()]);
        assert!(
            res.orphans.contains(&"target.md".to_string()),
            "with the linker unreadable, target appears as a (false) orphan: {:?}",
            res.orphans
        );
    }
}
