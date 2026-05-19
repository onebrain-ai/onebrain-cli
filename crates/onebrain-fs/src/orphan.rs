//! Orphan checkpoint scan · port of Bun's `runOrphanScan` (orphan-scan.ts).

use crate::frontmatter::parse_frontmatter;
use std::fs;
use std::path::Path;

/// Parse a checkpoint filename of the form `YYYY-MM-DD-{token}-checkpoint-NN.md`.
/// Returns `(date, token)` or `None` if the shape doesn't match.
pub(crate) fn parse_checkpoint_filename(name: &str) -> Option<(&str, &str)> {
    // YYYY-MM-DD prefix — exactly 10 chars (4-2-2 with dashes at idx 4 and 7)
    if name.len() < 11 {
        return None;
    }
    let bytes = name.as_bytes();
    let valid_date_chars = bytes[..10].iter().enumerate().all(|(i, &b)| match i {
        4 | 7 => b == b'-',
        _ => b.is_ascii_digit(),
    });
    if !valid_date_chars || bytes[10] != b'-' {
        return None;
    }
    let date = &name[..10];
    let after_date = &name[11..];
    let cp_idx = after_date.find("-checkpoint-")?;
    let token = &after_date[..cp_idx];
    if token.is_empty() {
        return None;
    }
    Some((date, token))
}

/// Check whether the given date has a manually-run (non-auto-saved) session log
/// in the given month directory.
///
/// A session log is recognised by `-session-` infix and `.md` suffix (whitelist —
/// NOT a `-checkpoint-` blacklist · the logs folder also contains update/weekly logs).
/// "Manual" means: frontmatter missing OR frontmatter present but `auto-saved` is
/// false / absent / not a recognised truthy value. Reading errors → file skipped.
pub(crate) fn has_manual_session_log(month_dir: &Path, date: &str) -> bool {
    let entries = match fs::read_dir(month_dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if !name_str.starts_with(date)
            || !name_str.contains("-session-")
            || !name_str.ends_with(".md")
        {
            continue;
        }
        let content = match fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        match parse_frontmatter(&content) {
            Some(fm) => {
                let auto_saved = fm.get("auto-saved");
                let is_auto = matches!(auto_saved, Some(serde_yaml::Value::Bool(true)))
                    || matches!(
                        auto_saved,
                        Some(serde_yaml::Value::String(s)) if s == "true"
                    );
                if !is_auto {
                    return true;
                }
            }
            None => return true, // no frontmatter → manual
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_filename() {
        let (date, token) =
            parse_checkpoint_filename("2026-05-19-abc123-checkpoint-01.md").unwrap();
        assert_eq!(date, "2026-05-19");
        assert_eq!(token, "abc123");
    }

    #[test]
    fn parses_multi_digit_nn() {
        let (date, token) = parse_checkpoint_filename("2026-05-19-tok-checkpoint-99.md").unwrap();
        assert_eq!(date, "2026-05-19");
        assert_eq!(token, "tok");
    }

    #[test]
    fn rejects_filename_without_checkpoint_infix() {
        assert!(parse_checkpoint_filename("2026-05-19-tok-session-01.md").is_none());
    }

    #[test]
    fn rejects_filename_without_date_prefix() {
        assert!(parse_checkpoint_filename("tok-checkpoint-01.md").is_none());
    }

    #[test]
    fn rejects_empty_token() {
        // "2026-05-19--checkpoint-01.md" has empty token between date and -checkpoint-
        assert!(parse_checkpoint_filename("2026-05-19--checkpoint-01.md").is_none());
    }
}

#[cfg(test)]
mod manual_log_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn returns_false_when_session_dir_missing() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does_not_exist");
        assert!(!has_manual_session_log(&missing, "2026-05-19"));
    }

    #[test]
    fn returns_true_when_manual_session_log_exists() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("2026-05-19-session-01.md"),
            "---\ntags: [session-log]\nauto-saved: false\n---\nbody",
        )
        .unwrap();
        assert!(has_manual_session_log(dir.path(), "2026-05-19"));
    }

    #[test]
    fn returns_false_when_only_auto_saved_log_exists() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("2026-05-19-session-01.md"),
            "---\nauto-saved: true\n---\nbody",
        )
        .unwrap();
        assert!(!has_manual_session_log(dir.path(), "2026-05-19"));
    }

    #[test]
    fn accepts_auto_saved_as_quoted_string() {
        // Bun handles both `auto-saved: true` (bool) and `auto-saved: "true"` (string)
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("2026-05-19-session-01.md"),
            "---\nauto-saved: \"true\"\n---\nbody",
        )
        .unwrap();
        assert!(!has_manual_session_log(dir.path(), "2026-05-19"));
    }

    #[test]
    fn ignores_non_session_files_with_matching_date() {
        // *-update-v3.0.0.md or *-weekly.md must NOT be treated as session logs.
        // Whitelist filter on `-session-` infix is required.
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("2026-05-19-update-v3.0.0.md"),
            "---\n---\nrelease notes",
        )
        .unwrap();
        assert!(!has_manual_session_log(dir.path(), "2026-05-19"));
    }

    #[test]
    fn returns_true_when_mixed_with_manual_log() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("2026-05-19-update-v3.0.0.md"),
            "---\n---\nrelease notes",
        )
        .unwrap();
        fs::write(
            dir.path().join("2026-05-19-session-01.md"),
            "---\nauto-saved: false\n---\nwrapup",
        )
        .unwrap();
        assert!(has_manual_session_log(dir.path(), "2026-05-19"));
    }

    #[test]
    fn missing_frontmatter_in_session_log_counts_as_manual() {
        // Bun: "Either no frontmatter or auto-saved is false/absent → manual log"
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("2026-05-19-session-01.md"),
            "no frontmatter here",
        )
        .unwrap();
        assert!(has_manual_session_log(dir.path(), "2026-05-19"));
    }
}

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

/// Collect checkpoint files into per-token groups, applying the today / current-token
/// / manual-log filters. Returns `HashMap<token, [absolute_paths]>`.
#[allow(dead_code)] // used by upcoming scan_orphans in Task 9
pub(crate) fn collect_candidate_groups(
    checkpoint_dir: &Path,
    session_dir: &Path,
    current_token: &str,
    today: &str,
) -> HashMap<String, Vec<PathBuf>> {
    let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut manual_log_cache: HashMap<String, bool> = HashMap::new();

    let entries = match fs::read_dir(checkpoint_dir) {
        Ok(e) => e,
        Err(_) => return groups,
    };

    for entry in entries.flatten() {
        let name_os = entry.file_name();
        let name = match name_os.to_str() {
            Some(s) => s,
            None => continue,
        };
        if !name.ends_with(".md") {
            continue;
        }
        let (date, token) = match parse_checkpoint_filename(name) {
            Some(t) => t,
            None => continue,
        };
        if date == today {
            continue;
        }
        if token == current_token {
            continue;
        }
        // Manual log check with per-date cache
        let manual = *manual_log_cache.entry(date.to_string()).or_insert_with(|| {
            let year = &date[..4];
            let month = &date[5..7];
            let month_dir = session_dir.join(year).join(month);
            has_manual_session_log(&month_dir, date)
        });
        if manual {
            continue;
        }
        groups
            .entry(token.to_string())
            .or_default()
            .push(entry.path());
    }

    groups
}

/// File mtime as milliseconds since UNIX epoch · None on any error.
pub(crate) fn get_mtime_ms(path: &Path) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let dur = mtime.duration_since(UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as u64)
}

/// Newest mtime across a file list · None if the list is empty OR any stat fails
/// (fail-safe propagation — one ambiguous file forces the whole group ambiguous).
pub(crate) fn get_newest_mtime_ms(paths: &[PathBuf]) -> Option<u64> {
    if paths.is_empty() {
        return None;
    }
    let mut newest: u64 = 0;
    for p in paths {
        let m = get_mtime_ms(p)?;
        if m > newest {
            newest = m;
        }
    }
    Some(newest)
}

use onebrain_core::{load_vault_config_at, CoreError};
use std::io::Write;

const MIN_GUARD_MINUTES: u64 = 60;
const DEFAULT_ACTIVE_SESSION_GUARD_MS: u64 = 60 * 60 * 1000;

/// Resolve the Active-Session Guard threshold in milliseconds from `vault.yml`'s
/// `checkpoint.minutes`. Policy: `max(60min, 2 * checkpoint.minutes)` minutes.
///
/// Fail-safe: returns 60min on missing/malformed vault.yml. Expected absence
/// (ENOENT-equivalent) is silent · real malformations emit a stderr warning.
#[allow(dead_code)] // used by upcoming scan_orphans in Task 9
pub(crate) fn active_session_guard_ms(vault_root: &Path) -> u64 {
    match load_vault_config_at(vault_root) {
        Ok(config) => {
            let cp_minutes = config.checkpoint.minutes as u64;
            let minutes = MIN_GUARD_MINUTES.max(2 * cp_minutes);
            minutes * 60 * 1000
        }
        Err(CoreError::VaultYamlMissing { .. }) => {
            // Expected absence — silent fallback.
            DEFAULT_ACTIVE_SESSION_GUARD_MS
        }
        Err(other) => {
            // Real malformation — emit warning to stderr (best-effort, never panic).
            let _ = writeln!(
                std::io::stderr(),
                "onebrain orphan-scan: vault.yml unreadable, using {}-min Active-Session Guard default ({other})",
                MIN_GUARD_MINUTES
            );
            DEFAULT_ACTIVE_SESSION_GUARD_MS
        }
    }
}

/// Whether a group of checkpoint files should be skipped (active in another harness
/// or otherwise ambiguous). Returns `true` → caller must NOT count this group.
///
/// Fail-safe: any uncertainty (empty group · stat failure · future mtime · zero
/// guard) returns `true` so the group is skipped. Bun symmetry: `/wrapup` uses
/// the same predicate, so any "skip here" lines up with "wrapup also skips."
#[allow(dead_code)] // used by upcoming scan_orphans in Task 9
pub(crate) fn is_group_active_or_ambiguous(paths: &[PathBuf], now_ms: u64, guard_ms: u64) -> bool {
    if guard_ms == 0 {
        return true;
    }
    let newest = match get_newest_mtime_ms(paths) {
        Some(n) => n,
        None => return true,
    };
    if newest > now_ms {
        // Future mtime / clock skew → fail-safe ambiguous
        return true;
    }
    let age_ms = now_ms - newest;
    age_ms < guard_ms
}

#[cfg(test)]
mod guard_threshold_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    const MIN_GUARD_MS: u64 = 60 * 60 * 1000;

    #[test]
    fn falls_back_to_60_min_when_vault_yml_missing() {
        let dir = tempdir().unwrap();
        // No vault.yml inside
        assert_eq!(active_session_guard_ms(dir.path()), MIN_GUARD_MS);
    }

    #[test]
    fn uses_60_min_default_when_checkpoint_block_absent() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("vault.yml"), "# no checkpoint key\n").unwrap();
        // checkpoint.minutes defaults to 30 → max(60, 60) = 60 min
        assert_eq!(active_session_guard_ms(dir.path()), MIN_GUARD_MS);
    }

    #[test]
    fn checkpoint_minutes_60_yields_120_min_threshold() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("vault.yml"), "checkpoint:\n  minutes: 60\n").unwrap();
        assert_eq!(active_session_guard_ms(dir.path()), 120 * 60 * 1000);
    }

    #[test]
    fn checkpoint_minutes_15_clamps_to_60_min_floor() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("vault.yml"), "checkpoint:\n  minutes: 15\n").unwrap();
        // max(60, 2*15=30) = 60
        assert_eq!(active_session_guard_ms(dir.path()), MIN_GUARD_MS);
    }

    #[test]
    fn malformed_vault_yml_falls_back_to_60_min() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("vault.yml"), "not: : valid").unwrap();
        assert_eq!(active_session_guard_ms(dir.path()), MIN_GUARD_MS);
    }
}

#[cfg(test)]
mod active_predicate_tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn write_with_mtime(
        dir: &std::path::Path,
        name: &str,
        secs_before_now: u64,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, "x").unwrap();
        let when = SystemTime::now() - Duration::from_secs(secs_before_now);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(when)).unwrap();
        path
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn group_under_guard_is_active() {
        let dir = tempdir().unwrap();
        let f = write_with_mtime(dir.path(), "a.md", 60); // 1 min ago
        let guard = 60 * 60 * 1000; // 60 min
        assert!(is_group_active_or_ambiguous(&[f], now_ms(), guard));
    }

    #[test]
    fn group_well_over_guard_is_counted() {
        let dir = tempdir().unwrap();
        let f = write_with_mtime(dir.path(), "a.md", 2 * 3600); // 2 hours ago
        let guard = 60 * 60 * 1000;
        assert!(!is_group_active_or_ambiguous(&[f], now_ms(), guard));
    }

    #[test]
    fn empty_group_is_ambiguous() {
        assert!(is_group_active_or_ambiguous(&[], now_ms(), 60 * 60 * 1000));
    }

    #[test]
    fn group_with_missing_file_is_ambiguous() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does_not_exist.md");
        assert!(is_group_active_or_ambiguous(
            &[missing],
            now_ms(),
            60 * 60 * 1000
        ));
    }

    #[test]
    fn future_mtime_clock_skew_is_ambiguous() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("future.md");
        fs::write(&f, "x").unwrap();
        let future = SystemTime::now() + Duration::from_secs(3600);
        filetime::set_file_mtime(&f, filetime::FileTime::from_system_time(future)).unwrap();
        // ageMs negative → ambiguous (skip)
        assert!(is_group_active_or_ambiguous(&[f], now_ms(), 60 * 60 * 1000));
    }

    #[test]
    fn zero_guard_ms_is_ambiguous() {
        let dir = tempdir().unwrap();
        let f = write_with_mtime(dir.path(), "a.md", 3600);
        assert!(is_group_active_or_ambiguous(&[f], now_ms(), 0));
    }

    #[test]
    fn newest_mtime_decides_for_mixed_age_group() {
        let dir = tempdir().unwrap();
        let old = write_with_mtime(dir.path(), "old.md", 7200);
        let fresh = write_with_mtime(dir.path(), "fresh.md", 60);
        let guard = 60 * 60 * 1000;
        // newest is `fresh` → 1 min ago → under guard → active
        assert!(is_group_active_or_ambiguous(&[old, fresh], now_ms(), guard));
    }
}

#[cfg(test)]
mod mtime_tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn set_mtime(path: &std::path::Path, secs_before_now: u64) {
        let when = SystemTime::now() - Duration::from_secs(secs_before_now);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when))
            .expect("setting mtime failed");
    }

    #[test]
    fn get_mtime_ms_returns_some_for_existing_file() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.md");
        fs::write(&f, "x").unwrap();
        assert!(get_mtime_ms(&f).is_some());
    }

    #[test]
    fn get_mtime_ms_returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("missing.md");
        assert_eq!(get_mtime_ms(&f), None);
    }

    #[test]
    fn newest_mtime_returns_none_for_empty_input() {
        let files: Vec<std::path::PathBuf> = vec![];
        assert_eq!(get_newest_mtime_ms(&files), None);
    }

    #[test]
    fn newest_mtime_picks_largest_value() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "x").unwrap();
        fs::write(&b, "y").unwrap();
        set_mtime(&a, 3600); // 1 hour old
        set_mtime(&b, 60); // 1 min old
        let newest = get_newest_mtime_ms(&[a.clone(), b.clone()]).unwrap();
        let b_mtime = get_mtime_ms(&b).unwrap();
        assert_eq!(newest, b_mtime);
    }

    #[test]
    fn newest_mtime_returns_none_if_any_file_missing() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("does_not_exist.md");
        fs::write(&a, "x").unwrap();
        assert_eq!(get_newest_mtime_ms(&[a, b]), None);
    }
}

#[cfg(test)]
mod collect_groups_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn touch(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn empty_checkpoint_dir_returns_empty_map() {
        let dir = tempdir().unwrap();
        let cp = dir.path().join("checkpoint");
        fs::create_dir_all(&cp).unwrap();
        let sess = dir.path().join("session");
        let groups = collect_candidate_groups(&cp, &sess, "abc12345", "2026-05-19");
        assert!(groups.is_empty());
    }

    #[test]
    fn skips_current_session_token() {
        let dir = tempdir().unwrap();
        let cp = dir.path().join("checkpoint");
        let sess = dir.path().join("session");
        touch(&cp, "2026-05-18-abc12345-checkpoint-01.md", "x");
        let groups = collect_candidate_groups(&cp, &sess, "abc12345", "2026-05-19");
        assert!(groups.is_empty());
    }

    #[test]
    fn skips_today_files() {
        let dir = tempdir().unwrap();
        let cp = dir.path().join("checkpoint");
        let sess = dir.path().join("session");
        touch(&cp, "2026-05-19-other-checkpoint-01.md", "x");
        let groups = collect_candidate_groups(&cp, &sess, "abc12345", "2026-05-19");
        assert!(groups.is_empty());
    }

    #[test]
    fn skips_date_with_manual_session_log() {
        let dir = tempdir().unwrap();
        let cp = dir.path().join("checkpoint");
        let sess = dir.path().join("session");
        touch(&cp, "2026-05-18-stale-checkpoint-01.md", "x");
        touch(
            &sess,
            "2026/05/2026-05-18-session-01.md",
            "---\nauto-saved: false\n---\nwrapup",
        );
        let groups = collect_candidate_groups(&cp, &sess, "abc12345", "2026-05-19");
        assert!(
            groups.is_empty(),
            "manual log for 2026-05-18 should suppress the stale checkpoint"
        );
    }

    #[test]
    fn groups_multiple_checkpoints_under_same_token() {
        let dir = tempdir().unwrap();
        let cp = dir.path().join("checkpoint");
        let sess = dir.path().join("session");
        touch(&cp, "2026-05-17-tokA-checkpoint-01.md", "x");
        touch(&cp, "2026-05-17-tokA-checkpoint-02.md", "x");
        touch(&cp, "2026-05-17-tokA-checkpoint-03.md", "x");
        let groups = collect_candidate_groups(&cp, &sess, "abc12345", "2026-05-19");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups.get("tokA").map(|v| v.len()), Some(3));
    }

    #[test]
    fn groups_distinct_tokens_separately() {
        let dir = tempdir().unwrap();
        let cp = dir.path().join("checkpoint");
        let sess = dir.path().join("session");
        touch(&cp, "2026-05-17-tokA-checkpoint-01.md", "x");
        touch(&cp, "2026-05-17-tokB-checkpoint-01.md", "x");
        let groups = collect_candidate_groups(&cp, &sess, "abc12345", "2026-05-19");
        assert_eq!(groups.len(), 2);
        assert!(groups.contains_key("tokA"));
        assert!(groups.contains_key("tokB"));
    }

    #[test]
    fn ignores_files_with_bad_filename_shape() {
        let dir = tempdir().unwrap();
        let cp = dir.path().join("checkpoint");
        let sess = dir.path().join("session");
        touch(&cp, "random_file.md", "x");
        touch(&cp, "2026-05-18-no-cp-marker-01.md", "x");
        touch(&cp, "not-even-a-date.md", "x");
        let groups = collect_candidate_groups(&cp, &sess, "abc12345", "2026-05-19");
        assert!(groups.is_empty());
    }
}
