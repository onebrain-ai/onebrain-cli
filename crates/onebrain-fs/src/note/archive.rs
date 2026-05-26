//! `note archive` — move a note into the dated archive bucket.
//!
//! Third WRITE verb of the `note` group (v3.2.0). Moves a note to
//! `<vault>/<archive_root>/YYYY/MM/<filename>` where `YYYY`/`MM` come from the
//! current UTC date and `<filename>` is the source basename — per the
//! CLAUDE.md archive convention (`06-archive/YYYY/MM/filename.md`).
//!
//! Wikilinks are preserved automatically: Obsidian resolves `[[basename]]`
//! regardless of folder and the basename never changes here, so no link
//! rewriting is needed. (Contrast `note move`, which DOES rewrite links.)

use crate::error::{FsError, Result};
use chrono::{Datelike, Utc};
use onebrain_core::CoreError;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Result of [`archive_note`]. Both paths are vault-relative.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ArchiveResult {
    /// Vault-relative path the note moved FROM.
    pub from: PathBuf,
    /// Vault-relative path the note moved TO (under the dated archive bucket).
    pub to: PathBuf,
}

/// Archive the note at `rel_path` into `<archive_root>/YYYY/MM/<filename>`,
/// using the current UTC date for the `YYYY`/`MM` bucket.
///
/// - Source must exist (else [`FsError::Io`] with ENOENT).
/// - The destination must not already exist (else [`CoreError::InvalidTarget`]) —
///   the existing file is never clobbered.
/// - Destination directories are created as needed.
pub fn archive_note(
    vault_root: &Path,
    rel_path: &Path,
    archive_root: &Path,
) -> Result<ArchiveResult> {
    let now = Utc::now();
    archive_note_at(vault_root, rel_path, archive_root, now.year(), now.month())
}

/// Date-injectable core of [`archive_note`], used by tests for deterministic
/// destination paths. `year`/`month` drive the `YYYY/MM` bucket.
fn archive_note_at(
    vault_root: &Path,
    rel_path: &Path,
    archive_root: &Path,
    year: i32,
    month: u32,
) -> Result<ArchiveResult> {
    let src_abs = vault_root.join(rel_path);

    // Source must exist — surface a real ENOENT so callers/tests can rely on
    // the standard not-found classification.
    if !src_abs.exists() {
        return Err(FsError::Io {
            path: src_abs.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("source note not found: {}", rel_path.display()),
            ),
        });
    }

    let filename = rel_path.file_name().ok_or_else(|| {
        FsError::Core(CoreError::InvalidTarget(format!(
            "source has no filename: {}",
            rel_path.display()
        )))
    })?;

    let rel_dest = archive_root
        .join(format!("{year:04}"))
        .join(format!("{month:02}"))
        .join(filename);
    let dest_abs = vault_root.join(&rel_dest);

    if dest_abs.exists() {
        return Err(FsError::Core(CoreError::InvalidTarget(format!(
            "archive destination exists: {}",
            rel_dest.display()
        ))));
    }

    if let Some(parent) = dest_abs.parent() {
        std::fs::create_dir_all(parent).map_err(|source| FsError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    // In-vault moves are same-device, so rename normally succeeds. Fall back to
    // copy + remove only if rename reports a cross-device link (EXDEV).
    if let Err(err) = std::fs::rename(&src_abs, &dest_abs) {
        if err.raw_os_error() == Some(libc_exdev()) {
            std::fs::copy(&src_abs, &dest_abs).map_err(|source| FsError::Io {
                path: dest_abs.clone(),
                source,
            })?;
            std::fs::remove_file(&src_abs).map_err(|source| FsError::Io {
                path: src_abs.clone(),
                source,
            })?;
        } else {
            return Err(FsError::Io {
                path: dest_abs.clone(),
                source: err,
            });
        }
    }

    Ok(ArchiveResult {
        from: rel_path.to_path_buf(),
        to: rel_dest,
    })
}

/// EXDEV errno ("cross-device link"). 18 on Linux and macOS/BSD.
fn libc_exdev() -> i32 {
    18
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn archives_to_dated_bucket() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "01-projects/Old Note.md", "# Old Note\nbody\n");

        let res = archive_note_at(
            root,
            Path::new("01-projects/Old Note.md"),
            Path::new("06-archive"),
            2026,
            5,
        )
        .unwrap();

        assert_eq!(res.from, PathBuf::from("01-projects/Old Note.md"));
        assert_eq!(res.to, PathBuf::from("06-archive/2026/05/Old Note.md"));
        // Destination has the content; it really moved.
        assert_eq!(
            fs::read_to_string(root.join("06-archive/2026/05/Old Note.md")).unwrap(),
            "# Old Note\nbody\n"
        );
    }

    #[test]
    fn source_removed_after_archive() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "note.md", "x\n");

        archive_note_at(root, Path::new("note.md"), Path::new("06-archive"), 2026, 1).unwrap();

        assert!(!root.join("note.md").exists(), "source should be gone");
    }

    #[test]
    fn dest_dirs_created() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "note.md", "x\n");
        // 06-archive does not exist yet.
        assert!(!root.join("06-archive").exists());

        archive_note_at(
            root,
            Path::new("note.md"),
            Path::new("06-archive"),
            2026,
            12,
        )
        .unwrap();

        assert!(root.join("06-archive/2026/12/note.md").exists());
    }

    #[test]
    fn destination_exists_errors() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "note.md", "new\n");
        // Pre-seed the archive destination.
        write(root, "06-archive/2026/03/note.md", "old\n");

        let err = archive_note_at(root, Path::new("note.md"), Path::new("06-archive"), 2026, 3)
            .unwrap_err();
        assert!(matches!(err, FsError::Core(CoreError::InvalidTarget(_))));
        // Neither file was touched.
        assert_eq!(fs::read_to_string(root.join("note.md")).unwrap(), "new\n");
        assert_eq!(
            fs::read_to_string(root.join("06-archive/2026/03/note.md")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn missing_source_errors() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let err = archive_note_at(root, Path::new("nope.md"), Path::new("06-archive"), 2026, 5)
            .unwrap_err();
        match err {
            FsError::Io { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected FsError::Io ENOENT, got {other:?}"),
        }
    }

    #[test]
    fn custom_archive_root_honored() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "note.md", "x\n");

        let res =
            archive_note_at(root, Path::new("note.md"), Path::new("attic/old"), 2025, 11).unwrap();

        assert_eq!(res.to, PathBuf::from("attic/old/2025/11/note.md"));
        assert!(root.join("attic/old/2025/11/note.md").exists());
    }

    #[test]
    fn public_fn_uses_current_utc_date() {
        // Smoke test of the public entry point: lands under archive_root with a
        // YYYY/MM bucket and the same basename.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "live.md", "x\n");

        let res = archive_note(root, Path::new("live.md"), Path::new("06-archive")).unwrap();

        let now = Utc::now();
        let expected = PathBuf::from(format!(
            "06-archive/{:04}/{:02}/live.md",
            now.year(),
            now.month()
        ));
        assert_eq!(res.to, expected);
        assert!(root.join(&expected).exists());
        assert!(!root.join("live.md").exists());
    }
}
