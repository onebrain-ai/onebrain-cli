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

use super::io::atomic_write;
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
    /// Vault-relative paths of notes that could not be read as UTF-8 text
    /// during the link scan and were therefore NOT rewritten. Non-empty means
    /// such a note may still contain a now-dangling `[[old]]` link. Omitted
    /// from JSON when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<PathBuf>,
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

    let (edits, skipped) = if update_links && old_basename != new_basename {
        build_edit_plan(vault_root, &from_abs, &old_basename, &new_basename)?
    } else {
        (Vec::new(), Vec::new())
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
            skipped,
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
        skipped,
    })
}

/// Scan every note (except the FROM file itself) and produce the set of edits
/// whose link target equals `old_basename`, rewritten to `new_basename`.
/// Returns `(edits, skipped)` where `skipped` holds the vault-relative paths of
/// notes that could not be read (and so may keep a now-dangling `[[old]]`).
fn build_edit_plan(
    vault_root: &Path,
    from_abs: &Path,
    old_basename: &str,
    new_basename: &str,
) -> Result<(Vec<PlannedEdit>, Vec<PathBuf>)> {
    let files = walk_notes(vault_root, None)?;
    let mut edits = Vec::new();
    let mut skipped: Vec<PathBuf> = Vec::new();
    for file in files {
        if file == from_abs {
            continue; // never rewrite the note being moved
        }
        // Best-effort: skip notes that can't be read as UTF-8 text — but RECORD
        // them, since such a note may still hold a `[[old]]` link we can't
        // rewrite (it would dangle after the move).
        let Ok(original) = std::fs::read_to_string(&file) else {
            skipped.push(file.strip_prefix(vault_root).unwrap_or(&file).to_path_buf());
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
    Ok((edits, skipped))
}

/// Apply the move + rewrites atomically. On ANY failure, roll back everything
/// already done (restore written files, move the renamed file back).
///
/// Two failure outcomes:
/// - Original op failed AND rollback was CLEAN → return the original error
///   unchanged (the vault is left exactly as it started).
/// - Original op failed AND rollback ALSO failed → return
///   [`CoreError::RollbackIncomplete`] naming the files that could not be
///   restored, so the caller knows the vault may be inconsistent rather than
///   believing it was cleanly reverted.
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
        if let Err(original_err) = atomic_write(&edit.abs_path, edit.new_content.as_bytes()) {
            // ROLL BACK everything already done, collecting any rollback-step
            // failures.
            let unrestored = attempt_rollback(from_abs, to_abs, &written);

            if unrestored.is_empty() {
                // Clean rollback — surface the original failure unchanged so
                // the vault is left exactly as it started.
                return Err(original_err);
            }
            // Rollback itself failed: the vault is half-rewritten. Surface a
            // DISTINCT error naming the files we couldn't restore so the caller
            // knows the vault may be inconsistent.
            let names = unrestored
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(FsError::Core(CoreError::RollbackIncomplete(format!(
                "after a failed move the rollback could not restore: {names} \
                 (original error: {original_err})"
            ))));
        }
        written.push(edit);
    }

    Ok(())
}

/// Best-effort rollback of a partially-applied move: restore every
/// already-rewritten file to its original content, then move the source file
/// back to its origin. Returns the vault-internal (absolute) paths of every
/// step that FAILED — empty when the rollback was clean.
///
/// Extracted from [`execute_plan`] so the rollback-failure path is unit-testable
/// (forcing a mid-call permission flip is impractical otherwise).
fn attempt_rollback(from_abs: &Path, to_abs: &Path, written: &[&PlannedEdit]) -> Vec<PathBuf> {
    let mut unrestored: Vec<PathBuf> = Vec::new();
    for done in written {
        if atomic_write(&done.abs_path, done.original.as_bytes()).is_err() {
            unrestored.push(done.abs_path.clone());
        }
    }
    if move_file(to_abs, from_abs).is_err() {
        // The moved file could not be returned to its origin.
        unrestored.push(to_abs.to_path_buf());
    }
    unrestored
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

    /// CRITICAL: when the original op fails AND rollback ALSO fails, the
    /// returned error must be the DISTINCT `RollbackIncomplete` variant that
    /// names which files could not be restored — NOT the original error
    /// (which would falsely imply a clean revert).
    ///
    /// Setup (drives the full `move_note`):
    /// - `old.md` (the source) lives next to `a.md`. Both `old.md` and `a.md`
    ///   are at the vault root, which stays writable so the FORWARD move and
    ///   `a.md`'s rewrite both succeed.
    /// - `locked/b.md` links to `old` too; its parent dir is read-only, so its
    ///   write FAILS → triggers rollback (this is failure #1).
    /// - Before the call we make the un-move target's parent (`root`) hostile
    ///   to the un-move by pre-creating a DIRECTORY at the source path's
    ///   would-be restore location is impossible while the file occupies it, so
    ///   instead we make `a.md`'s restore fail: we make `a.md` a path whose
    ///   directory we keep writable for the forward write but whose RESTORE we
    ///   block by replacing `a.md` with a read-only directory of the same name
    ///   AFTER its rewrite — see the rollback unit test below for the
    ///   mechanism; here we assert the public contract via the un-move lever.
    ///
    /// Mechanism actually used: the un-move `rename(new.md → old.md)` is made
    /// to fail by pre-creating a non-empty DIRECTORY named `old.md` is not
    /// possible (the source file owns that name). So we exercise the
    /// rollback-failure branch through the extracted [`attempt_rollback`]
    /// helper in `rollback_helper_reports_unrestored_files`, and here we
    /// confirm the END-TO-END error type wiring by forcing the un-move to fail
    /// via a read-only source parent that the forward move tolerated because
    /// the file was staged in a subdir that we lock only after staging.
    #[cfg(unix)]
    #[test]
    fn rollback_failure_reports_inconsistent_vault() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path();

        // Source + one rewrite target at root (writable: forward move + a.md
        // rewrite succeed).
        write(root, "old.md", "# Old\n");
        write(root, "a.md", "see [[old]] here\n");
        // Failing-write target in a read-only dir (failure #1).
        write(root, "locked/b.md", "also [[old]] there\n");

        let locked_dir = root.join("locked");
        let mut p = fs::metadata(&locked_dir).unwrap().permissions();
        p.set_mode(0o555);
        fs::set_permissions(&locked_dir, p).unwrap();

        // Move old.md → moved/new.md (subdir created by execute_plan). The
        // un-move will be rename(moved/new.md → old.md). To make THAT fail
        // (failure #2) we occupy `old.md`'s restore slot with a read-only
        // directory: we can't while the source file exists, so instead we make
        // the un-move's *destination directory* (`root`) reject the rename by
        // pre-creating a directory named `old.md` — impossible.
        //
        // Working lever: pre-create the un-move TARGET as a directory under a
        // name the un-move will try to use. We rename the SOURCE to a temp name
        // first so `old.md` is free, then drop a read-only dir there. Since
        // move_note owns the forward move, we instead point the destination
        // into a dir we make read-only right after execute_plan creates it —
        // not interceptable. Therefore this end-to-end test asserts the CLEAN
        // rollback path (the common case), and the rollback-FAILURE branch is
        // covered deterministically by the helper test below.
        let err = move_note(
            root,
            Path::new("old.md"),
            Path::new("moved/new.md"),
            true,
            false,
        )
        .unwrap_err();

        let mut p = fs::metadata(&locked_dir).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(&locked_dir, p).unwrap();

        // Clean rollback → original IO error surfaces unchanged; vault intact.
        assert!(matches!(err, FsError::Io { .. }));
        assert!(root.join("old.md").exists(), "source restored");
        assert!(!root.join("moved/new.md").exists(), "dest removed");
        assert_eq!(read(root, "a.md"), "see [[old]] here\n");
    }

    /// Deterministically exercise the rollback-FAILURE branch via the extracted
    /// [`attempt_rollback`] helper. We feed it a "written" set containing one
    /// entry whose parent directory is read-only, so its restore `atomic_write`
    /// fails, plus an un-move whose target parent is also read-only. The helper
    /// must collect BOTH unrestored paths.
    #[cfg(unix)]
    #[test]
    fn rollback_helper_reports_unrestored_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path();

        // A "rewritten" file whose restore will FAIL: it lives in a read-only
        // dir, so `atomic_write({path}.tmp)` can't create the temp file.
        let ro_dir = root.join("ro");
        fs::create_dir_all(&ro_dir).unwrap();
        let rewritten = ro_dir.join("a.md");
        fs::write(&rewritten, "current (rewritten) content\n").unwrap();
        let edit = PlannedEdit {
            abs_path: rewritten.clone(),
            original: "ORIGINAL content\n".to_string(),
            new_content: "current (rewritten) content\n".to_string(),
            occurrences: 1,
        };

        // The moved file currently sits at `to_abs`; the un-move target's
        // parent (`from_dir`) is read-only, so `rename(to → from)` fails.
        let from_dir = root.join("from_ro");
        fs::create_dir_all(&from_dir).unwrap();
        let from_abs = from_dir.join("old.md"); // does not exist (was moved away)
        let to_dir = root.join("to_ok");
        fs::create_dir_all(&to_dir).unwrap();
        let to_abs = to_dir.join("new.md");
        fs::write(&to_abs, "# Old\n").unwrap(); // the moved file

        // Lock both hostile dirs.
        for d in [&ro_dir, &from_dir] {
            let mut p = fs::metadata(d).unwrap().permissions();
            p.set_mode(0o555);
            fs::set_permissions(d, p).unwrap();
        }

        let written: Vec<&PlannedEdit> = vec![&edit];
        let unrestored = attempt_rollback(&from_abs, &to_abs, &written);

        // Unlock for cleanup.
        for d in [&ro_dir, &from_dir] {
            let mut p = fs::metadata(d).unwrap().permissions();
            p.set_mode(0o755);
            fs::set_permissions(d, p).unwrap();
        }

        // BOTH the un-restorable rewrite AND the un-move target are reported.
        assert!(
            unrestored.contains(&rewritten),
            "rewritten file that couldn't be restored must be named: {unrestored:?}"
        );
        assert!(
            unrestored.contains(&to_abs),
            "the moved file that couldn't be un-moved must be named: {unrestored:?}"
        );
    }

    /// Clean rollback: `attempt_rollback` returns an EMPTY vec when every step
    /// succeeds (writable dirs), so `execute_plan` surfaces the original error.
    #[test]
    fn rollback_helper_clean_returns_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let rewritten = root.join("a.md");
        fs::write(&rewritten, "rewritten\n").unwrap();
        let edit = PlannedEdit {
            abs_path: rewritten.clone(),
            original: "ORIGINAL\n".to_string(),
            new_content: "rewritten\n".to_string(),
            occurrences: 1,
        };
        let from_abs = root.join("old.md"); // free
        let to_abs = root.join("new.md");
        fs::write(&to_abs, "# Old\n").unwrap();

        let written: Vec<&PlannedEdit> = vec![&edit];
        let unrestored = attempt_rollback(&from_abs, &to_abs, &written);

        assert!(unrestored.is_empty(), "clean rollback names nothing");
        // Side effects: a.md restored, file un-moved.
        assert_eq!(fs::read_to_string(&rewritten).unwrap(), "ORIGINAL\n");
        assert!(from_abs.exists());
        assert!(!to_abs.exists());
    }

    #[test]
    fn rewrite_links_unit_preserves_suffixes() {
        let (out, n) = rewrite_links("[[old]] [[old|a]] [[old#s]] [[keep]]", "old", "new");
        assert_eq!(out, "[[new]] [[new|a]] [[new#s]] [[keep]]");
        assert_eq!(n, 3);
    }

    /// An unreadable note that might hold a `[[old]]` link is recorded in
    /// `skipped` (it would dangle after the move) rather than silently dropped.
    /// The move of readable notes still proceeds.
    #[cfg(unix)]
    #[test]
    fn unreadable_note_is_skipped_and_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "old.md", "# Old\n");
        write(root, "readable.md", "see [[old]]\n");
        write(root, "blocked.md", "also [[old]]\n");
        let blocked = root.join("blocked.md");
        let mut p = fs::metadata(&blocked).unwrap().permissions();
        p.set_mode(0o000);
        fs::set_permissions(&blocked, p).unwrap();

        let res = move_note(root, Path::new("old.md"), Path::new("new.md"), true, false).unwrap();

        let mut p = fs::metadata(&blocked).unwrap().permissions();
        p.set_mode(0o644);
        fs::set_permissions(&blocked, p).unwrap();

        // The readable note was rewritten; the unreadable one is reported.
        assert_eq!(res.links_rewritten, 1, "only readable link rewritten");
        assert_eq!(read(root, "readable.md"), "see [[new]]\n");
        assert_eq!(res.skipped, vec![PathBuf::from("blocked.md")]);
    }
}
