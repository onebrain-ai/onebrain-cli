//! Scheduler log path helper. Mirrors Bun
//! `src/lib/scheduler/log-paths.ts`.
//!
//! Used by the scheduler runtime to write per-skill stdout/stderr logs into
//! `<logs_folder>/scheduler/YYYY/MM/YYYY-MM-DD-<skill>.{md,err.md}`.

use chrono::{Datelike, NaiveDate};

/// Build the relative log path for a scheduled skill invocation.
///
/// `is_error = true` switches the suffix from `.md` to `.err.md`. The leading
/// `/` on a skill name (`/daily`) is stripped so the filename is `daily.md`.
pub fn scheduler_log_path(
    logs_folder: &str,
    date: NaiveDate,
    skill: &str,
    is_error: bool,
) -> String {
    let yyyy = format!("{:04}", date.year());
    let mm = format!("{:02}", date.month());
    let dd = format!("{:02}", date.day());
    let skill_safe = skill.trim_start_matches('/');
    let suffix = if is_error { ".err.md" } else { ".md" };
    format!("{logs_folder}/scheduler/{yyyy}/{mm}/{yyyy}-{mm}-{dd}-{skill_safe}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_path_uses_md_suffix() {
        let out = scheduler_log_path(
            "07-logs",
            NaiveDate::from_ymd_opt(2026, 5, 12).unwrap(),
            "/daily",
            false,
        );
        assert_eq!(out, "07-logs/scheduler/2026/05/2026-05-12-daily.md");
    }

    #[test]
    fn error_path_uses_err_md_suffix() {
        let out = scheduler_log_path(
            "07-logs",
            NaiveDate::from_ymd_opt(2026, 5, 12).unwrap(),
            "/distill",
            true,
        );
        assert_eq!(out, "07-logs/scheduler/2026/05/2026-05-12-distill.err.md");
    }
}
