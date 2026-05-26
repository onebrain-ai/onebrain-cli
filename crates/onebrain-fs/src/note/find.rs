//! `note find` — locate files/folders by glob, optionally filtered by mtime.
//!
//! Unlike `search`/`list`, this is a raw finder: it walks the whole vault
//! (including `06-archive` and `07-logs`, skipping only tooling dirs) and is
//! not restricted to `.md`. A glob with no `/` matches the basename (like
//! `find -name`); a glob containing `/` matches the vault-relative path.

use super::walker::walk_all;
use crate::error::{FsError, Result};
use globset::Glob;
use onebrain_core::CoreError;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SECS_PER_DAY: i64 = 86_400;

/// Restrict matches to files or folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindType {
    File,
    Folder,
}

/// Inputs for [`find_notes`].
pub struct FindOptions {
    pub glob: String,
    pub r#type: Option<FindType>,
    /// Day offset (Unix-`find`-like): `-N` = modified within the last N days,
    /// `0` = today, `+N` = older than N days. `None` = no mtime filter.
    pub mtime: Option<i64>,
    pub limit: usize,
}

/// A single match. `path` is vault-relative.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FindEntry {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Result of [`find_notes`].
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FindResult {
    pub entries: Vec<FindEntry>,
    pub total: usize,
    pub truncated: bool,
}

/// Find files/folders under the vault matching `opts.glob`, applying the
/// optional type + mtime filters, capped to `opts.limit`.
pub fn find_notes(vault_root: &Path, opts: &FindOptions) -> Result<FindResult> {
    let matcher = Glob::new(&opts.glob)
        .map_err(|e| FsError::Core(CoreError::InvalidTarget(format!("invalid glob: {e}"))))?
        .compile_matcher();
    // No `/` in the pattern → match the basename (Unix `find -name` ergonomics);
    // otherwise match the vault-relative path.
    let by_path = opts.glob.contains('/');
    let now = SystemTime::now();

    let mut entries = Vec::new();
    let mut total = 0usize;
    for (abs, is_dir) in walk_all(vault_root)? {
        if let Some(t) = opts.r#type {
            let want_dir = matches!(t, FindType::Folder);
            if is_dir != want_dir {
                continue;
            }
        }
        let rel = abs.strip_prefix(vault_root).unwrap_or(&abs);
        let hit = if by_path {
            matcher.is_match(rel)
        } else {
            abs.file_name()
                .map(|n| matcher.is_match(Path::new(n)))
                .unwrap_or(false)
        };
        if !hit {
            continue;
        }
        if let Some(n) = opts.mtime {
            if !mtime_matches(&abs, n, now) {
                continue;
            }
        }
        total += 1;
        if entries.len() < opts.limit {
            entries.push(FindEntry {
                path: rel.to_path_buf(),
                is_dir,
            });
        }
    }
    let truncated = total > entries.len();
    Ok(FindResult {
        entries,
        total,
        truncated,
    })
}

/// Unix-`find`-style mtime predicate. `now` injected for testability.
fn mtime_matches(path: &Path, n: i64, now: SystemTime) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let age = now
        .duration_since(modified)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    match n.cmp(&0) {
        std::cmp::Ordering::Less => age <= (-n) * SECS_PER_DAY, // within last |n| days
        std::cmp::Ordering::Equal => age < SECS_PER_DAY,        // today
        std::cmp::Ordering::Greater => age > n * SECS_PER_DAY,  // older than n days
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        p
    }

    fn find_opts(glob: &str, r#type: Option<FindType>, mtime: Option<i64>) -> FindOptions {
        FindOptions {
            glob: glob.to_string(),
            r#type,
            mtime,
            limit: 50,
        }
    }

    fn rel_paths(res: &FindResult) -> Vec<String> {
        res.entries
            .iter()
            .map(|e| e.path.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn basename_glob_matches_nested_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "07-logs/2026/05/x-checkpoint-01.md", "c");
        write(root, "note.md", "n");
        write(root, "03-knowledge/a.md", "a");

        let res = find_notes(root, &find_opts("*-checkpoint-*.md", None, None)).unwrap();
        assert_eq!(rel_paths(&res), vec!["07-logs/2026/05/x-checkpoint-01.md"]);
        assert!(!res.entries[0].is_dir);
    }

    #[test]
    fn type_folder_returns_only_dirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "01-projects/p.md", "p");
        write(root, "03-knowledge/a.md", "a");

        let res = find_notes(root, &find_opts("0*", Some(FindType::Folder), None)).unwrap();
        let paths = rel_paths(&res);
        assert!(paths.contains(&"01-projects".to_string()));
        assert!(paths.contains(&"03-knowledge".to_string()));
        assert!(res.entries.iter().all(|e| e.is_dir));
    }

    #[test]
    fn path_glob_matches_relative_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "03-knowledge/ml/deep.md", "d");
        write(root, "01-projects/p.md", "p");

        let res = find_notes(
            root,
            &find_opts("03-knowledge/**", Some(FindType::File), None),
        )
        .unwrap();
        assert_eq!(rel_paths(&res), vec!["03-knowledge/ml/deep.md"]);
    }

    #[test]
    fn mtime_filter_keeps_only_recent() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let recent = write(root, "recent.md", "r");
        let old = write(root, "old.md", "o");
        set_file_mtime(&recent, FileTime::from_system_time(SystemTime::now())).unwrap();
        set_file_mtime(
            &old,
            FileTime::from_system_time(SystemTime::now() - Duration::from_secs(10 * 86_400)),
        )
        .unwrap();

        let res = find_notes(root, &find_opts("*.md", Some(FindType::File), Some(-1))).unwrap();
        assert_eq!(rel_paths(&res), vec!["recent.md"]);
    }

    #[test]
    fn invalid_glob_errors() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "a");
        let err = find_notes(root, &find_opts("[unclosed", None, None)).unwrap_err();
        assert!(matches!(err, FsError::Core(CoreError::InvalidTarget(_))));
    }
}
