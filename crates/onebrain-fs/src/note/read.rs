//! `note read` — surgical read of a single vault note.
//!
//! The model gets only what it asks for: the whole body (capped to `--limit`),
//! a single heading section, the parsed frontmatter, or just the task lines.

use crate::error::{FsError, Result};
use crate::frontmatter::parse_frontmatter;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Inputs for [`read_note`]. The three view selectors (`section`,
/// `frontmatter_only`, `tasks_only`) are mutually exclusive — the CLI enforces
/// this via clap; here the first set one wins in the order section → frontmatter
/// → tasks → body.
pub struct ReadOptions {
    /// Extract the content under this heading (matched by trimmed text).
    pub section: Option<String>,
    /// Emit only the parsed YAML frontmatter.
    pub frontmatter_only: bool,
    /// Emit only task lines (`- [ ]` / `- [x]`).
    pub tasks_only: bool,
    /// Max lines when returning the body (0 = unlimited).
    pub limit: usize,
}

/// Result of [`read_note`]. `path` is vault-relative; exactly one of
/// `content` / `frontmatter` / `tasks` is populated depending on `mode`.
#[derive(Debug, Serialize, PartialEq)]
pub struct ReadResult {
    pub path: PathBuf,
    /// One of `"body"`, `"section"`, `"frontmatter"`, `"tasks"`.
    pub mode: &'static str,
    /// Body / section text (modes `body` and `section`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Parsed frontmatter mapping (mode `frontmatter`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontmatter: Option<serde_yaml::Value>,
    /// Task lines (mode `tasks`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<String>>,
}

/// Read `rel_path` (relative to `vault_root`) and project it down to whatever
/// `opts` requests.
///
/// View precedence: `section` → `frontmatter_only` → `tasks_only` → body.
///
/// - Body: the whole file, capped to `opts.limit` lines (`0` = unlimited).
/// - Section: the heading line whose trimmed text equals `opts.section`, plus
///   every following line up to (but not including) the next heading of the
///   same or higher level. Includes the heading line itself. If the section is
///   not found, `content` is `None`.
/// - Frontmatter: the parsed YAML mapping, or `None` if absent/unparseable.
/// - Tasks: lines matching `^\s*- \[[ xX]\] `, trimmed of leading whitespace.
///
/// A missing file surfaces as [`FsError::Io`] (ENOENT).
pub fn read_note(vault_root: &Path, rel_path: &Path, opts: &ReadOptions) -> Result<ReadResult> {
    let abs = vault_root.join(rel_path);
    let raw = std::fs::read_to_string(&abs).map_err(|source| FsError::Io {
        path: abs.clone(),
        source,
    })?;
    let path = rel_path.to_path_buf();

    if let Some(heading) = &opts.section {
        return Ok(ReadResult {
            path,
            mode: "section",
            content: extract_section(&raw, heading),
            frontmatter: None,
            tasks: None,
        });
    }
    if opts.frontmatter_only {
        return Ok(ReadResult {
            path,
            mode: "frontmatter",
            content: None,
            frontmatter: parse_frontmatter(&raw),
            tasks: None,
        });
    }
    if opts.tasks_only {
        return Ok(ReadResult {
            path,
            mode: "tasks",
            content: None,
            frontmatter: None,
            tasks: Some(extract_tasks(&raw)),
        });
    }

    Ok(ReadResult {
        path,
        mode: "body",
        content: Some(cap_lines(&raw, opts.limit)),
        frontmatter: None,
        tasks: None,
    })
}

/// Return `raw` capped to the first `limit` lines (`0` = unlimited).
fn cap_lines(raw: &str, limit: usize) -> String {
    if limit == 0 {
        return raw.to_string();
    }
    raw.lines().take(limit).collect::<Vec<_>>().join("\n")
}

/// ATX heading level (number of leading `#`s) + trimmed title text, if `line`
/// is a heading (`#`s followed by a space).
///
/// Shared with `note::append` (`--section` placement reuses the same
/// heading-detection rules so read/append agree on section boundaries).
pub(super) fn heading_parts(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if hashes == 0 {
        return None;
    }
    let rest = &line[hashes..];
    // Require the conventional space after the `#`s (`# Title`, not `#nohash`).
    let title = rest.strip_prefix(' ')?;
    Some((hashes, title.trim()))
}

/// Extract the block under the heading whose trimmed text equals `target`.
/// Includes the heading line; stops at the next heading of same-or-higher level.
fn extract_section(raw: &str, target: &str) -> Option<String> {
    let lines: Vec<&str> = raw.lines().collect();
    let target = target.trim();

    let mut start = None;
    let mut level = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if let Some((lvl, title)) = heading_parts(line) {
            if title == target {
                start = Some(i);
                level = lvl;
                break;
            }
        }
    }
    let start = start?;

    let mut end = lines.len();
    for (offset, line) in lines[start + 1..].iter().enumerate() {
        if let Some((lvl, _)) = heading_parts(line) {
            if lvl <= level {
                end = start + 1 + offset;
                break;
            }
        }
    }
    Some(lines[start..end].join("\n"))
}

/// Collect task lines (`- [ ]` / `- [x]` / `- [X]`, allowing leading
/// whitespace), trimmed of that leading whitespace.
fn extract_tasks(raw: &str) -> Vec<String> {
    raw.lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let rest = trimmed.strip_prefix("- [")?;
            let mark = rest.chars().next()?;
            if matches!(mark, ' ' | 'x' | 'X') && rest[mark.len_utf8()..].starts_with("] ") {
                Some(trimmed.to_string())
            } else {
                None
            }
        })
        .collect()
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

    fn opts(
        section: Option<&str>,
        frontmatter_only: bool,
        tasks_only: bool,
        limit: usize,
    ) -> ReadOptions {
        ReadOptions {
            section: section.map(str::to_string),
            frontmatter_only,
            tasks_only,
            limit,
        }
    }

    #[test]
    fn body_returns_whole_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "line1\nline2\nline3\n");

        let res = read_note(root, Path::new("a.md"), &opts(None, false, false, 0)).unwrap();
        assert_eq!(res.mode, "body");
        // limit=0 → verbatim file content (trailing newline preserved).
        assert_eq!(res.content.as_deref(), Some("line1\nline2\nline3\n"));
        assert_eq!(res.path, PathBuf::from("a.md"));
    }

    #[test]
    fn body_caps_to_limit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "l1\nl2\nl3\nl4\nl5\n");

        let res = read_note(root, Path::new("a.md"), &opts(None, false, false, 2)).unwrap();
        assert_eq!(res.content.as_deref(), Some("l1\nl2"));
    }

    #[test]
    fn section_extracts_block_until_same_level_heading() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "a.md",
            "# Title\nintro\n\n## Alpha\na1\na2\n\n## Beta\nb1\n",
        );

        let res = read_note(
            root,
            Path::new("a.md"),
            &opts(Some("Alpha"), false, false, 0),
        )
        .unwrap();
        assert_eq!(res.mode, "section");
        assert_eq!(res.content.as_deref(), Some("## Alpha\na1\na2\n"));
    }

    #[test]
    fn section_stops_at_higher_level_heading() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // A deeper subsection is included; a shallower (higher-level) heading stops it.
        write(root, "a.md", "## Alpha\na1\n### Sub\ns1\n# Top\nt1\n");

        let res = read_note(
            root,
            Path::new("a.md"),
            &opts(Some("Alpha"), false, false, 0),
        )
        .unwrap();
        assert_eq!(res.content.as_deref(), Some("## Alpha\na1\n### Sub\ns1"));
    }

    #[test]
    fn section_not_found_returns_none() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "# Title\nbody\n");

        let res = read_note(
            root,
            Path::new("a.md"),
            &opts(Some("Nope"), false, false, 0),
        )
        .unwrap();
        assert_eq!(res.mode, "section");
        assert!(res.content.is_none());
    }

    #[test]
    fn frontmatter_only_parses_yaml() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "a.md",
            "---\ntags: [x, y]\ncreated: 2026-05-26\n---\nbody\n",
        );

        let res = read_note(root, Path::new("a.md"), &opts(None, true, false, 0)).unwrap();
        assert_eq!(res.mode, "frontmatter");
        assert!(res.content.is_none());
        let fm = res.frontmatter.unwrap();
        assert!(fm.get("tags").is_some());
        assert_eq!(
            fm.get("created").and_then(|v| v.as_str()),
            Some("2026-05-26")
        );
    }

    #[test]
    fn frontmatter_only_absent_is_none() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "no frontmatter here\n");

        let res = read_note(root, Path::new("a.md"), &opts(None, true, false, 0)).unwrap();
        assert_eq!(res.mode, "frontmatter");
        assert!(res.frontmatter.is_none());
    }

    #[test]
    fn tasks_only_collects_task_lines() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "a.md",
            "# Title\n- [ ] open task 📅 2026-05-26\nplain line\n  - [x] done nested\n- [X] caps done\n- not a task\n-[ ] no space\n",
        );

        let res = read_note(root, Path::new("a.md"), &opts(None, false, true, 0)).unwrap();
        assert_eq!(res.mode, "tasks");
        assert!(res.content.is_none());
        let tasks = res.tasks.unwrap();
        assert_eq!(
            tasks,
            vec![
                "- [ ] open task 📅 2026-05-26".to_string(),
                "- [x] done nested".to_string(),
                "- [X] caps done".to_string(),
            ]
        );
    }

    #[test]
    fn missing_file_is_io_error() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let err = read_note(root, Path::new("nope.md"), &opts(None, false, false, 0)).unwrap_err();
        assert!(matches!(err, FsError::Io { .. }));
    }
}
