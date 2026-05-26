//! `note append` — append text to an existing vault note.
//!
//! First WRITE verb of the `note` group (v3.2.0). Two modes:
//!
//! - No `--section`: the text lands at the END of the file.
//! - `--section H`: the text lands at the end of the section under heading `H`
//!   (immediately before the next heading of the same-or-higher level, or at
//!   EOF if `H` is the file's last section). If `H` doesn't exist, it is
//!   CREATED at the end of the file as a level-2 ATX heading (`## H`) and the
//!   content is appended under it.
//!
//! Writes go through a temp file + atomic `rename`, matching the tmp+rename
//! pattern used by `init::marketplace` and `register_hooks::settings`.

use super::read::heading_parts;
use crate::error::{FsError, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Result of [`append_note`]. `path` is vault-relative.
#[derive(Debug, Serialize, PartialEq)]
pub struct AppendResult {
    /// Vault-relative path of the note that was appended to.
    pub path: PathBuf,
    /// The section heading the content landed under, if `--section` was used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Byte length of the content as appended (the verbatim caller string,
    /// before trailing-newline normalisation).
    pub bytes_appended: usize,
    /// `true` if the section heading did not exist and was created.
    pub created_section: bool,
}

/// Append `content` to the note at `rel_path` (relative to `vault_root`).
///
/// - No `section`: append at EOF.
/// - `section = Some(h)`: append at the end of the section under heading `h`
///   (before the next same-or-higher-level heading, or at EOF if last). If `h`
///   is absent, create `## h` at EOF and append the content beneath it
///   (`created_section = true`).
///
/// Trailing-newline discipline: the result is separated from the preceding
/// content by exactly one newline, the appended content ends with a newline,
/// and no spurious blank lines are introduced. Existing content is otherwise
/// preserved byte-for-byte.
///
/// A missing target file surfaces as [`FsError::Io`] (ENOENT) — append targets
/// an existing note (use `note new` to create one).
pub fn append_note(
    vault_root: &Path,
    rel_path: &Path,
    content: &str,
    section: Option<&str>,
) -> Result<AppendResult> {
    let abs = vault_root.join(rel_path);
    let raw = std::fs::read_to_string(&abs).map_err(|source| FsError::Io {
        path: abs.clone(),
        source,
    })?;

    let (new_content, created_section) = match section {
        Some(heading) => insert_under_section(&raw, heading, content),
        None => (append_at_eof(&raw, content), false),
    };

    atomic_write(&abs, new_content.as_bytes())?;

    Ok(AppendResult {
        path: rel_path.to_path_buf(),
        section: section.map(str::to_string),
        bytes_appended: content.len(),
        created_section,
    })
}

/// Normalise `block` so it ends with exactly one trailing newline.
fn ensure_trailing_newline(block: &str) -> String {
    let trimmed = block.trim_end_matches('\n');
    format!("{trimmed}\n")
}

/// Append `content` to `raw` at EOF, separated by exactly one newline and
/// terminated by exactly one newline. An empty file yields just the
/// newline-terminated content.
fn append_at_eof(raw: &str, content: &str) -> String {
    let block = ensure_trailing_newline(content);
    if raw.is_empty() {
        return block;
    }
    // Exactly one newline between existing content and the appended block.
    let base = raw.trim_end_matches('\n');
    if base.is_empty() {
        // File was only newlines — collapse to the appended block.
        return block;
    }
    format!("{base}\n{block}")
}

/// Insert `content` at the end of the section under `heading`. If the heading
/// is absent, create it (`## heading`) at EOF. Returns `(new_content,
/// created_section)`.
fn insert_under_section(raw: &str, heading: &str, content: &str) -> (String, bool) {
    let target = heading.trim();
    let lines: Vec<&str> = raw.lines().collect();

    // Locate the heading line + its level.
    let mut start = None;
    let mut level = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if let Some((lvl, title)) = heading_parts(line) {
            if title == target {
                start = Some(i);
                level = lvl;
                break;
            }
        }
    }

    let Some(start) = start else {
        // Heading absent → create `## heading` at EOF, content beneath it.
        let block = ensure_trailing_newline(content);
        let section_block = format!("## {target}\n{block}");
        let combined = if raw.trim_end_matches('\n').is_empty() {
            section_block
        } else {
            format!("{}\n{}", raw.trim_end_matches('\n'), section_block)
        };
        return (combined, true);
    };

    // Find the end of the section: first following heading of same-or-higher
    // level, else EOF.
    let mut end = lines.len();
    for (offset, line) in lines[start + 1..].iter().enumerate() {
        if let Some((lvl, _)) = heading_parts(line) {
            if lvl <= level {
                end = start + 1 + offset;
                break;
            }
        }
    }

    // Section body = lines[start..end]. Trim trailing blank lines off the
    // body so we don't accumulate blank lines, then re-join everything.
    let before = &lines[..end];
    let after = &lines[end..];

    let block = ensure_trailing_newline(content);

    // Reconstruct: section-and-everything-before, one newline, the appended
    // block, then the remainder (next heading onwards) preserved verbatim.
    // Drop trailing blank lines inside the section so the appended block sits
    // directly after the last non-blank line.
    let joined = before.join("\n");
    let head = joined.trim_end_matches('\n');

    let mut out = if head.is_empty() {
        block.clone()
    } else {
        format!("{head}\n{block}")
    };

    if !after.is_empty() {
        // Preserve the rest of the file (the next heading onward) exactly,
        // separated by one newline. `lines()` dropped the trailing newline of
        // the original file; restore a final newline below.
        let rest = after.join("\n");
        out.push_str(&rest);
        // Preserve original EOF newline behaviour: files conventionally end
        // with a newline.
        if raw.ends_with('\n') && !out.ends_with('\n') {
            out.push('\n');
        }
    }

    (out, false)
}

/// Atomic write via `{path}.tmp` + `rename`. Matches the tmp+rename pattern in
/// `init::marketplace::atomic_write_canonical`. Maps IO failures to
/// [`FsError::Io`].
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
    fn append_at_eof_no_section() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "line1\nline2\n");

        let res = append_note(root, Path::new("a.md"), "appended line", None).unwrap();
        assert_eq!(res.path, PathBuf::from("a.md"));
        assert_eq!(res.section, None);
        assert!(!res.created_section);
        assert_eq!(res.bytes_appended, "appended line".len());
        assert_eq!(read(root, "a.md"), "line1\nline2\nappended line\n");
    }

    #[test]
    fn append_under_existing_section_lands_before_next_heading() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "a.md",
            "# Title\nintro\n\n## Alpha\na1\na2\n\n## Beta\nb1\n",
        );

        let res = append_note(root, Path::new("a.md"), "a3", Some("Alpha")).unwrap();
        assert_eq!(res.section.as_deref(), Some("Alpha"));
        assert!(!res.created_section);
        // a3 lands at the end of the Alpha block, NOT at EOF.
        assert_eq!(
            read(root, "a.md"),
            "# Title\nintro\n\n## Alpha\na1\na2\na3\n## Beta\nb1\n"
        );
    }

    #[test]
    fn append_under_last_section_lands_at_eof() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "# Title\nintro\n\n## Beta\nb1\n");

        let res = append_note(root, Path::new("a.md"), "b2", Some("Beta")).unwrap();
        assert!(!res.created_section);
        assert_eq!(read(root, "a.md"), "# Title\nintro\n\n## Beta\nb1\nb2\n");
    }

    #[test]
    fn append_under_subsection_stops_at_higher_level_heading() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Alpha (level 2) contains a level-3 Sub; a later level-1 Top stops it.
        write(root, "a.md", "## Alpha\na1\n### Sub\ns1\n# Top\nt1\n");

        append_note(root, Path::new("a.md"), "added", Some("Alpha")).unwrap();
        // `added` must land after `s1` (still inside Alpha's scope), before `# Top`.
        assert_eq!(
            read(root, "a.md"),
            "## Alpha\na1\n### Sub\ns1\nadded\n# Top\nt1\n"
        );
    }

    #[test]
    fn create_section_when_absent() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "# Title\nbody\n");

        let res = append_note(root, Path::new("a.md"), "fresh content", Some("Gamma")).unwrap();
        assert!(res.created_section);
        assert_eq!(res.section.as_deref(), Some("Gamma"));
        // Heading created as level-2 ATX at EOF, content beneath it.
        assert_eq!(
            read(root, "a.md"),
            "# Title\nbody\n## Gamma\nfresh content\n"
        );
    }

    #[test]
    fn trailing_newline_no_doubled_blank_lines() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // File ends with a blank line (two newlines).
        write(root, "a.md", "line1\n\n");

        append_note(root, Path::new("a.md"), "next", None).unwrap();
        let out = read(root, "a.md");
        // Exactly one newline as separator, file ends with exactly one newline.
        assert_eq!(out, "line1\nnext\n");
        assert!(out.ends_with('\n'));
        assert!(!out.ends_with("\n\n"));
    }

    #[test]
    fn content_with_trailing_newline_not_doubled() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "line1\n");

        // Caller passes content that already ends with a newline.
        append_note(root, Path::new("a.md"), "block\n", None).unwrap();
        let out = read(root, "a.md");
        assert_eq!(out, "line1\nblock\n");
    }

    #[test]
    fn file_without_trailing_newline_gets_separator() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // No trailing newline on the existing file.
        write(root, "a.md", "line1");

        append_note(root, Path::new("a.md"), "line2", None).unwrap();
        assert_eq!(read(root, "a.md"), "line1\nline2\n");
    }

    #[test]
    fn multiline_content_appended_verbatim() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "head\n");

        let content = "- [ ] task one\n- [ ] task two";
        let res = append_note(root, Path::new("a.md"), content, None).unwrap();
        assert_eq!(res.bytes_appended, content.len());
        assert_eq!(read(root, "a.md"), "head\n- [ ] task one\n- [ ] task two\n");
    }

    #[test]
    fn existing_content_preserved_exactly() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let original = "# Title\n\nintro paragraph\n\n## Section\nbody line\n";
        write(root, "a.md", original);

        append_note(root, Path::new("a.md"), "tail", None).unwrap();
        let out = read(root, "a.md");
        // Everything before the append is byte-identical.
        assert!(out.starts_with(original));
        assert_eq!(
            out,
            "# Title\n\nintro paragraph\n\n## Section\nbody line\ntail\n"
        );
    }

    #[test]
    fn missing_file_is_io_error() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let err = append_note(root, Path::new("nope.md"), "x", None).unwrap_err();
        assert!(matches!(err, FsError::Io { .. }));
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.md", "x\n");

        append_note(root, Path::new("a.md"), "y", None).unwrap();
        assert!(!root.join("a.md.tmp").exists());
        assert!(root.join("a.md").exists());
    }
}
