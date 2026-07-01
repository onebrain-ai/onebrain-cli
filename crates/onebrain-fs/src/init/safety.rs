//! Target-directory safety check for `onebrain init`.
//!
//! Without this guard, `onebrain init` in the wrong directory (cwd is a
//! random project, the user's home dir, an existing codebase) pollutes the
//! tree with `00-inbox/`, `01-projects/`, …, `vault.yml`, `.claude/`, and
//! friends. The user can't easily undo it.
//!
//! Detection logic (run BEFORE the existing vault.yml guard so we exit
//! before any vault.yml-related write):
//!
//! 1. Target does not exist → caller will create it; proceed.
//! 2. Target exists and is empty (zero entries) → proceed.
//! 3. Target contains `onebrain.yml` or legacy `vault.yml` (existing or
//!    partial OneBrain vault) → delegate to the existing config guard;
//!    safety check is a no-op.
//! 4. Target contains other files / hidden files (e.g. `README.md`, `.git/`,
//!    `.DS_Store`, `node_modules/`) → request confirmation. If `--force` is
//!    set OR structured output mode is active, do not prompt.
//!
//! Edge cases:
//!   - Permission denied while reading the directory bubbles up via
//!     `FsError::Io` (mapped to `EXIT_FS_ERROR` 66 in the CLI binary).
//!   - Symlinks are resolved by the OS — we don't traverse, only enumerate
//!     top-level entries.
//!   - The check is intentionally NOT recursive: presence of any entry at
//!     the top level is enough to trigger.

use crate::{FsError, Result};
use onebrain_core::{CONFIG_FILENAME, LEGACY_CONFIG_FILENAME};
use std::path::Path;

/// Classification of a target directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DirState {
    /// Target does not exist on disk; caller will create it.
    Missing,
    /// Target exists with zero entries.
    Empty,
    /// Target exists and contains an OneBrain config file (canonical
    /// `onebrain.yml` or legacy `vault.yml`) — treat as a (possibly
    /// partial) OneBrain vault and let the existing guard handle it.
    OneBrainVault,
    /// Target exists with at least one entry and no `vault.yml`. Holds a
    /// short summary (count + sample entries) for the prompt text.
    NonEmptyNonVault { summary: String },
}

/// Inspect the target directory and return its [`DirState`].
pub(crate) fn classify(vault_dir: &Path) -> Result<DirState> {
    let meta = match std::fs::symlink_metadata(vault_dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DirState::Missing),
        Err(e) => {
            return Err(FsError::Io {
                path: vault_dir.to_path_buf(),
                source: e,
            });
        }
    };
    if !meta.is_dir() {
        // A regular file at the target path: treat as a non-empty non-vault
        // path. The downstream write attempts will fail with a sensible
        // FsError; here we just return a meaningful state for the prompt.
        return Ok(DirState::NonEmptyNonVault {
            summary: "target exists but is not a directory".to_string(),
        });
    }

    let entries = std::fs::read_dir(vault_dir).map_err(|e| FsError::Io {
        path: vault_dir.to_path_buf(),
        source: e,
    })?;

    let mut names: Vec<String> = Vec::new();
    let mut count = 0_usize;
    let mut folder_count = 0_usize;
    let mut has_config = false;
    for entry in entries {
        let entry = entry.map_err(|e| FsError::Io {
            path: vault_dir.to_path_buf(),
            source: e,
        })?;
        count += 1;
        let name = entry.file_name().to_string_lossy().to_string();
        if name == CONFIG_FILENAME || name == LEGACY_CONFIG_FILENAME {
            has_config = true;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            folder_count += 1;
        }
        if names.len() < 3 {
            names.push(name);
        }
    }

    if count == 0 {
        return Ok(DirState::Empty);
    }
    if has_config {
        return Ok(DirState::OneBrainVault);
    }
    let file_count = count - folder_count;
    let summary = format!(
        "{file_count} file(s), {folder_count} folder(s) (e.g., {})",
        names.join(", ")
    );
    Ok(DirState::NonEmptyNonVault { summary })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_dir_returns_missing() {
        let d = tempdir().unwrap();
        let target = d.path().join("does-not-exist");
        assert_eq!(classify(&target).unwrap(), DirState::Missing);
    }

    #[test]
    fn empty_dir_returns_empty() {
        let d = tempdir().unwrap();
        assert_eq!(classify(d.path()).unwrap(), DirState::Empty);
    }

    #[test]
    fn dir_with_onebrain_yml_returns_onebrain_vault() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("onebrain.yml"), "ok").unwrap();
        assert_eq!(classify(d.path()).unwrap(), DirState::OneBrainVault);
    }

    #[test]
    fn dir_with_legacy_vault_yml_returns_onebrain_vault() {
        // Back-compat: legacy `vault.yml` still marks the dir as a OneBrain
        // vault so init delegates to the overwrite guard (and doctor
        // migrates the file).
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("vault.yml"), "ok").unwrap();
        assert_eq!(classify(d.path()).unwrap(), DirState::OneBrainVault);
    }

    #[test]
    fn dir_with_files_no_vault_yml_returns_nonempty() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("README.md"), "hi").unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        match classify(d.path()).unwrap() {
            DirState::NonEmptyNonVault { summary } => {
                assert!(
                    summary.contains("1 file(s)") && summary.contains("1 folder(s)"),
                    "unexpected summary: {summary}"
                );
                // At least one of the entries should appear
                assert!(summary.contains("README.md") || summary.contains("src"));
            }
            other => panic!("unexpected state: {other:?}"),
        }
    }

    #[test]
    fn hidden_files_are_not_ignored() {
        // .DS_Store / .git / similar hidden files still count as "not empty"
        // — the safety check should not be clever about which dotfiles are
        // safe to overlay onto.
        let d = tempdir().unwrap();
        std::fs::write(d.path().join(".DS_Store"), "").unwrap();
        match classify(d.path()).unwrap() {
            DirState::NonEmptyNonVault { .. } => {}
            other => panic!("hidden file not counted as non-empty: {other:?}"),
        }
    }

    #[test]
    fn regular_file_at_target_returns_nonempty_with_message() {
        let d = tempdir().unwrap();
        let target = d.path().join("regular-file");
        std::fs::write(&target, "ok").unwrap();
        match classify(&target).unwrap() {
            DirState::NonEmptyNonVault { summary } => {
                assert!(summary.contains("not a directory"), "got: {summary}");
            }
            other => panic!("unexpected state: {other:?}"),
        }
    }

    /// `symlink_metadata` returning a non-NotFound error (e.g. EACCES when the
    /// parent directory has no x-bit) must propagate as `Err`, never as
    /// `Ok(DirState::Missing)`.
    #[cfg(unix)]
    #[test]
    fn stat_error_non_notfound_propagates() {
        extern "C" {
            fn geteuid() -> u32;
        }
        if unsafe { geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let restricted = d.path().join("restricted");
        std::fs::create_dir_all(&restricted).unwrap();
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o000)).unwrap();
        // stat("restricted/vault") → EACCES (not ENOENT).
        let target = restricted.join("vault");
        let result = classify(&target);
        // Restore before asserting so tempdir cleanup succeeds.
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            result.is_err(),
            "EACCES must propagate as Err, not become DirState::Missing; got {result:?}"
        );
    }

    /// `read_dir` failing (e.g. EACCES when the directory has no r-bit) must
    /// propagate as `Err`. `symlink_metadata` (lstat) still succeeds because it
    /// only needs x-bit on the *parent* directory, not the target itself.
    #[cfg(unix)]
    #[test]
    fn read_dir_error_propagates() {
        extern "C" {
            fn geteuid() -> u32;
        }
        if unsafe { geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        // chmod 000: lstat(dir) succeeds (needs x on parent only),
        // read_dir(dir) fails (needs r on the dir itself).
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = classify(d.path());
        // Restore before asserting so tempdir cleanup succeeds.
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            result.is_err(),
            "read_dir EACCES must propagate as Err; got {result:?}"
        );
    }

    /// A directory with more than 3 entries: the summary's sample list is
    /// capped at 3 names, and the total count reflects all entries.
    #[test]
    fn more_than_three_entries_sample_is_capped() {
        let d = tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(d.path().join(format!("file{i}.txt")), "").unwrap();
        }
        match classify(d.path()).unwrap() {
            DirState::NonEmptyNonVault { summary } => {
                // 5 files, 0 folders
                assert!(
                    summary.contains("5 file(s)"),
                    "unexpected summary: {summary}"
                );
                // Sample in "e.g., ..." contains at most 3 items (split by ", ")
                let sample_start = summary.find("e.g., ").expect("summary has e.g., section");
                let sample = &summary[sample_start + 6..]; // after "e.g., "
                let sample_names: Vec<&str> = sample.split(", ").collect();
                assert!(
                    sample_names.len() <= 3,
                    "sample must be capped at 3, got {}: {summary}",
                    sample_names.len()
                );
            }
            other => panic!("unexpected state: {other:?}"),
        }
    }
}
