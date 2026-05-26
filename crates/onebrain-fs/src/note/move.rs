//! `note move` — move/rename a note AND rewrite every incoming wikilink.
//!
//! The MOST complex verb of the `note` group (v3.2.0). Moving a note changes
//! its basename (the link target Obsidian resolves), so every OTHER note that
//! links to the old basename must be rewritten to the new basename — preserving
//! any alias or section: `[[old]]`→`[[new]]`, `[[old|alias]]`→`[[new|alias]]`,
//! `[[old#sec]]`→`[[new#sec]]`.
//!
//! The operation is **transactional**. We build the full edit plan in memory
//! (the file move + every `(path, old_content, new_content)` rewrite) BEFORE
//! touching disk, capturing original contents so any mid-execution failure can
//! be rolled back to leave the vault exactly as it started.
//!
//! Writes go through a temp file + atomic `rename`, matching the tmp+rename
//! pattern used by [`super::append`] / [`super::new`].

use super::walker::walk_notes;
use crate::error::{FsError, Result};
use onebrain_core::CoreError;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Result of [`move_note`]. `from`/`to` and every entry of `updated_files` are
/// vault-relative.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MoveResult {
    /// Vault-relative path the note moved FROM.
    pub from: PathBuf,
    /// Vault-relative path the note moved TO.
    pub to: PathBuf,
    /// Total number of individual link occurrences that were (or would be)
    /// changed across all notes.
    pub links_rewritten: usize,
    /// Count of notes whose content changed (was/would-be rewritten).
    pub files_updated: usize,
    /// `true` when this was a `--dry-run`: nothing was written to disk.
    pub dry_run: bool,
    /// Vault-relative paths of the notes whose links were (or would be)
    /// rewritten. Sorted (inherits [`walk_notes`] ordering).
    pub updated_files: Vec<PathBuf>,
}

/// A single planned rewrite: an absolute note path, its original content (for
/// rollback), the new content to write, and how many link occurrences changed.
struct PlannedEdit {
    abs_path: PathBuf,
    original: String,
    new_content: String,
    occurrences: usize,
}

/// Move the note at `from` to `to` (both vault-relative), rewriting incoming
/// wikilinks unless `update_links` is false.
///
/// - Source must exist (else [`FsError::Io`] ENOENT).
/// - Destination must NOT already exist (else [`CoreError::InvalidTarget`]).
/// - Destination parent directories are created as needed.
/// - `dry_run = true`: compute the plan and return it WITHOUT touching disk.
/// - `update_links = false`: move the file only; skip the wikilink rewrite.
///
/// Execution is transactional: if any write fails partway, every already-applied
/// change (rewritten files + the file move) is rolled back so the vault is left
/// exactly as it started, and the original error is returned.
pub fn move_note(
    vault_root: &Path,
    from: &Path,
    to: &Path,
    update_links: bool,
    dry_run: bool,
) -> Result<MoveResult> {
    let from_abs = vault_root.join(from);
    let to_abs = vault_root.join(to);

    // 1. Validate.
    if !from_abs.exists() {
        return Err(FsError::Io {
            path: from_abs.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("source note not found: {}", from.display()),
            ),
        });
    }
    if to_abs.exists() {
        return Err(FsError::Core(CoreError::InvalidTarget(format!(
            "destination exists: {}",
            to.display()
        ))));
    }

    // 2. Build the link-rewrite plan (empty when --no-link-update).
    let old_basename = basename_no_ext(from);
    let new_basename = basename_no_ext(to);

    let edits = if update_links && old_basename != new_basename {
        build_edit_plan(vault_root, &from_abs, &old_basename, &new_basename)?
    } else {
        Vec::new()
    };

    let links_rewritten: usize = edits.iter().map(|e| e.occurrences).sum();
    let updated_files: Vec<PathBuf> = edits
        .iter()
        .map(|e| {
            e.abs_path
                .strip_prefix(vault_root)
                .unwrap_or(&e.abs_path)
                .to_path_buf()
        })
        .collect();

    // 3. Dry-run: report the plan, change nothing.
    if dry_run {
        return Ok(MoveResult {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            links_rewritten,
            files_updated: edits.len(),
            dry_run: true,
            updated_files,
        });
    }

    // 4. Execute transactionally.
    execute_plan(&from_abs, &to_abs, &edits)?;

    Ok(MoveResult {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        links_rewritten,
        files_updated: edits.len(),
        dry_run: false,
        updated_files,
    })
}

/// Scan every note (except the FROM file itself) and produce the set of edits
/// whose link target equals `old_basename`, rewritten to `new_basename`.
fn build_edit_plan(
    vault_root: &Path,
    from_abs: &Path,
    old_basename: &str,
    new_basename: &str,
) -> Result<Vec<PlannedEdit>> {
    let files = walk_notes(vault_root, None)?;
    let mut edits = Vec::new();
    for file in files {
        if file == from_abs {
            continue; // never rewrite the note being moved
        }
        // Best-effort: skip notes that can't be read as UTF-8 text.
        let Ok(original) = std::fs::read_to_string(&file) else {
            continue;
        };
        let (new_content, occurrences) = rewrite_links(&original, old_basename, new_basename);
        if occurrences > 0 {
            edits.push(PlannedEdit {
                abs_path: file,
                original,
                new_content,
                occurrences,
            });
        }
    }
    Ok(edits)
}

/// Apply the move + rewrites atomically. On ANY failure, roll back everything
/// already done (restore written files, move the renamed file back) and return
/// the original error.
fn execute_plan(from_abs: &Path, to_abs: &Path, edits: &[PlannedEdit]) -> Result<()> {
    // Create destination parent dirs.
    if let Some(parent) = to_abs.parent() {
        if let Err(source) = std::fs::create_dir_all(parent) {
            return Err(FsError::Io {
                path: parent.to_path_buf(),
                source,
            });
        }
    }

    // Step 1: move the file. (No rewrites applied yet, so no rollback needed
    // if this fails.)
    move_file(from_abs, to_abs)?;

    // Step 2: write each planned rewrite, tracking what's been done so we can
    // undo on failure.
    let mut written: Vec<&PlannedEdit> = Vec::with_capacity(edits.len());
    for edit in edits {
        if let Err(e) = atomic_write(&edit.abs_path, edit.new_content.as_bytes()) {
            // ROLL BACK: restore every file already rewritten, then move the
            // source file back to its original location.
            for done in &written {
                // Best-effort restore; the original error is what we surface.
                let _ = atomic_write(&done.abs_path, done.original.as_bytes());
            }
            let _ = move_file(to_abs, from_abs);
            return Err(e);
        }
        written.push(edit);
    }

    Ok(())
}

/// Move `src` → `dst`. In-vault moves are same-device so `rename` succeeds;
/// fall back to copy + remove on a cross-device link (EXDEV). Mirrors
/// [`super::archive`]'s strategy.
fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if let Err(err) = std::fs::rename(src, dst) {
        if err.raw_os_error() == Some(libc_exdev()) {
            std::fs::copy(src, dst).map_err(|source| FsError::Io {
                path: dst.to_path_buf(),
                source,
            })?;
            std::fs::remove_file(src).map_err(|source| FsError::Io {
                path: src.to_path_buf(),
                source,
            })?;
        } else {
            return Err(FsError::Io {
                path: dst.to_path_buf(),
                source: err,
            });
        }
    }
    Ok(())
}

/// EXDEV errno ("cross-device link"). 18 on Linux and macOS/BSD.
fn libc_exdev() -> i32 {
    18
}

/// File stem (basename without extension) as an owned `String`.
fn basename_no_ext(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Rewrite every wikilink in `content` whose link target equals `old_basename`
/// so its target becomes `new_basename`, preserving any `|alias` or `#section`
/// suffix. Returns `(new_content, occurrences_changed)`.
fn rewrite_links(content: &str, old_basename: &str, new_basename: &str) -> (String, usize) {
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut occurrences = 0usize;
    let mut i = 0;
    while i < content.len() {
        if i + 1 < bytes.len() && bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let inner_start = i + 2;
            if let Some(rel_end) = content[inner_start..].find("]]") {
                let inner = &content[inner_start..inner_start + rel_end];
                // Split target from the first `|` or `#` suffix, preserving it.
                let split_at = inner.find(['|', '#']);
                let (target_raw, suffix) = match split_at {
                    Some(pos) => (&inner[..pos], &inner[pos..]),
                    None => (inner, ""),
                };
                if target_raw.trim() == old_basename {
                    out.push_str("[[");
                    out.push_str(new_basename);
                    out.push_str(suffix);
                    out.push_str("]]");
                    occurrences += 1;
                    i = inner_start + rel_end + 2;
                    continue;
                }
                // Not a match: copy this whole wikilink verbatim and advance.
                out.push_str(&content[i..inner_start + rel_end + 2]);
                i = inner_start + rel_end + 2;
                continue;
            }
        }
        // Copy one byte-aligned char.
        let ch = content[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, occurrences)
}

/// Atomic write via `{path}.tmp` + `rename`. Matches [`super::new`]'s pattern.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.to_path_buf();
    let new_ext = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    };
    tmp.set_extension(new_ext);
    std::fs::write(&tmp, bytes).map_err(|source| FsError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| FsError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
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

    fn read(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join(rel)).unwrap()
    }

    #[test]
    fn move_rewrites_plain_alias_and_section_links() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "old.md", "# Old\n");
        write(root, "a.md", "see [[old]] for more\n");
        write(root, "b.md", "jump [[old|the old note]]\n");
        write(root, "c.md", "deep [[old#Section]] here\n");
        // d.md links elsewhere — must be untouched.
        write(root, "d.md", "unrelated [[other]]\n");

        let res = move_note(
            root,
            Path::new("old.md"),
            Path::new("03-knowledge/new.md"),
            true,
            false,
        )
        .unwrap();

        assert_eq!(res.from, PathBuf::from("old.md"));
        assert_eq!(res.to, PathBuf::from("03-knowledge/new.md"));
        assert_eq!(res.links_rewritten, 3);
        assert_eq!(res.files_updated, 3);
        assert!(!res.dry_run);
        assert_eq!(
            res.updated_files,
            vec![
                PathBuf::from("a.md"),
                PathBuf::from("b.md"),
                PathBuf::from("c.md"),
            ]
        );

        // File physically moved.
        assert!(!root.join("old.md").exists());
        assert_eq!(read(root, "03-knowledge/new.md"), "# Old\n");

        // Links rewritten, suffixes preserved.
        assert_eq!(read(root, "a.md"), "see [[new]] for more\n");
        assert_eq!(read(root, "b.md"), "jump [[new|the old note]]\n");
        assert_eq!(read(root, "c.md"), "deep [[new#Section]] here\n");
        // Unrelated link untouched.
        assert_eq!(read(root, "d.md"), "unrelated [[other]]\n");
    }

    #[test]
    fn multiple_occurrences_on_one_line_all_counted() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "old.md", "# Old\n");
        write(root, "a.md", "[[old]] and again [[old|x]] done\n");

        let res = move_note(root, Path::new("old.md"), Path::new("new.md"), true, false).unwrap();

        assert_eq!(res.links_rewritten, 2);
        assert_eq!(res.files_updated, 1);
        assert_eq!(read(root, "a.md"), "[[new]] and again [[new|x]] done\n");
    }

    #[test]
    fn no_link_update_moves_file_only() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "old.md", "# Old\n");
        write(root, "a.md", "see [[old]] here\n");

        let res = move_note(
            root,
            Path::new("old.md"),
            Path::new("new.md"),
            false, // --no-link-update
            false,
        )
        .unwrap();

        assert_eq!(res.links_rewritten, 0);
        assert_eq!(res.files_updated, 0);
        assert!(res.updated_files.is_empty());

        // File moved …
        assert!(!root.join("old.md").exists());
        assert!(root.join("new.md").exists());
        // … but the link is NOT rewritten.
        assert_eq!(read(root, "a.md"), "see [[old]] here\n");
    }

    #[test]
    fn dry_run_changes_nothing_but_reports_plan() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "old.md", "# Old\n");
        write(root, "a.md", "see [[old]] and [[old|y]]\n");
        write(root, "b.md", "ref [[old#sec]]\n");

        let res = move_note(
            root,
            Path::new("old.md"),
            Path::new("new.md"),
            true,
            true, // --dry-run
        )
        .unwrap();

        assert!(res.dry_run);
        assert_eq!(res.links_rewritten, 3);
        assert_eq!(res.files_updated, 2);
        assert_eq!(
            res.updated_files,
            vec![PathBuf::from("a.md"), PathBuf::from("b.md")]
        );

        // NOTHING changed on disk.
        assert!(root.join("old.md").exists());
        assert!(!root.join("new.md").exists());
        assert_eq!(read(root, "a.md"), "see [[old]] and [[old|y]]\n");
        assert_eq!(read(root, "b.md"), "ref [[old#sec]]\n");
    }

    #[test]
    fn destination_exists_errors() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "old.md", "src\n");
        write(root, "new.md", "existing\n");

        let err =
            move_note(root, Path::new("old.md"), Path::new("new.md"), true, false).unwrap_err();
        assert!(matches!(err, FsError::Core(CoreError::InvalidTarget(_))));
        // Both files untouched.
        assert_eq!(read(root, "old.md"), "src\n");
        assert_eq!(read(root, "new.md"), "existing\n");
    }

    #[test]
    fn missing_source_errors() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let err =
            move_note(root, Path::new("nope.md"), Path::new("new.md"), true, false).unwrap_err();
        match err {
            FsError::Io { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected FsError::Io ENOENT, got {other:?}"),
        }
    }

    #[test]
    fn dest_parent_dirs_created() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "old.md", "x\n");
        // Deep destination parent doesn't exist yet.
        assert!(!root.join("01-projects/foo/bar").exists());

        move_note(
            root,
            Path::new("old.md"),
            Path::new("01-projects/foo/bar/new.md"),
            true,
            false,
        )
        .unwrap();

        assert!(root.join("01-projects/foo/bar/new.md").exists());
    }

    #[test]
    fn same_basename_rename_skips_link_rewrite() {
        // Moving to a different folder but KEEPING the basename: Obsidian
        // resolves by basename, so no links need rewriting.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "note.md", "# Note\n");
        write(root, "a.md", "see [[note]]\n");

        let res = move_note(
            root,
            Path::new("note.md"),
            Path::new("03-knowledge/note.md"),
            true,
            false,
        )
        .unwrap();

        assert_eq!(res.links_rewritten, 0);
        assert_eq!(res.files_updated, 0);
        assert!(root.join("03-knowledge/note.md").exists());
        assert_eq!(read(root, "a.md"), "see [[note]]\n");
    }

    #[test]
    fn no_tmp_files_left_behind() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "old.md", "# Old\n");
        write(root, "a.md", "see [[old]]\n");

        move_note(root, Path::new("old.md"), Path::new("new.md"), true, false).unwrap();

        assert!(!root.join("a.md.tmp").exists());
        assert!(!root.join("new.md.tmp").exists());
    }

    /// CRITICAL rollback test: force a write failure mid-rewrite and assert the
    /// vault is restored to its exact starting state.
    ///
    /// We make one target note's PARENT DIRECTORY read-only so the atomic
    /// `{path}.tmp` write into it fails. Two notes link to `old`; the executor
    /// processes them in sorted order, so `a.md` is rewritten first, then the
    /// write into the locked `locked/b.md` fails — triggering rollback of
    /// `a.md` AND the file move.
    #[cfg(unix)]
    #[test]
    fn write_failure_mid_rewrite_rolls_back_everything() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "old.md", "# Old\n");
        write(root, "a.md", "see [[old]] here\n");
        write(root, "locked/b.md", "also [[old]] there\n");

        let a_before = read(root, "a.md");
        let b_before = read(root, "locked/b.md");

        // Make the `locked/` dir read-only so writing `locked/b.md.tmp` fails.
        let locked_dir = root.join("locked");
        let mut perms = fs::metadata(&locked_dir).unwrap().permissions();
        perms.set_mode(0o555); // r-x r-x r-x — no write
        fs::set_permissions(&locked_dir, perms).unwrap();

        let err =
            move_note(root, Path::new("old.md"), Path::new("new.md"), true, false).unwrap_err();

        // Restore writability so the tempdir can clean up + we can read files.
        let mut perms = fs::metadata(&locked_dir).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&locked_dir, perms).unwrap();

        // The error propagated …
        assert!(matches!(err, FsError::Io { .. }));

        // … the source file is back at its ORIGINAL path (move undone) …
        assert!(root.join("old.md").exists(), "source must be restored");
        assert!(!root.join("new.md").exists(), "dest must not remain");
        assert_eq!(read(root, "old.md"), "# Old\n");

        // … and every other note is restored to its original content.
        assert_eq!(read(root, "a.md"), a_before);
        assert_eq!(read(root, "locked/b.md"), b_before);
    }

    #[test]
    fn rewrite_links_unit_preserves_suffixes() {
        let (out, n) = rewrite_links("[[old]] [[old|a]] [[old#s]] [[keep]]", "old", "new");
        assert_eq!(out, "[[new]] [[new|a]] [[new#s]] [[keep]]");
        assert_eq!(n, 3);
    }
}
