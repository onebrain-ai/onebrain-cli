//! `note search` — substring / regex scan across vault notes.

use super::walker::walk_notes;
use crate::error::{FsError, Result};
use onebrain_core::CoreError;
use regex::RegexBuilder;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Match strategy for [`search_notes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Literal substring (default).
    Lex,
    /// Full Rust regex.
    Regex,
}

/// Inputs for [`search_notes`].
pub struct SearchOptions {
    pub pattern: String,
    pub folder: Option<PathBuf>,
    pub limit: usize,
    pub mode: SearchMode,
}

/// A single matching line. `path` is vault-relative; `line` is 1-based.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct NoteMatch {
    pub path: PathBuf,
    pub line: usize,
    pub context: String,
    pub title: Option<String>,
}

/// Result of [`search_notes`].
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SearchResult {
    pub matches: Vec<NoteMatch>,
    pub total_found: usize,
    pub truncated: bool,
    /// Vault-relative paths of notes that could not be read as UTF-8 text and
    /// were therefore excluded from the scan. Non-empty means results may be
    /// incomplete. Omitted from JSON when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<PathBuf>,
}

/// Scan every note under the vault (or `opts.folder`) for `opts.pattern`,
/// returning up to `opts.limit` matches. `total_found` counts all matches even
/// when truncated.
pub fn search_notes(vault_root: &Path, opts: &SearchOptions) -> Result<SearchResult> {
    let re = match opts.mode {
        SearchMode::Regex => Some(compile_regex(&opts.pattern)?),
        SearchMode::Lex => None,
    };
    let files = walk_notes(vault_root, opts.folder.as_deref())?;

    let mut matches = Vec::new();
    let mut total_found = 0usize;
    let mut skipped: Vec<PathBuf> = Vec::new();
    for file in &files {
        // Best-effort: skip notes that can't be read as UTF-8 text rather than
        // aborting a vault-wide search on one odd file — but RECORD them so the
        // CLI can warn that results may be incomplete.
        let Ok(content) = std::fs::read_to_string(file) else {
            skipped.push(file.strip_prefix(vault_root).unwrap_or(file).to_path_buf());
            continue;
        };
        let title = first_h1(&content);
        let rel = file.strip_prefix(vault_root).unwrap_or(file).to_path_buf();
        for (idx, line) in content.lines().enumerate() {
            let hit = match &re {
                Some(re) => re.is_match(line),
                None => line.contains(&opts.pattern),
            };
            if hit {
                total_found += 1;
                if matches.len() < opts.limit {
                    matches.push(NoteMatch {
                        path: rel.clone(),
                        line: idx + 1,
                        context: line.trim().to_string(),
                        title: title.clone(),
                    });
                }
            }
        }
    }
    let truncated = total_found > matches.len();
    Ok(SearchResult {
        matches,
        total_found,
        truncated,
        skipped,
    })
}

/// Cap the regex engine so an adversarial pattern can't blow up memory.
fn compile_regex(pattern: &str) -> Result<regex::Regex> {
    RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|e| FsError::Core(CoreError::InvalidTarget(format!("invalid regex: {e}"))))
}

/// First `# H1` heading text in `content`, if any.
fn first_h1(content: &str) -> Option<String> {
    content.lines().find_map(|l| {
        l.strip_prefix("# ")
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
    })
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

    fn opts(pattern: &str, mode: SearchMode, limit: usize) -> SearchOptions {
        SearchOptions {
            pattern: pattern.to_string(),
            folder: None,
            limit,
            mode,
        }
    }

    #[test]
    fn lex_search_returns_path_line_context_title() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "# Alpha\nsome TODO here\nplain\n");

        let res = search_notes(root, &opts("TODO", SearchMode::Lex, 20)).unwrap();
        assert_eq!(res.total_found, 1);
        assert!(!res.truncated);
        let m = &res.matches[0];
        assert_eq!(m.path, PathBuf::from("a.md"));
        assert_eq!(m.line, 2);
        assert_eq!(m.context, "some TODO here");
        assert_eq!(m.title.as_deref(), Some("Alpha"));
    }

    #[test]
    fn lex_search_no_match_is_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "nothing relevant\n");
        let res = search_notes(root, &opts("zzz", SearchMode::Lex, 20)).unwrap();
        assert_eq!(res.total_found, 0);
        assert!(res.matches.is_empty());
    }

    #[test]
    fn limit_truncates_but_total_found_is_accurate() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "x\nx\nx\n"); // 3 matching lines
        let res = search_notes(root, &opts("x", SearchMode::Lex, 2)).unwrap();
        assert_eq!(res.total_found, 3);
        assert_eq!(res.matches.len(), 2);
        assert!(res.truncated);
    }

    #[test]
    fn regex_mode_matches() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "v1.2.3 release\nplain line\n");
        let res = search_notes(root, &opts(r"v\d+\.\d+", SearchMode::Regex, 20)).unwrap();
        assert_eq!(res.total_found, 1);
        assert_eq!(res.matches[0].line, 1);
    }

    #[test]
    fn invalid_regex_errors() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "anything\n");
        let err = search_notes(root, &opts("(unclosed", SearchMode::Regex, 20)).unwrap_err();
        assert!(matches!(err, FsError::Core(CoreError::InvalidTarget(_))));
    }

    #[test]
    fn lex_mode_treats_pattern_literally() {
        // `(unclosed` is invalid regex but a fine literal substring.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "a (unclosed paren\n");
        let res = search_notes(root, &opts("(unclosed", SearchMode::Lex, 20)).unwrap();
        assert_eq!(res.total_found, 1);
    }

    /// An unreadable note is recorded in `skipped` (not silently dropped), and
    /// the scan still returns matches from the readable files.
    #[cfg(unix)]
    #[test]
    fn unreadable_note_is_skipped_and_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "ok.md", "has TODO here\n");
        write(root, "blocked.md", "TODO unreadable\n");
        // Strip all perms so read_to_string fails (EACCES).
        let blocked = root.join("blocked.md");
        let mut p = fs::metadata(&blocked).unwrap().permissions();
        p.set_mode(0o000);
        fs::set_permissions(&blocked, p).unwrap();

        let res = search_notes(root, &opts("TODO", SearchMode::Lex, 20)).unwrap();

        // Restore perms for cleanup.
        let mut p = fs::metadata(&blocked).unwrap().permissions();
        p.set_mode(0o644);
        fs::set_permissions(&blocked, p).unwrap();

        // Only the readable file contributes a match.
        assert_eq!(res.total_found, 1, "only readable note matched");
        assert_eq!(res.matches[0].path, PathBuf::from("ok.md"));
        // The unreadable file is recorded.
        assert_eq!(res.skipped, vec![PathBuf::from("blocked.md")]);
    }
}
