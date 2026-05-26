//! `note stat` — content + filesystem counts for a single vault note.
//!
//! Pure text/metadata logic: lines / words / chars / size, plus Markdown-aware
//! counts (headings, wikilinks, tasks). Mirrors `list.rs` for the fs-metadata
//! handling (size / created / modified).

use super::path_out::to_slash;
use crate::error::{FsError, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::Path;

/// Result of [`stat_note`]. `path` is a vault-relative forward-slash string.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct NoteStatData {
    pub path: String,
    pub lines: usize,
    pub words: usize,
    pub chars: usize,
    pub size_bytes: u64,
    /// Count of ATX heading lines (`#`–`######` followed by a space).
    pub headings: usize,
    /// Count of `[[…]]` wikilink occurrences (multiple per line counted).
    pub wikilinks: usize,
    pub tasks_total: usize,
    pub tasks_done: usize,
    pub tasks_open: usize,
    pub created: Option<DateTime<Utc>>,
    pub modified: DateTime<Utc>,
}

/// Compute content + filesystem statistics for `rel_path` (relative to
/// `vault_root`). A missing file surfaces as [`FsError::Io`] (ENOENT).
pub fn stat_note(vault_root: &Path, rel_path: &Path) -> Result<NoteStatData> {
    let abs = vault_root.join(rel_path);
    let raw = std::fs::read_to_string(&abs).map_err(|source| FsError::Io {
        path: abs.clone(),
        source,
    })?;
    let meta = std::fs::metadata(&abs).map_err(|source| FsError::Io {
        path: abs.clone(),
        source,
    })?;
    let modified: DateTime<Utc> = meta
        .modified()
        .map(DateTime::<Utc>::from)
        .map_err(|source| FsError::Io {
            path: abs.clone(),
            source,
        })?;
    let created = meta.created().ok().map(DateTime::<Utc>::from);

    let counts = count_content(&raw);

    Ok(NoteStatData {
        path: to_slash(rel_path),
        lines: raw.lines().count(),
        words: raw.split_whitespace().count(),
        chars: raw.chars().count(),
        size_bytes: meta.len(),
        headings: counts.headings,
        wikilinks: counts.wikilinks,
        tasks_total: counts.tasks_done + counts.tasks_open,
        tasks_done: counts.tasks_done,
        tasks_open: counts.tasks_open,
        created,
        modified,
    })
}

#[derive(Default)]
struct ContentCounts {
    headings: usize,
    wikilinks: usize,
    tasks_done: usize,
    tasks_open: usize,
}

/// Markdown-aware counts over the raw text: ATX headings, wikilink
/// occurrences, and done/open tasks.
fn count_content(raw: &str) -> ContentCounts {
    let mut counts = ContentCounts::default();
    for line in raw.lines() {
        if is_heading(line) {
            counts.headings += 1;
        }
        match task_mark(line) {
            Some(true) => counts.tasks_done += 1,
            Some(false) => counts.tasks_open += 1,
            None => {}
        }
    }
    counts.wikilinks = count_wikilinks(raw);
    counts
}

/// True if `line` is an ATX heading: 1–6 leading `#`s followed by a space.
fn is_heading(line: &str) -> bool {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    (1..=6).contains(&hashes) && line[hashes..].starts_with(' ')
}

/// Classify a task line: `Some(true)` = done (`- [x] ` / `- [X] `),
/// `Some(false)` = open (`- [ ] `), `None` = not a task. Leading whitespace
/// is allowed.
fn task_mark(line: &str) -> Option<bool> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("- [")?;
    let mark = rest.chars().next()?;
    if !rest[mark.len_utf8()..].starts_with("] ") {
        return None;
    }
    match mark {
        ' ' => Some(false),
        'x' | 'X' => Some(true),
        _ => None,
    }
}

/// Count `[[…]]` occurrences: scan for `[[`, then require a following `]]`.
/// Each matched link advances past its closing `]]`, so several links on one
/// line all count.
fn count_wikilinks(raw: &str) -> usize {
    let bytes = raw.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            // Look for the closing `]]` after the opening `[[`.
            if let Some(close) = find_close(bytes, i + 2) {
                count += 1;
                i = close + 2;
                continue;
            }
            // No closing delimiter — stop scanning.
            break;
        }
        i += 1;
    }
    count
}

/// Index of the `]` that begins a `]]` at or after `from`, else `None`.
fn find_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j + 1 < bytes.len() {
        if bytes[j] == b']' && bytes[j + 1] == b']' {
            return Some(j);
        }
        j += 1;
    }
    None
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

    const FIXTURE: &str = "# Title\n\
intro line with two words here\n\
\n\
## Section A\n\
- [ ] open task\n\
- [x] done task\n\
see [[Note One]] and [[Note Two]] in one line\n\
### Deep heading\n\
- [X] caps done\n\
plain [[Single Link]] line\n";

    #[test]
    fn counts_lines_words_chars() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", FIXTURE);
        let s = stat_note(root, Path::new("a.md")).unwrap();
        assert_eq!(s.lines, 10, "line count");
        assert_eq!(s.words, FIXTURE.split_whitespace().count(), "word count");
        assert_eq!(s.chars, FIXTURE.chars().count(), "char count");
        assert_eq!(s.path, "a.md");
    }

    #[test]
    fn counts_headings() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", FIXTURE);
        let s = stat_note(root, Path::new("a.md")).unwrap();
        // `# Title`, `## Section A`, `### Deep heading`
        assert_eq!(s.headings, 3);
    }

    #[test]
    fn heading_requires_space_after_hashes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "a.md",
            "# Real\n#nothashtag\n####### too many\n## Also\n",
        );
        let s = stat_note(root, Path::new("a.md")).unwrap();
        // `# Real` and `## Also` only (#nothashtag has no space; 7 hashes is out of range).
        assert_eq!(s.headings, 2);
    }

    #[test]
    fn counts_tasks_done_open_total() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", FIXTURE);
        let s = stat_note(root, Path::new("a.md")).unwrap();
        // open: `- [ ] open task`
        // done: `- [x] done task`, `- [X] caps done`
        assert_eq!(s.tasks_open, 1);
        assert_eq!(s.tasks_done, 2);
        assert_eq!(s.tasks_total, 3);
    }

    #[test]
    fn counts_wikilinks_including_multiple_per_line() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", FIXTURE);
        let s = stat_note(root, Path::new("a.md")).unwrap();
        // `[[Note One]]`, `[[Note Two]]` (same line) + `[[Single Link]]` = 3
        assert_eq!(s.wikilinks, 3);
    }

    #[test]
    fn unterminated_wikilink_is_not_counted() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "valid [[Real]] then broken [[oops\n");
        let s = stat_note(root, Path::new("a.md")).unwrap();
        assert_eq!(s.wikilinks, 1);
    }

    #[test]
    fn reports_size_bytes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "12345"); // 5 bytes
        let s = stat_note(root, Path::new("a.md")).unwrap();
        assert_eq!(s.size_bytes, 5);
        assert_eq!(s.chars, 5);
        // No trailing newline → one (unterminated) line.
        assert_eq!(s.lines, 1);
    }

    #[test]
    fn missing_file_is_io_error() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let err = stat_note(root, Path::new("nope.md")).unwrap_err();
        assert!(matches!(err, FsError::Io { .. }));
    }
}
