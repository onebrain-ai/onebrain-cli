use crate::Result;
use onebrain_core::VaultRoot;
use std::path::Path;
use walkdir::WalkDir;

/// Count `.md` files in the vault that lack a `qmd-embedded: true` frontmatter line.
///
/// Returns 0 if the vault has no markdown files.
pub fn count_unembedded(root: &VaultRoot, _qmd_collection: &str) -> Result<usize> {
    let mut count = 0usize;
    for entry in WalkDir::new(root.as_path()) {
        let entry = entry.map_err(|source| crate::FsError::WalkFailed {
            path: root.as_path().to_path_buf(),
            source,
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        if !has_embedded_marker(entry.path()) {
            count += 1;
        }
    }
    Ok(count)
}

fn has_embedded_marker(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some(rest) = content.strip_prefix("---\n") else {
        return false;
    };
    let Some(end) = rest.find("\n---") else {
        return false;
    };
    let frontmatter = &rest[..end];
    frontmatter
        .lines()
        .any(|l| l.trim() == "qmd-embedded: true")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn vault_with_files(files: &[(&str, &str)]) -> (tempfile::TempDir, VaultRoot) {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "").unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
        let root = onebrain_core::find_vault_root(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn empty_vault_returns_zero() {
        let (_d, root) = vault_with_files(&[]);
        assert_eq!(count_unembedded(&root, "ob-1").unwrap(), 0);
    }

    #[test]
    fn counts_md_without_embedded_marker() {
        let (_d, root) = vault_with_files(&[
            ("note-a.md", "---\ntags: [x]\n---\nbody"),
            ("note-b.md", "no frontmatter at all"),
        ]);
        assert_eq!(count_unembedded(&root, "ob-1").unwrap(), 2);
    }

    #[test]
    fn skips_md_with_embedded_marker() {
        let (_d, root) = vault_with_files(&[
            ("note-a.md", "---\nqmd-embedded: true\n---\nbody"),
            ("note-b.md", "---\ntags: [x]\n---\nunembedded"),
        ]);
        assert_eq!(count_unembedded(&root, "ob-1").unwrap(), 1);
    }

    #[test]
    fn ignores_non_md_files() {
        let (_d, root) = vault_with_files(&[
            ("note.md", "no frontmatter"),
            ("image.png", "binary"),
            ("vault.yml", ""),
        ]);
        assert_eq!(count_unembedded(&root, "ob-1").unwrap(), 1);
    }
}
