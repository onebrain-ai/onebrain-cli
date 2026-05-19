//! Orphan checkpoint scan · port of Bun's `runOrphanScan` (orphan-scan.ts).

use crate::frontmatter::parse_frontmatter;
use std::fs;
use std::path::Path;

/// Parse a checkpoint filename of the form `YYYY-MM-DD-{token}-checkpoint-NN.md`.
/// Returns `(date, token)` or `None` if the shape doesn't match.
#[allow(dead_code)] // used by upcoming orphan-scan tasks in this slice
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
#[allow(dead_code)] // used by upcoming collect_candidate_groups in Task 8
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
