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
    /// Vault-relative folder prefixes to scan (allowlist, e.g. `01-projects/`,
    /// `02-areas/`). ONLY tasks under these folders are returned — this keeps the
    /// scan to actionable project/area notes and excludes documentation
    /// (READMEs), the inbox, knowledge, resources, logs, archive, agent memory,
    /// etc. An empty list scans the whole vault. Callers pass the configured
    /// folder names (`folders.projects` / `folders.areas`).
    pub include_prefixes: Vec<String>,
    /// Cap the result so a pathological vault can't return an unbounded list.
    pub max: usize,
}

impl Default for TaskScanOptions {
    fn default() -> Self {
        Self {
            include_prefixes: ["01-projects/", "02-areas/"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max: 2000,
        }
    }
}

/// If `trimmed` (already left-trimmed) is a fenced-code-block marker, return
/// `(fence_char, run_length, has_info_string)`. A marker is 3+ backticks or
/// 3+ tildes. Frontmatter / thematic-break `---` is not a fence.
fn fence_marker(trimmed: &str) -> Option<(char, usize, bool)> {
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    // `ch` is ASCII (1 byte), so the run length in chars == byte length of the
    // prefix; slicing at `run` below is a valid UTF-8 boundary.
    let run = trimmed.chars().take_while(|&c| c == ch).count();
    if run < 3 {
        return None;
    }
    let has_info = !trimmed[run..].trim().is_empty();
    Some((ch, run, has_info))
}

/// Tracks open fenced-code-block state across one file's lines.
#[derive(Default)]
struct FenceState {
    /// `Some((fence_char, opening_run_len))` while inside a fence.
    open: Option<(char, usize)>,
}

impl FenceState {
    /// Feed the next line; returns true if the line is part of a code block
    /// (the fence delimiters themselves included) and must be ignored for
    /// task scanning.
    fn in_code(&mut self, line: &str) -> bool {
        let marker = fence_marker(line.trim_start());
        match (self.open, marker) {
            // Inside a fence: close only on a same-char run >= opener with no
            // info string (a CommonMark closing fence carries no info string).
            (Some((open_ch, open_len)), Some((ch, len, has_info))) => {
                if ch == open_ch && len >= open_len && !has_info {
                    self.open = None;
                }
                true
            }
            (Some(_), None) => true,
            // Outside a fence: an opening marker starts one.
            (None, Some((ch, len, _))) => {
                self.open = Some((ch, len));
                true
            }
            (None, None) => false,
        }
    }
}

static TASK_RE: OnceLock<Regex> = OnceLock::new();
static DUE_RE: OnceLock<Regex> = OnceLock::new();

/// Walk `vault_root`'s `.md` notes and collect every dated checkbox line.
/// Synchronous + best-effort (unreadable / non-UTF-8 files are skipped); callers
/// that must stay off the async runtime should wrap this in `spawn_blocking`.
pub fn scan_tasks(vault_root: &Path, opts: &TaskScanOptions) -> Vec<TaskHit> {
    let mut tasks = Vec::new();
    visit_tasks(vault_root, opts, |task| tasks.push(task));
    tasks
}

/// Walk `vault_root`'s `.md` notes and pass each dated checkbox line to
/// `visitor` without retaining prior results. `opts.max` still caps the number
/// of visited matches, so existing callers can keep the pathological-vault
/// guard while streaming callers choose a larger bound and control retention.
pub fn visit_tasks(vault_root: &Path, opts: &TaskScanOptions, mut visitor: impl FnMut(TaskHit)) {
    let task_re = TASK_RE.get_or_init(|| Regex::new(r"^\s*-\s+\[(.)\]\s+(.+?)\s*$").unwrap());
    let due_re = DUE_RE.get_or_init(|| Regex::new(r"\x{1F4C5}\s*(\d{4}-\d{2}-\d{2})").unwrap());

    let mut visited = 0;
    let walker = walkdir::WalkDir::new(vault_root)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| !is_tooling_dir(e));

    'walk: for entry in walker.flatten() {
        if visited >= opts.max {
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
        // Allowlist: scan only the configured task folders (projects + areas).
        // An empty list means "scan everything".
        if !opts.include_prefixes.is_empty()
            && !opts
                .include_prefixes
                .iter()
                .any(|p| rel.starts_with(p.as_str()))
        {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut fence = FenceState::default();
        for (i, line) in content.lines().enumerate() {
            if visited >= opts.max {
                break 'walk;
            }
            // Checkbox lines inside ``` / ~~~ fences are documentation, not
            // calendar todos — skip them (fixes demo-task pollution).
            if fence.in_code(line) {
                continue;
            }
            if let Some(caps) = task_re.captures(line) {
                let text = caps.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                let due = match due_re.captures(&text).and_then(|c| c.get(1)) {
                    Some(m) => m.as_str().to_string(),
                    None => continue, // undated → not a calendar todo
                };
                let status = caps.get(1).map(|m| m.as_str()).unwrap_or(" ");
                let text_clean = due_re.replace(&text, "").trim().to_string();
                visitor(TaskHit {
                    file: rel.clone(),
                    line: (i + 1) as u32,
                    text: text_clean,
                    done: status.eq_ignore_ascii_case("x"),
                    due: Some(due),
                });
                visited += 1;
            }
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
    fn includes_only_projects_and_areas() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Allowlisted (projects + areas) — included.
        write(
            root,
            "01-projects/p.md",
            "- [ ] project task 📅 2026-06-30\n",
        );
        write(
            root,
            "02-areas/health.md",
            "- [ ] area task 📅 2026-06-30\n",
        );
        // Everything else — excluded (README docs, inbox, knowledge, logs, tooling).
        write(
            root,
            "README.md",
            "- [ ] High priority task 🔺 📅 2026-03-22\n",
        );
        write(root, "00-inbox/i.md", "- [ ] inbox 📅 2026-06-30\n");
        write(root, "03-knowledge/k.md", "- [ ] knowledge 📅 2026-06-30\n");
        write(root, "07-logs/session.md", "- [ ] log 📅 2026-06-30\n");
        write(root, ".obsidian/x.md", "- [ ] tooling 📅 2026-06-30\n");
        let hits = scan_tasks(root, &TaskScanOptions::default());

        assert_eq!(hits.len(), 2, "only projects + areas: {hits:?}");
        let files: Vec<&str> = hits.iter().map(|t| t.file.as_str()).collect();
        assert!(files.contains(&"01-projects/p.md"));
        assert!(files.contains(&"02-areas/health.md"));
        assert!(!files.iter().any(|f| f.starts_with("README")));
    }

    #[test]
    fn empty_include_scans_everything() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "anywhere/x.md", "- [ ] a 📅 2026-06-30\n");
        write(root, "README.md", "- [ ] b 📅 2026-06-30\n");
        let hits = scan_tasks(
            root,
            &TaskScanOptions {
                include_prefixes: vec![],
                max: 2000,
            },
        );
        assert_eq!(hits.len(), 2, "empty allowlist = scan all");
    }

    #[test]
    fn excludes_tasks_inside_fenced_code_blocks() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // A real task, then a fenced block whose task lines must be ignored,
        // then another real task after the fence closes.
        write(
            root,
            "01-projects/p.md",
            "- [ ] real one 📅 2026-06-30\n\
             ```\n\
             - [ ] fenced demo 📅 2026-05-20\n\
             - [ ] another fenced 📅 2026-05-22\n\
             ```\n\
             - [ ] real two 📅 2026-07-01\n",
        );
        let hits = scan_tasks(root, &TaskScanOptions::default());
        let texts: Vec<&str> = hits.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(hits.len(), 2, "fenced tasks excluded: {texts:?}");
        assert!(texts.contains(&"real one"));
        assert!(texts.contains(&"real two"));
        assert!(!texts.iter().any(|t| t.contains("fenced")));
    }

    #[test]
    fn fence_tilde_variant_and_info_string() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "01-projects/p.md",
            "~~~rust\n- [ ] tilde fenced 📅 2026-05-20\n~~~\n- [ ] after 📅 2026-06-30\n",
        );
        let hits = scan_tasks(root, &TaskScanOptions::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "after");
    }

    #[test]
    fn longer_outer_fence_not_closed_by_shorter_inner() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // 4-backtick opener; an inner 3-backtick line must NOT close it.
        write(
            root,
            "01-projects/p.md",
            "````\n```\n- [ ] still fenced 📅 2026-05-20\n````\n- [ ] out 📅 2026-06-30\n",
        );
        let hits = scan_tasks(root, &TaskScanOptions::default());
        assert_eq!(hits.len(), 1, "{:?}", hits);
        assert_eq!(hits[0].text, "out");
    }

    #[test]
    fn frontmatter_dashes_do_not_start_a_fence() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "01-projects/p.md",
            "---\ntags: [x]\n---\n- [ ] after frontmatter 📅 2026-06-30\n",
        );
        let hits = scan_tasks(root, &TaskScanOptions::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "after frontmatter");
    }

    #[test]
    fn fence_marker_classifies_lines() {
        assert_eq!(fence_marker("```"), Some(('`', 3, false)));
        assert_eq!(fence_marker("```rust"), Some(('`', 3, true)));
        assert_eq!(fence_marker("~~~~"), Some(('~', 4, false)));
        assert_eq!(fence_marker("``"), None, "only 2 backticks");
        assert_eq!(fence_marker("- [ ] x"), None);
        assert_eq!(fence_marker("---"), None, "frontmatter dash is not a fence");
        assert_eq!(fence_marker(""), None, "empty string early return");
    }

    #[test]
    fn fence_state_toggles() {
        let mut f = FenceState::default();
        assert!(!f.in_code("plain text"));
        assert!(f.in_code("```"), "opener is in-code");
        assert!(f.in_code("inside"), "body is in-code");
        assert!(f.in_code("```"), "closer is in-code");
        assert!(!f.in_code("plain again"), "closed");
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
                include_prefixes: vec!["01-projects/".into()],
                max: 4,
            },
        );
        assert_eq!(hits.len(), 4);
    }

    #[test]
    fn visitor_streams_more_than_default_cap_without_collecting_results() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let body: String = (0..2_501)
            .map(|i| format!("- [ ] t{i} 📅 2026-06-30\n"))
            .collect();
        write(root, "01-projects/many.md", &body);
        let mut count = 0;

        visit_tasks(
            root,
            &TaskScanOptions {
                include_prefixes: vec!["01-projects/".into()],
                max: usize::MAX,
            },
            |_| count += 1,
        );

        assert_eq!(count, 2_501);
    }
}
