//! `note new` — create a new vault note.
//!
//! Second WRITE verb of the `note` group (v3.2.0). Three content sources, in
//! priority order:
//!
//! - `--template <NAME>`: load `<vault>/.claude/plugins/onebrain/templates/<NAME>.md`
//!   and substitute placeholders `{{date}}` (today UTC `YYYY-MM-DD`), `{{title}}`
//!   (the path's file stem), and `{{slug}}` (kebab-case of the title). A missing
//!   template surfaces as [`CoreError::InvalidTarget`].
//! - `--frontmatter <K=V>` pairs: inject/override keys. With a template, the
//!   pairs merge into the template's leading frontmatter block (creating one if
//!   absent). Without a template, they seed a minimal note's frontmatter.
//! - Neither: a minimal note — a `created: <today>` frontmatter block followed
//!   by a `# <title>` H1.
//!
//! `created: <today>` is always present in generated frontmatter unless the
//! user supplied their own `created` pair.
//!
//! Writes go through a temp file + atomic `rename`, matching the tmp+rename
//! pattern used by [`super::append`].

use crate::error::{FsError, Result};
use chrono::Utc;
use onebrain_core::CoreError;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Result of [`new_note`]. `path` is vault-relative.
#[derive(Debug, Serialize, PartialEq)]
pub struct NewResult {
    /// Vault-relative path of the note that was created.
    pub path: PathBuf,
    /// `true` — a file was written (always true on success).
    pub created: bool,
    /// `true` if an existing file was overwritten (`--force`).
    pub overwritten: bool,
    /// Template name used, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// Create a new note at `rel_path` (relative to `vault_root`).
///
/// - `template`: resolve `<vault>/.claude/plugins/onebrain/templates/<NAME>.md`,
///   substitute `{{date}}` / `{{title}}` / `{{slug}}`, and use it as the body.
///   Missing template → [`CoreError::InvalidTarget`].
/// - `frontmatter`: `key=value` pairs merged into the note's leading YAML
///   frontmatter block.
/// - `force`: overwrite an existing file. Without it, an existing file is
///   [`CoreError::InvalidTarget`].
///
/// Parent directories are created as needed. IO failures map to [`FsError::Io`].
pub fn new_note(
    vault_root: &Path,
    rel_path: &Path,
    template: Option<&str>,
    frontmatter: &[String],
    force: bool,
) -> Result<NewResult> {
    let abs = vault_root.join(rel_path);

    let exists = abs.exists();
    if exists && !force {
        return Err(FsError::Core(CoreError::InvalidTarget(format!(
            "file exists: {} (use --force)",
            rel_path.display()
        ))));
    }

    let title = title_from_path(rel_path);
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let pairs = parse_frontmatter(frontmatter)?;

    let body = match template {
        Some(name) => {
            let raw = load_template(vault_root, name)?;
            let substituted = substitute(&raw, &today, &title, &slug(&title));
            merge_frontmatter(&substituted, &pairs, &today)
        }
        None => minimal_note(&title, &pairs, &today),
    };

    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|source| FsError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    atomic_write(&abs, body.as_bytes())?;

    Ok(NewResult {
        path: rel_path.to_path_buf(),
        created: true,
        overwritten: exists,
        template: template.map(str::to_string),
    })
}

/// Derive the note title from the path's file stem (e.g.
/// `03-knowledge/ml/New Topic.md` → `New Topic`).
fn title_from_path(rel_path: &Path) -> String {
    rel_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_string()
}

/// Kebab-case `title`: lowercase, runs of non-alphanumerics collapse to a
/// single `-`, leading/trailing `-` trimmed.
fn slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = false;
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Load `<vault>/.claude/plugins/onebrain/templates/<name>.md`. Missing → error.
fn load_template(vault_root: &Path, name: &str) -> Result<String> {
    let path = vault_root
        .join(".claude/plugins/onebrain/templates")
        .join(format!("{name}.md"));
    std::fs::read_to_string(&path).map_err(|_| {
        FsError::Core(CoreError::InvalidTarget(format!(
            "template not found: {}",
            path.display()
        )))
    })
}

/// Replace `{{date}}`, `{{title}}`, `{{slug}}` placeholders.
fn substitute(raw: &str, date: &str, title: &str, slug: &str) -> String {
    raw.replace("{{date}}", date)
        .replace("{{title}}", title)
        .replace("{{slug}}", slug)
}

/// Parse `key=value` items into ordered pairs. Splits on the FIRST `=` so
/// values may contain `=`. An item without `=` is an error.
fn parse_frontmatter(items: &[String]) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::with_capacity(items.len());
    for item in items {
        let Some((k, v)) = item.split_once('=') else {
            return Err(FsError::Core(CoreError::InvalidTarget(format!(
                "invalid --frontmatter pair (expected key=value): {item}"
            ))));
        };
        let key = k.trim();
        if key.is_empty() {
            return Err(FsError::Core(CoreError::InvalidTarget(format!(
                "invalid --frontmatter pair (empty key): {item}"
            ))));
        }
        pairs.push((key.to_string(), v.trim().to_string()));
    }
    Ok(pairs)
}

/// Render a YAML frontmatter block from `pairs`, guaranteeing a `created` key
/// (defaulting to `today` when the user didn't supply one). Returns the block
/// including the fenced `---` delimiters and a trailing newline.
fn render_frontmatter(pairs: &[(String, String)], today: &str) -> String {
    let mut lines: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}: {}", yaml_scalar(v)))
        .collect();
    if !pairs.iter().any(|(k, _)| k == "created") {
        lines.push(format!("created: {today}"));
    }
    format!("---\n{}\n---\n", lines.join("\n"))
}

/// Quote a YAML scalar only when it would otherwise be ambiguous (empty, or
/// containing a character that affects YAML parsing). Simple bare words and
/// dates pass through unquoted.
fn yaml_scalar(value: &str) -> String {
    let needs_quote = value.is_empty()
        || value.starts_with(' ')
        || value.ends_with(' ')
        || value.starts_with([
            '!', '&', '*', '?', '|', '>', '%', '@', '`', '"', '\'', '#', ',',
        ]);
    if needs_quote {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

/// Build a minimal note: frontmatter block (with `created`) + `# <title>` H1.
fn minimal_note(title: &str, pairs: &[(String, String)], today: &str) -> String {
    let fm = render_frontmatter(pairs, today);
    format!("{fm}\n# {title}\n")
}

/// Merge `pairs` into the leading frontmatter block of `content`. If `content`
/// has no leading frontmatter block, prepend one. User keys override existing
/// keys; a `created` key is guaranteed (defaulting to `today`).
fn merge_frontmatter(content: &str, pairs: &[(String, String)], today: &str) -> String {
    // Nothing to inject and no created guarantee needed below: keep as-is only
    // if the content already carries a `created`. Otherwise we still want to
    // ensure `created` exists when we synthesise a block.
    match split_frontmatter(content) {
        Some((existing, rest)) => {
            // Override/append the user pairs onto the existing block.
            let mut merged = existing;
            for (k, v) in pairs {
                upsert(&mut merged, k, v);
            }
            if !merged.iter().any(|(k, _)| k == "created") {
                merged.push(("created".to_string(), today.to_string()));
            }
            let block = render_pairs_block(&merged);
            format!("{block}{rest}")
        }
        None => {
            // No leading frontmatter — prepend a fresh block built from pairs.
            let block = render_frontmatter(pairs, today);
            // Keep the body separated from the block by a blank line, matching
            // the minimal-note layout.
            let body = content.trim_start_matches('\n');
            format!("{block}\n{body}")
        }
    }
}

/// Insert or replace `key` in an ordered pair list.
fn upsert(pairs: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some(slot) = pairs.iter_mut().find(|(k, _)| k == key) {
        slot.1 = value.to_string();
    } else {
        pairs.push((key.to_string(), value.to_string()));
    }
}

/// Render an ordered pair list as a fenced YAML block (with trailing newline).
fn render_pairs_block(pairs: &[(String, String)]) -> String {
    let lines: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}: {}", yaml_scalar(v)))
        .collect();
    format!("---\n{}\n---\n", lines.join("\n"))
}

/// If `content` opens with a `---` fenced frontmatter block, parse its simple
/// `key: value` lines and return `(pairs, rest)` where `rest` is the remainder
/// (everything after the closing `---` line, including its trailing newline).
/// Returns `None` when there is no leading frontmatter block.
fn split_frontmatter(content: &str) -> Option<(Vec<(String, String)>, String)> {
    let rest = content.strip_prefix("---\n")?;
    let close = rest.find("\n---\n");
    // Also accept a block whose closing fence is the final line without a
    // trailing newline.
    let (body_block, after) = match close {
        Some(idx) => (&rest[..idx], &rest[idx + "\n---\n".len()..]),
        None => {
            let trimmed = rest.strip_suffix("\n---")?;
            (trimmed, "")
        }
    };

    let mut pairs = Vec::new();
    for line in body_block.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (k, v) = line.split_once(':')?;
        pairs.push((k.trim().to_string(), v.trim().to_string()));
    }
    Some((pairs, after.to_string()))
}

/// Atomic write via `{path}.tmp` + `rename`. Matches [`super::append`]'s pattern.
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

    fn read(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join(rel)).unwrap()
    }

    fn write_template(root: &Path, name: &str, body: &str) {
        let dir = root.join(".claude/plugins/onebrain/templates");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.md")), body).unwrap();
    }

    #[test]
    fn create_plain_note_has_created_and_h1() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let today = Utc::now().format("%Y-%m-%d").to_string();

        let res = new_note(
            root,
            Path::new("03-knowledge/ml/New Topic.md"),
            None,
            &[],
            false,
        )
        .unwrap();
        assert_eq!(res.path, PathBuf::from("03-knowledge/ml/New Topic.md"));
        assert!(res.created);
        assert!(!res.overwritten);
        assert_eq!(res.template, None);

        let out = read(root, "03-knowledge/ml/New Topic.md");
        assert!(out.contains(&format!("created: {today}")), "got: {out}");
        assert!(out.contains("# New Topic"), "got: {out}");
        assert!(out.starts_with("---\n"));
    }

    #[test]
    fn create_with_frontmatter_pairs_includes_keys() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        new_note(
            root,
            Path::new("note.md"),
            None,
            &["tags=ai".to_string(), "status=draft".to_string()],
            false,
        )
        .unwrap();

        let out = read(root, "note.md");
        assert!(out.contains("tags: ai"), "got: {out}");
        assert!(out.contains("status: draft"), "got: {out}");
        // created is still injected since the user didn't supply it.
        assert!(out.contains("created: "), "got: {out}");
    }

    #[test]
    fn user_supplied_created_is_not_overridden() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        new_note(
            root,
            Path::new("note.md"),
            None,
            &["created=1999-01-01".to_string()],
            false,
        )
        .unwrap();

        let out = read(root, "note.md");
        assert!(out.contains("created: 1999-01-01"), "got: {out}");
        // Only one created line.
        assert_eq!(out.matches("created:").count(), 1, "got: {out}");
    }

    #[test]
    fn template_substitutes_date_title_slug() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let today = Utc::now().format("%Y-%m-%d").to_string();
        write_template(
            root,
            "concept",
            "---\ncreated: {{date}}\n---\n# {{title}}\n\nslug: {{slug}}\n",
        );

        let res = new_note(
            root,
            Path::new("03-knowledge/Deep Learning.md"),
            Some("concept"),
            &[],
            false,
        )
        .unwrap();
        assert_eq!(res.template.as_deref(), Some("concept"));

        let out = read(root, "03-knowledge/Deep Learning.md");
        assert!(out.contains(&format!("created: {today}")), "got: {out}");
        assert!(out.contains("# Deep Learning"), "got: {out}");
        assert!(out.contains("slug: deep-learning"), "got: {out}");
        assert!(!out.contains("{{"), "placeholders remain: {out}");
    }

    #[test]
    fn template_not_found_errors() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let err = new_note(root, Path::new("note.md"), Some("missing"), &[], false).unwrap_err();
        assert!(matches!(err, FsError::Core(CoreError::InvalidTarget(_))));
    }

    #[test]
    fn file_exists_without_force_errors() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("note.md"), "original\n").unwrap();

        let err = new_note(root, Path::new("note.md"), None, &[], false).unwrap_err();
        assert!(matches!(err, FsError::Core(CoreError::InvalidTarget(_))));
        // The original file is untouched.
        assert_eq!(read(root, "note.md"), "original\n");
    }

    #[test]
    fn force_overwrites_existing_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("note.md"), "original\n").unwrap();

        let res = new_note(root, Path::new("note.md"), None, &[], true).unwrap();
        assert!(res.overwritten);
        let out = read(root, "note.md");
        assert!(out.contains("# note"), "got: {out}");
        assert!(!out.contains("original"), "got: {out}");
    }

    #[test]
    fn parent_dirs_created() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Deeply nested parent that doesn't exist yet.
        new_note(
            root,
            Path::new("01-projects/foo/bar/Note.md"),
            None,
            &[],
            false,
        )
        .unwrap();
        assert!(root.join("01-projects/foo/bar/Note.md").exists());
    }

    #[test]
    fn template_with_frontmatter_pairs_merged() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_template(
            root,
            "t",
            "---\ncreated: {{date}}\ntags: old\n---\n# {{title}}\n",
        );

        new_note(
            root,
            Path::new("note.md"),
            Some("t"),
            &["tags=new".to_string(), "status=wip".to_string()],
            false,
        )
        .unwrap();

        let out = read(root, "note.md");
        // tags overridden, status appended, only one tags line.
        assert!(out.contains("tags: new"), "got: {out}");
        assert!(!out.contains("tags: old"), "got: {out}");
        assert!(out.contains("status: wip"), "got: {out}");
        assert_eq!(out.matches("tags:").count(), 1, "got: {out}");
        assert!(out.contains("# note"), "got: {out}");
    }

    #[test]
    fn template_without_frontmatter_gets_block_prepended() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_template(root, "bare", "# {{title}}\n\nbody text\n");

        new_note(
            root,
            Path::new("note.md"),
            Some("bare"),
            &["tags=ai".to_string()],
            false,
        )
        .unwrap();

        let out = read(root, "note.md");
        assert!(out.starts_with("---\n"), "got: {out}");
        assert!(out.contains("tags: ai"), "got: {out}");
        assert!(out.contains("created: "), "got: {out}");
        assert!(out.contains("# note"), "got: {out}");
        assert!(out.contains("body text"), "got: {out}");
    }

    #[test]
    fn invalid_frontmatter_pair_errors() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let err = new_note(
            root,
            Path::new("note.md"),
            None,
            &["no_equals_here".to_string()],
            false,
        )
        .unwrap_err();
        assert!(matches!(err, FsError::Core(CoreError::InvalidTarget(_))));
    }

    #[test]
    fn no_tmp_file_left_behind() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        new_note(root, Path::new("note.md"), None, &[], false).unwrap();
        assert!(!root.join("note.md.tmp").exists());
        assert!(root.join("note.md").exists());
    }

    #[test]
    fn slug_kebab_cases_title() {
        assert_eq!(slug("Deep Learning"), "deep-learning");
        assert_eq!(slug("New Topic 2026!"), "new-topic-2026");
        assert_eq!(slug("  Hello---World  "), "hello-world");
    }
}
