//! `onebrain task list` — list dated vault tasks (fence-aware), filterable by
//! due date and folder. Wraps `onebrain_fs::task::scan_tasks`; the daemon API
//! and this verb share that one scanner.

use crate::cli::TaskListArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::{ensure, Context, Result};
use onebrain_core::{load_vault_config, VaultFolders};
use onebrain_fs::task::{scan_tasks, TaskHit, TaskScanOptions};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
struct TaskListData {
    tasks: Vec<TaskHit>,
    total: usize,
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &TaskListArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);
    let config = load_vault_config(&resolved.root).context("load vault config")?;

    let opts = TaskScanOptions {
        include_prefixes: resolve_prefixes(&config.folders, &args.folder),
        max: 2000,
    };
    let hits = scan_tasks(resolved.root.as_path(), &opts);

    let cutoff = match &args.due_by {
        Some(raw) => Some(resolve_due_by(raw)?),
        None => None,
    };
    let hits = apply_filters(hits, args.all, cutoff.as_deref());

    let total = hits.len();
    let envelope = Envelope::ok("task.list", Some(vault_info), TaskListData { tasks: hits, total });
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
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
        raw.len() == 10 && b[4] == b'-' && b[7] == b'-' && raw.bytes().filter(|c| *c == b'-').count() == 2,
        "invalid --due-by date: expected YYYY-MM-DD or `today`, got `{raw}`"
    );
    Ok(raw.to_string())
}

/// Apply the verb's filters: drop `TASKS.md` (Dataview query blocks, not real
/// tasks); drop done unless `--all`; keep only `due <= cutoff` when set.
fn apply_filters(hits: Vec<TaskHit>, all: bool, cutoff: Option<&str>) -> Vec<TaskHit> {
    hits.into_iter()
        .filter(|t| t.file != "TASKS.md")
        .filter(|t| all || !t.done)
        .filter(|t| match cutoff {
            Some(c) => t.due.as_deref().is_some_and(|d| d <= c),
            None => true,
        })
        .collect()
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
    out.push_str(&format!("\n{} task(s)\n", d.total));
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
        let out = apply_filters(hits, false, Some("2026-06-29"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].due.as_deref(), Some("2026-06-29"));
    }

    #[test]
    fn filters_all_keeps_done_and_no_cutoff_keeps_future() {
        let hits = vec![
            hit("01-projects/p.md", "2026-06-01", true),
            hit("01-projects/p.md", "2999-01-01", false),
        ];
        let out = apply_filters(hits, true, None);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn render_text_lists_tasks_with_count() {
        let env = Envelope::ok(
            "task.list",
            None,
            TaskListData { tasks: vec![hit("01-projects/p.md", "2026-06-29", false)], total: 1 },
        );
        let s = render_text(&env);
        assert!(s.contains("- [ ] t 📅 2026-06-29 (01-projects/p.md)"));
        assert!(s.contains("1 task(s)"));
    }

    #[test]
    fn render_text_handles_empty() {
        let env = Envelope::ok("task.list", None, TaskListData { tasks: vec![], total: 0 });
        assert_eq!(render_text(&env), "No tasks.");
    }
}
