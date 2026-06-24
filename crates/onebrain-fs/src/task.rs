//! Vault-wide task scan — shared core for the daemon's `GET /api/vault/tasks`
//! endpoint and (the roadmap) `onebrain task list` CLI verb, so both surfaces use
//! ONE implementation rather than each carrying its own regex + walk.
//!
//! Returns every DATED Obsidian-Tasks checkbox line — `- [ ] text 📅 YYYY-MM-DD`.
//! Undated `- [ ]` lines (the long implementation checklists in plan/spec docs)
//! are intentionally excluded: only scheduled items are calendar todos.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

use crate::note::{is_tooling_dir, to_slash};

/// One dated task found in a vault note.
#[derive(Debug, Clone, Serialize)]
pub struct TaskHit {
    /// Vault-relative, slash-separated path of the note the task lives in.
    pub file: String,
    /// 1-based line number within that note.
    pub line: u32,
    /// Task text after the checkbox, with the `📅 date` marker stripped (it rides
    /// in `due`); any other markers (priority, tags) are kept.
    pub text: String,
    pub done: bool,
    /// The `📅 YYYY-MM-DD` due date (always Some — only dated tasks are returned).
    pub due: Option<String>,
}

/// Tuning for [`scan_tasks`].
pub struct TaskScanOptions {
    /// Vault-relative folder prefixes whose tasks are NOT user todos (session
    /// logs, archive, agent memory). Caller passes the configured folder names.
    pub skip_prefixes: Vec<String>,
    /// Cap the result so a pathological vault can't return an unbounded list.
    pub max: usize,
}

impl Default for TaskScanOptions {
    fn default() -> Self {
        Self {
            skip_prefixes: ["05-agent/", "06-archive/", "07-logs/"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max: 2000,
        }
    }
}

static TASK_RE: OnceLock<Regex> = OnceLock::new();
static DUE_RE: OnceLock<Regex> = OnceLock::new();

/// Walk `vault_root`'s `.md` notes and collect every dated checkbox line.
/// Synchronous + best-effort (unreadable / non-UTF-8 files are skipped); callers
/// that must stay off the async runtime should wrap this in `spawn_blocking`.
pub fn scan_tasks(vault_root: &Path, opts: &TaskScanOptions) -> Vec<TaskHit> {
    let task_re = TASK_RE.get_or_init(|| Regex::new(r"^\s*-\s+\[(.)\]\s+(.+?)\s*$").unwrap());
    let due_re = DUE_RE.get_or_init(|| Regex::new(r"\x{1F4C5}\s*(\d{4}-\d{2}-\d{2})").unwrap());

    let mut tasks: Vec<TaskHit> = Vec::new();
    let walker = walkdir::WalkDir::new(vault_root)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| !is_tooling_dir(e));

    for entry in walker.flatten() {
        if tasks.len() >= opts.max {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let is_md = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !is_md {
            continue;
        }
        let rel = path
            .strip_prefix(vault_root)
            .map(to_slash)
            .unwrap_or_default();
        if opts
            .skip_prefixes
            .iter()
            .any(|p| rel.starts_with(p.as_str()))
        {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (i, line) in content.lines().enumerate() {
            if tasks.len() >= opts.max {
                break;
            }
            if let Some(caps) = task_re.captures(line) {
                let text = caps.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                let due = match due_re.captures(&text).and_then(|c| c.get(1)) {
                    Some(m) => m.as_str().to_string(),
                    None => continue, // undated → not a calendar todo
                };
                let status = caps.get(1).map(|m| m.as_str()).unwrap_or(" ");
                let text_clean = due_re.replace(&text, "").trim().to_string();
                tasks.push(TaskHit {
                    file: rel.clone(),
                    line: (i + 1) as u32,
                    text: text_clean,
                    done: status.eq_ignore_ascii_case("x"),
                    due: Some(due),
                });
            }
        }
    }
    tasks
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
    fn returns_only_dated_tasks_with_clean_text_and_line() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "01-projects/p.md", "# P\n\n- [ ] do the thing ⏫ 📅 2026-06-30\n- [ ] undated chore\n- [x] shipped it 📅 2026-06-01\n");
        let hits = scan_tasks(root, &TaskScanOptions::default());

        assert_eq!(hits.len(), 2, "undated line is excluded");
        let open = hits.iter().find(|t| !t.done).unwrap();
        assert_eq!(open.file, "01-projects/p.md");
        assert_eq!(open.line, 3); // 1-based; line 1 is '# P'
        assert_eq!(open.due.as_deref(), Some("2026-06-30"));
        assert_eq!(
            open.text, "do the thing ⏫",
            "📅 date stripped, other markers kept"
        );
        assert!(hits
            .iter()
            .any(|t| t.done && t.due.as_deref() == Some("2026-06-01")));
    }

    #[test]
    fn skips_excluded_folders_and_tooling_dirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "07-logs/session.md", "- [ ] log task 📅 2026-06-30\n");
        write(root, "06-archive/old.md", "- [ ] archived 📅 2026-06-30\n");
        write(root, ".obsidian/x.md", "- [ ] tooling 📅 2026-06-30\n");
        write(root, "00-inbox/i.md", "- [ ] real one 📅 2026-06-30\n");
        let hits = scan_tasks(root, &TaskScanOptions::default());

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file, "00-inbox/i.md");
    }

    #[test]
    fn respects_the_max_cap() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let body: String = (0..10)
            .map(|i| format!("- [ ] t{i} 📅 2026-06-30\n"))
            .collect();
        write(root, "01-projects/many.md", &body);
        let hits = scan_tasks(
            root,
            &TaskScanOptions {
                skip_prefixes: vec![],
                max: 4,
            },
        );
        assert_eq!(hits.len(), 4);
    }
}
