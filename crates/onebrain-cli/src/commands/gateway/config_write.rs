//! Writing `~/.onebrain/gateway.yml` without destroying an operator's
//! hand-edited file. Extracted from `telegram_setup.rs` (v3.5.0 T3) so the
//! tunnel wizard's `public_url` write gets the same guarantees.
//!
//! Strategy, in order — each textual path is taken only if the result reads
//! back (same `serde_yaml::from_str` `config.rs` uses) with `key` equal to
//! `value`; otherwise the next one is tried:
//! 1. **Append** — the key is absent: append `rendered` to the exact bytes.
//! 2. **Replace one line** — the key is present as a one-line scalar
//!    (`public_url: …`) and `rendered` is one line: swap that line only.
//! 3. **Round-trip** — rebuild the document from a parsed `serde_yaml::Value`.
//!    Keeps every key, loses comments/layout; reported via `WriteOutcome`.
//!    A raw `Value`, never `GatewayConfig`: its `bot_token` is
//!    `skip_serializing`, so a typed round-trip would drop the secret.

use std::io::Write;
use std::path::Path;

use anyhow::Context;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    /// No file existed; written from scratch.
    Created,
    /// Key was absent; appended textually, existing bytes untouched.
    Appended,
    /// One-line scalar key replaced in place, every other byte untouched.
    ReplacedLine,
    /// Whole document re-serialized; `dropped_comments` when it had any.
    Rewrote { dropped_comments: bool },
}

pub(crate) fn set_top_level_key(
    path: &Path,
    key: &str,
    value: serde_yaml::Value,
    rendered: &str,
) -> anyhow::Result<WriteOutcome> {
    debug_assert!(
        rendered.ends_with('\n'),
        "rendered YAML must end with a newline"
    );
    let existing = match std::fs::read_to_string(path) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut mapping = match existing.as_deref() {
        Some(content) => {
            match serde_yaml::from_str::<serde_yaml::Value>(content)
                .with_context(|| format!("parse existing {}", path.display()))?
            {
                serde_yaml::Value::Mapping(m) => m,
                serde_yaml::Value::Null => serde_yaml::Mapping::new(),
                other => anyhow::bail!(
                    "{} must be a YAML mapping at its root, found {other:?}",
                    path.display()
                ),
            }
        }
        None => serde_yaml::Mapping::new(),
    };
    let key_value = serde_yaml::Value::String(key.to_string());
    let parent = path
        .parent()
        .context("gateway.yml has no parent directory")?;

    if let Some(content) = existing.as_deref() {
        let candidate = if mapping.contains_key(&key_value) {
            replace_scalar_line(content, key, rendered)
        } else {
            let mut appended = content.to_string();
            if !appended.is_empty() && !appended.ends_with('\n') {
                appended.push('\n');
            }
            appended.push_str(rendered);
            Some(appended)
        };
        if let Some(text) = candidate {
            if reads_back(&text, key, &value) {
                ensure_private_dir(parent)?;
                write_private_file(path, text.as_bytes())?;
                return Ok(if mapping.contains_key(&key_value) {
                    WriteOutcome::ReplacedLine
                } else {
                    WriteOutcome::Appended
                });
            }
        }
    }

    mapping.insert(key_value, value);
    let doc = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .context("serialize gateway.yml")?;
    ensure_private_dir(parent)?;
    write_private_file(path, doc.as_bytes())?;
    Ok(match existing {
        None => WriteOutcome::Created,
        Some(c) => WriteOutcome::Rewrote {
            dropped_comments: has_comment_line(&c),
        },
    })
}

/// `content` with the single column-0 `key:` line swapped for `rendered`, or
/// `None` when that is not provably a one-line scalar: `rendered` spans
/// lines, the key appears more than once, or the next line is indented / a
/// list item (a block value).
fn replace_scalar_line(content: &str, key: &str, rendered: &str) -> Option<String> {
    if rendered.trim_end_matches('\n').contains('\n') {
        return None;
    }
    let prefix = format!("{key}:");
    let lines: Vec<&str> = content.lines().collect();
    let hits: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].starts_with(&prefix))
        .collect();
    let &[idx] = hits.as_slice() else {
        return None;
    };
    if lines
        .get(idx + 1)
        .is_some_and(|next| next.starts_with([' ', '\t', '-']))
    {
        return None;
    }
    let mut out = String::with_capacity(content.len() + rendered.len());
    for (i, line) in lines.iter().enumerate() {
        if i == idx {
            out.push_str(rendered);
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    Some(out)
}

/// `true` iff `content` parses as one YAML document whose `key` equals `value`.
fn reads_back(content: &str, key: &str, value: &serde_yaml::Value) -> bool {
    serde_yaml::from_str::<serde_yaml::Value>(content)
        .ok()
        .and_then(|v| v.get(key).cloned())
        .is_some_and(|got| &got == value)
}

/// Whole-line `#` comment present (conservative: trailing comments are not
/// detected — only used to decide whether to warn).
fn has_comment_line(content: &str) -> bool {
    content.lines().any(|l| l.trim_start().starts_with('#'))
}

/// Create `dir` 0700 (Unix), re-asserting the mode if it already existed.
/// Moved verbatim from `telegram_setup.rs`.
pub(crate) fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create private dir {}", dir.display()))?;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(error = %e, path = %dir.display(), "could not re-assert 0700");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).with_context(|| format!("create dir {}", dir.display()))
    }
}

/// Atomically replace `path` with `bytes` via a 0600 `<name>.tmp` sibling.
/// Never touches the parent's mode (callers that want a private parent call
/// [`ensure_private_dir`] first; `~/Library/LaunchAgents` must NOT be chmodded).
pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let name = path
        .file_name()
        .context("path has no file name")?
        .to_string_lossy();
    let tmp = path.with_file_name(format!("{name}.tmp"));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write {}", tmp.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(error = %e, path = %tmp.display(), "could not re-assert 0600");
        }
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> serde_yaml::Value {
        serde_yaml::Value::String(v.to_string())
    }

    fn setup(content: Option<&str>) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".onebrain").join("gateway.yml");
        if let Some(c) = content {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, c).unwrap();
        }
        (root, path)
    }

    const NEW_URL: &str = "public_url: 'https://b.example.com'\n";

    #[test]
    fn appends_a_new_key_and_keeps_every_existing_byte() {
        let original = "# mine\nport: 9999\n";
        let (_r, path) = setup(Some(original));
        let out =
            set_top_level_key(&path, "public_url", s("https://b.example.com"), NEW_URL).unwrap();
        assert_eq!(out, WriteOutcome::Appended);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with(original), "{content}");
        assert!(content.ends_with(NEW_URL), "{content}");
    }

    /// Re-running the tunnel wizard with a new hostname must not cost the
    /// operator their comments.
    #[test]
    fn replaces_a_one_line_scalar_in_place_and_keeps_comments() {
        let (_r, path) = setup(Some(
            "# keep\npublic_url: 'https://old.example.com'\n# also keep\nport: 1\n",
        ));
        let out = set_top_level_key(
            &path,
            "public_url",
            s("https://new.example.com"),
            "public_url: 'https://new.example.com'\n",
        )
        .unwrap();
        assert_eq!(out, WriteOutcome::ReplacedLine);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# keep\npublic_url: 'https://new.example.com'\n# also keep\nport: 1\n"
        );
    }

    #[test]
    fn a_block_valued_key_falls_back_to_a_reported_round_trip() {
        let (_r, path) = setup(Some("# c\npublic_url:\n  nested: x\n"));
        let out =
            set_top_level_key(&path, "public_url", s("https://b.example.com"), NEW_URL).unwrap();
        assert_eq!(
            out,
            WriteOutcome::Rewrote {
                dropped_comments: true
            }
        );
        let v: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["public_url"], s("https://b.example.com"));
    }

    /// A multi-line `rendered` never takes the one-line replace path, even
    /// when it would happen to read back.
    #[test]
    fn a_multi_line_rendered_value_never_takes_the_one_line_replace() {
        let (_r, path) = setup(Some("# c\nblock: ''\n"));
        let mut m = serde_yaml::Mapping::new();
        m.insert(s("a"), s("b"));
        let out = set_top_level_key(
            &path,
            "block",
            serde_yaml::Value::Mapping(m),
            "block:\n  a: b\n",
        )
        .unwrap();
        assert_eq!(
            out,
            WriteOutcome::Rewrote {
                dropped_comments: true
            }
        );
    }

    #[test]
    fn a_missing_file_is_created_0600_in_a_0700_dir() {
        let (_r, path) = setup(None);
        assert_eq!(
            set_top_level_key(&path, "public_url", s("https://b.example.com"), NEW_URL).unwrap(),
            WriteOutcome::Created
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
    }

    #[test]
    fn a_document_end_marker_defeats_append_and_falls_back() {
        let (_r, path) = setup(Some("port: 9999\n...\n"));
        let out =
            set_top_level_key(&path, "public_url", s("https://b.example.com"), NEW_URL).unwrap();
        assert!(matches!(out, WriteOutcome::Rewrote { .. }), "{out:?}");
        let v: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["port"].as_i64(), Some(9999));
    }

    #[test]
    fn write_private_file_leaves_the_parent_mode_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("LaunchAgents");
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            write_private_file(&dir.join("x.plist"), b"x").unwrap();
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o755
            );
            assert_eq!(
                std::fs::metadata(dir.join("x.plist"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        #[cfg(not(unix))]
        write_private_file(&dir.join("x.plist"), b"x").unwrap();
        assert!(!dir.join("x.plist.tmp").exists());
    }
}
