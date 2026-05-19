//! Step 5 · copy root docs (CONTRIBUTING.md, CHANGELOG.md, PLUGIN-CHANGELOG.md)
//! from the extracted tarball into the vault root. Non-fatal — any missing
//! source file is silently skipped, matching Bun's try/catch swallow.

use std::fs;
use std::path::Path;

/// The fixed list of root docs vault-sync copies. Matches Bun verbatim.
pub const ROOT_DOCS: [&str; 3] = ["CONTRIBUTING.md", "CHANGELOG.md", "PLUGIN-CHANGELOG.md"];

/// Step 5 · copy each root doc. Returns the count of docs actually copied —
/// useful for tests, ignored by the orchestrator (Bun returns void).
pub fn copy_root_docs(extracted_dir: &Path, vault_root: &Path) -> u64 {
    let mut copied = 0u64;
    for doc in ROOT_DOCS.iter() {
        let src = extracted_dir.join(doc);
        let dest = vault_root.join(doc);
        match fs::read(&src) {
            Ok(content) => {
                if fs::write(&dest, content).is_ok() {
                    copied += 1;
                }
            }
            Err(_) => {
                // Silently skip — Bun's try/catch swallows.
            }
        }
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn all_docs_present_copies_three() {
        let dir = tempdir().unwrap();
        let ext = dir.path().join("ext");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&ext).unwrap();
        fs::create_dir_all(&vault).unwrap();
        for d in ROOT_DOCS {
            fs::write(ext.join(d), format!("# {d}\n")).unwrap();
        }
        let got = copy_root_docs(&ext, &vault);
        assert_eq!(got, 3);
        assert!(vault.join("CHANGELOG.md").exists());
    }

    #[test]
    fn missing_docs_silently_skipped() {
        let dir = tempdir().unwrap();
        let ext = dir.path().join("ext");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&ext).unwrap();
        fs::create_dir_all(&vault).unwrap();
        fs::write(ext.join("CHANGELOG.md"), "# c\n").unwrap();
        // Only CHANGELOG.md present.
        let got = copy_root_docs(&ext, &vault);
        assert_eq!(got, 1);
        assert!(vault.join("CHANGELOG.md").exists());
        assert!(!vault.join("CONTRIBUTING.md").exists());
    }
}
