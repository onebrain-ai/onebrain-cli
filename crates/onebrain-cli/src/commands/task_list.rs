//! `onebrain task list` — list dated vault tasks (fence-aware), filterable by
//! due date and folder. Streams from `onebrain_fs::task::visit_tasks`; the
//! daemon API and this verb share the same scanner.

use crate::cli::TaskListArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::{ensure, Context, Result};
use onebrain_core::{load_vault_config, VaultFolders};
use onebrain_fs::task::{visit_tasks, TaskHit, TaskScanOptions};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
struct TaskListData {
    tasks: Vec<TaskHit>,
    total: usize,
}

struct RankedTask(TaskHit);

impl PartialEq for RankedTask {
    fn eq(&self, other: &Self) -> bool {
        task_order(&self.0, &other.0) == Ordering::Equal
    }
}

impl Eq for RankedTask {}

impl PartialOrd for RankedTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedTask {
    fn cmp(&self, other: &Self) -> Ordering {
        task_order(&self.0, &other.0)
    }
}

struct TaskCollector<'a> {
    all: bool,
    cutoff: Option<&'a str>,
    limit: Option<usize>,
    total: usize,
    selected: BinaryHeap<RankedTask>,
    unlimited: Vec<TaskHit>,
}

impl<'a> TaskCollector<'a> {
    fn new(all: bool, cutoff: Option<&'a str>, limit: Option<usize>) -> Self {
        Self {
            all,
            cutoff,
            limit,
            total: 0,
            selected: BinaryHeap::new(),
            unlimited: Vec::new(),
        }
    }

    fn consider(&mut self, task: TaskHit) {
        if !task_matches(&task, self.all, self.cutoff) {
            return;
        }
        self.total += 1;
        let Some(limit) = self.limit else {
            self.unlimited.push(task);
            return;
        };
        if limit == 0 {
            return;
        }

        if self.selected.len() < limit {
            self.selected.push(RankedTask(task));
        } else if self
            .selected
            .peek()
            .is_some_and(|worst| task_order(&task, &worst.0) == Ordering::Less)
        {
            self.selected.pop();
            self.selected.push(RankedTask(task));
        }
    }

    fn finish(mut self) -> (Vec<TaskHit>, usize) {
        let tasks = if self.limit.is_some() {
            self.selected
                .into_sorted_vec()
                .into_iter()
                .map(|ranked| ranked.0)
                .collect()
        } else {
            self.unlimited.sort_by(task_order);
            self.unlimited
        };
        (tasks, self.total)
    }
}

fn task_matches(task: &TaskHit, all: bool, cutoff: Option<&str>) -> bool {
    task.file != "TASKS.md"
        && !task.file.ends_with("/TASKS.md")
        && (all || !task.done)
        && match cutoff {
            Some(cutoff) => task.due.as_deref().is_some_and(|due| due <= cutoff),
            None => true,
        }
}

fn task_order(a: &TaskHit, b: &TaskHit) -> Ordering {
    let due = match (&a.due, &b.due) {
        (Some(a_due), Some(b_due)) => a_due.cmp(b_due),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    due.then_with(|| a.file.cmp(&b.file))
        .then_with(|| a.line.cmp(&b.line))
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &TaskListArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);
    let config = load_vault_config(&resolved.root).context("load vault config")?;

    let opts = scan_options(&config.folders, &args.folder);

    let cutoff = match &args.due_by {
        Some(raw) => Some(resolve_due_by(raw)?),
        None => None,
    };
    let mut collector = TaskCollector::new(args.all, cutoff.as_deref(), args.limit);
    visit_tasks(resolved.root.as_path(), &opts, |task| {
        collector.consider(task)
    });
    let (hits, total) = collector.finish();
    let envelope = Envelope::ok(
        "task.list",
        Some(vault_info),
        TaskListData { tasks: hits, total },
    );
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn scan_options(folders: &VaultFolders, explicit: &[String]) -> TaskScanOptions {
    TaskScanOptions {
        include_prefixes: resolve_prefixes(folders, explicit),
        // Filtering and top-N selection happen while the shared scanner
        // streams. A pre-filter cap would make `data.total` incorrect when
        // early hits are done or outside the due-date cutoff.
        max: usize::MAX,
    }
}

/// Explicit `--folder` flags win; otherwise scan projects + areas + inbox.
/// Each prefix is normalized to a trailing-slash form for `scan_tasks`.
fn resolve_prefixes(folders: &VaultFolders, explicit: &[String]) -> Vec<String> {
    let raw: Vec<&str> = if explicit.is_empty() {
        vec![&folders.projects, &folders.areas, &folders.inbox]
    } else {
        explicit.iter().map(String::as_str).collect()
    };
    raw.iter()
        .map(|f| format!("{}/", f.trim_end_matches('/')))
        .collect()
}

/// Resolve `--due-by` into a `YYYY-MM-DD` string. `today` → local date.
fn resolve_due_by(raw: &str) -> Result<String> {
    if raw.eq_ignore_ascii_case("today") {
        return Ok(chrono::Local::now().format("%Y-%m-%d").to_string());
    }
    let b = raw.as_bytes();
    ensure!(
        raw.len() == 10
            && b[4] == b'-'
            && b[7] == b'-'
            && raw.bytes().filter(|c| *c == b'-').count() == 2,
        "invalid --due-by date: expected YYYY-MM-DD or `today`, got `{raw}`"
    );
    Ok(raw.to_string())
}

fn render_text(env: &Envelope<TaskListData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.tasks.is_empty() {
        return "No tasks.".to_string();
    }
    let mut out = String::new();
    for t in &d.tasks {
        out.push_str(&format!(
            "- [{}] {} 📅 {} ({})\n",
            if t.done { "x" } else { " " },
            t.text,
            t.due.as_deref().unwrap_or(""),
            t.file,
        ));
    }
    if d.tasks.len() < d.total {
        out.push_str(&format!(
            "\nShowing {} of {} task(s)\n",
            d.tasks.len(),
            d.total
        ));
    } else {
        out.push_str(&format!("\n{} task(s)\n", d.total));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(file: &str, due: &str, done: bool) -> TaskHit {
        TaskHit {
            file: file.into(),
            line: 1,
            text: "t".into(),
            done,
            due: Some(due.into()),
        }
    }

    fn folders() -> VaultFolders {
        // Serde defaults give the canonical 00–07 names.
        serde_yaml::from_str("{}").unwrap()
    }

    fn collect(
        hits: Vec<TaskHit>,
        all: bool,
        cutoff: Option<&str>,
        limit: Option<usize>,
    ) -> (Vec<TaskHit>, usize) {
        let mut collector = TaskCollector::new(all, cutoff, limit);
        for hit in hits {
            collector.consider(hit);
        }
        collector.finish()
    }

    #[test]
    fn prefixes_default_to_projects_areas_inbox() {
        let p = resolve_prefixes(&folders(), &[]);
        assert_eq!(p, vec!["01-projects/", "02-areas/", "00-inbox/"]);
    }

    #[test]
    fn prefixes_explicit_override_and_normalize_slash() {
        let p = resolve_prefixes(&folders(), &["01-projects".into(), "02-areas/".into()]);
        assert_eq!(p, vec!["01-projects/", "02-areas/"]);
    }

    #[test]
    fn task_list_scan_is_unbounded_before_filters() {
        let opts = scan_options(&folders(), &[]);
        assert_eq!(opts.max, usize::MAX);
    }

    #[test]
    fn due_by_today_resolves_to_local_date() {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(resolve_due_by("TODAY").unwrap(), today);
        assert_eq!(resolve_due_by("2026-06-29").unwrap(), "2026-06-29");
    }

    #[test]
    fn due_by_rejects_malformed() {
        assert!(resolve_due_by("2026/06/29").is_err());
        assert!(resolve_due_by("tomorrow").is_err());
        assert!(resolve_due_by("2026-6-9").is_err());
    }

    #[test]
    fn filters_drop_tasks_md_done_and_future() {
        let hits = vec![
            hit("TASKS.md", "2026-06-01", false), // dropped: dashboard
            hit("01-projects/p.md", "2026-06-01", true), // dropped: done
            hit("01-projects/p.md", "2026-06-29", false), // kept
            hit("01-projects/p.md", "2026-07-15", false), // dropped: future
        ];
        let (out, total) = collect(hits, false, Some("2026-06-29"), None);
        assert_eq!(out.len(), 1);
        assert_eq!(total, 1);
        assert_eq!(out[0].due.as_deref(), Some("2026-06-29"));
    }

    #[test]
    fn filters_all_keeps_done_and_no_cutoff_keeps_future() {
        let hits = vec![
            hit("01-projects/p.md", "2026-06-01", true),
            hit("01-projects/p.md", "2999-01-01", false),
        ];
        let (out, total) = collect(hits, true, None, None);
        assert_eq!(out.len(), 2);
        assert_eq!(total, 2);
    }

    #[test]
    fn limit_returns_earliest_tasks_and_preserves_full_total() {
        let mut later_same_day = hit("01-projects/z.md", "2026-06-02", false);
        later_same_day.line = 8;
        let hits = vec![
            later_same_day,
            hit("01-projects/b.md", "2026-06-01", false),
            hit("01-projects/a.md", "2026-06-02", false),
        ];

        let (limited, total) = collect(hits, false, None, Some(2));

        assert_eq!(total, 3);
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].file, "01-projects/b.md");
        assert_eq!(limited[1].file, "01-projects/a.md");
    }

    #[test]
    fn streaming_limit_retains_only_requested_tasks_beyond_legacy_scan_cap() {
        let mut collector = TaskCollector::new(false, None, Some(5));
        for index in (0..2_501).rev() {
            let mut task = hit(&format!("01-projects/{index:04}.md"), "2026-06-01", false);
            task.text = format!("task-{index:04}");
            collector.consider(task);
        }

        assert_eq!(collector.total, 2_501);
        assert_eq!(collector.selected.len(), 5);
        let (tasks, total) = collector.finish();
        assert_eq!(total, 2_501);
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.text.as_str())
                .collect::<Vec<_>>(),
            [
                "task-0000",
                "task-0001",
                "task-0002",
                "task-0003",
                "task-0004"
            ]
        );
    }

    #[test]
    fn huge_limit_does_not_preallocate_the_user_supplied_capacity() {
        let mut collector = TaskCollector::new(false, None, Some(usize::MAX));
        collector.consider(hit("01-projects/a.md", "2026-06-01", false));

        let (tasks, total) = collector.finish();
        assert_eq!(tasks.len(), 1);
        assert_eq!(total, 1);
    }

    #[test]
    fn undated_tasks_sort_after_dated_tasks() {
        let mut undated = hit("01-projects/a.md", "2026-06-01", false);
        undated.due = None;
        let hits = vec![undated, hit("01-projects/z.md", "2026-06-02", false)];

        let (limited, total) = collect(hits, false, None, Some(1));

        assert_eq!(total, 2);
        assert_eq!(limited[0].file, "01-projects/z.md");
    }

    #[test]
    fn render_text_lists_tasks_with_count() {
        let env = Envelope::ok(
            "task.list",
            None,
            TaskListData {
                tasks: vec![hit("01-projects/p.md", "2026-06-29", false)],
                total: 1,
            },
        );
        let s = render_text(&env);
        assert!(s.contains("- [ ] t 📅 2026-06-29 (01-projects/p.md)"));
        assert!(s.contains("1 task(s)"));
    }

    #[test]
    fn render_text_reports_when_results_are_limited() {
        let env = Envelope::ok(
            "task.list",
            None,
            TaskListData {
                tasks: vec![hit("01-projects/p.md", "2026-06-29", false)],
                total: 3,
            },
        );
        let s = render_text(&env);
        assert!(s.contains("Showing 1 of 3 task(s)"));
    }

    #[test]
    fn render_text_handles_empty() {
        let env = Envelope::ok(
            "task.list",
            None,
            TaskListData {
                tasks: vec![],
                total: 0,
            },
        );
        assert_eq!(render_text(&env), "No tasks.");
    }

    #[test]
    fn render_text_done_task_uses_x_marker() {
        let env = Envelope::ok(
            "task.list",
            None,
            TaskListData {
                tasks: vec![hit("01-projects/p.md", "2026-06-29", true)],
                total: 1,
            },
        );
        let s = render_text(&env);
        assert!(s.contains("- [x] t 📅 2026-06-29 (01-projects/p.md)"));
    }

    #[test]
    fn filters_drop_nested_tasks_md() {
        let hits = vec![
            hit("01-projects/TASKS.md", "2026-06-01", false), // dropped: nested dashboard
            hit("TASKS.md", "2026-06-01", false),             // dropped: root dashboard
            hit("01-projects/p.md", "2026-06-01", false),     // kept
        ];
        let (out, total) = collect(hits, false, None, None);
        assert_eq!(out.len(), 1);
        assert_eq!(total, 1);
        assert_eq!(out[0].file, "01-projects/p.md");
    }

    #[test]
    fn render_text_due_none_emits_empty_date_field() {
        let no_due = TaskHit {
            file: "01-projects/p.md".into(),
            line: 1,
            text: "no date task".into(),
            done: false,
            due: None,
        };
        let env = Envelope::ok(
            "task.list",
            None,
            TaskListData {
                tasks: vec![no_due],
                total: 1,
            },
        );
        let s = render_text(&env);
        assert!(
            s.contains("- [ ] no date task 📅  (01-projects/p.md)"),
            "got: {s}"
        );
    }

    #[test]
    fn filters_cutoff_drops_task_with_no_due_date() {
        let no_due = TaskHit {
            file: "01-projects/p.md".into(),
            line: 1,
            text: "t".into(),
            done: false,
            due: None,
        };
        let (out, total) = collect(vec![no_due], false, Some("2026-06-29"), None);
        assert!(
            out.is_empty(),
            "task with no due date should be dropped when cutoff is set"
        );
        assert_eq!(total, 0);
    }
}
