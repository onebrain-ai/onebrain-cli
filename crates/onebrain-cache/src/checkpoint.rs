//! Checkpoint hook logic · handle_stop / handle_reset · state-file driven.

use std::fs;
use std::path::Path;

/// Scan `vault_root/logs_folder/checkpoint/` for files matching `{date}-{token}-checkpoint-NN.md`
/// · return the highest NN. Returns 0 if dir missing or no matching files.
#[allow(dead_code)] // used by upcoming handle_stop in Task 5
pub(crate) fn max_checkpoint_nn(
    vault_root: &Path,
    logs_folder: &str,
    date: &str,
    token: &str,
) -> u32 {
    let dir = vault_root.join(logs_folder).join("checkpoint");
    let prefix = format!("{date}-{token}-checkpoint-");
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut max = 0u32;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if !name.starts_with(&prefix) || !name.ends_with(".md") {
            continue;
        }
        // Extract NN: chars between "-checkpoint-" and ".md"
        let after_prefix = &name[prefix.len()..];
        let nn_str = after_prefix.trim_end_matches(".md");
        if nn_str.len() != 2 || !nn_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(nn) = nn_str.parse::<u32>() {
            if nn > max {
                max = nn;
            }
        }
    }
    max
}

#[cfg(test)]
mod max_nn_tests {
    use super::*;
    use tempfile::tempdir;

    fn touch(dir: &Path, rel: &str) {
        let p = dir.join(rel);
        if let Some(par) = p.parent() {
            fs::create_dir_all(par).unwrap();
        }
        fs::write(p, "x").unwrap();
    }

    #[test]
    fn returns_zero_when_dir_missing() {
        let dir = tempdir().unwrap();
        assert_eq!(
            max_checkpoint_nn(dir.path(), "07-logs", "2026-05-19", "tok"),
            0
        );
    }

    #[test]
    fn returns_zero_when_no_matching_files() {
        let dir = tempdir().unwrap();
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-19-OTHERTOK-checkpoint-01.md",
        );
        assert_eq!(
            max_checkpoint_nn(dir.path(), "07-logs", "2026-05-19", "tok"),
            0
        );
    }

    #[test]
    fn returns_max_nn_for_matching_token_and_date() {
        let dir = tempdir().unwrap();
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-19-tok-checkpoint-01.md",
        );
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-19-tok-checkpoint-03.md",
        );
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-19-tok-checkpoint-02.md",
        );
        assert_eq!(
            max_checkpoint_nn(dir.path(), "07-logs", "2026-05-19", "tok"),
            3
        );
    }

    #[test]
    fn ignores_files_with_different_date() {
        let dir = tempdir().unwrap();
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-18-tok-checkpoint-05.md",
        );
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-19-tok-checkpoint-02.md",
        );
        assert_eq!(
            max_checkpoint_nn(dir.path(), "07-logs", "2026-05-19", "tok"),
            2
        );
    }

    #[test]
    fn respects_custom_logs_folder() {
        let dir = tempdir().unwrap();
        touch(
            dir.path(),
            "custom-logs/checkpoint/2026-05-19-tok-checkpoint-04.md",
        );
        assert_eq!(
            max_checkpoint_nn(dir.path(), "custom-logs", "2026-05-19", "tok"),
            4
        );
    }
}
