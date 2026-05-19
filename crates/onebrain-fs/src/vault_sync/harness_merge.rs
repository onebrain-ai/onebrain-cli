//! Step 6 · merge `@`-import lines from the extracted harness files into the
//! vault's. Port of Bun's `mergeHarnessFiles` / `mergeHarnessFile`.
//!
//! Algorithm:
//!   1. For each of `CLAUDE.md` / `GEMINI.md` / `AGENTS.md`:
//!      a. If source absent — return 0 (nothing to do).
//!      b. If vault file absent — write source verbatim; return count of `@`-lines in source.
//!      c. Collect `@`-lines from source not already present in vault.
//!      d. If none — return 0.
//!      e. Insert them before the LAST `@`-line in vault. If vault has no `@`-line, append `["", ...newImports]` to the end.
//!   2. Return total imports added across all three files.
//!
//! Splits text on `\n` (LF only) — Bun's `.split('\n')` doesn't normalize CRLF;
//! we mirror that exactly to avoid silent reformatting of mixed-line-ending files.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// The three harness files vault-sync merges.
pub const HARNESS_FILES: [&str; 3] = ["CLAUDE.md", "GEMINI.md", "AGENTS.md"];

/// Merge a single harness file. Returns count of new `@`-import lines added.
pub fn merge_harness_file(
    extracted_dir: &Path,
    vault_root: &Path,
    filename: &str,
) -> std::io::Result<u64> {
    let src_path = extracted_dir.join(filename);
    let dest_path = vault_root.join(filename);

    let repo_text = match fs::read_to_string(&src_path) {
        Ok(t) => t,
        Err(_) => return Ok(0), // Source not in tarball — nothing to do
    };

    let vault_text = match fs::read_to_string(&dest_path) {
        Ok(t) => t,
        Err(_) => {
            // Vault copy doesn't exist — write repo version verbatim.
            fs::write(&dest_path, &repo_text)?;
            return Ok(count_at_lines(&repo_text));
        }
    };

    // Build set of trimmed @-lines already in vault.
    let vault_at_set: HashSet<&str> = vault_text
        .split('\n')
        .filter(|l| l.starts_with('@'))
        .map(|l| l.trim())
        .collect();

    // Collect new imports from repo. trimEnd matches Bun's `.map(l => l.trimEnd())`.
    let new_imports: Vec<String> = repo_text
        .split('\n')
        .filter(|l| l.starts_with('@') && !vault_at_set.contains(l.trim()))
        .map(|l| l.trim_end().to_string())
        .collect();

    if new_imports.is_empty() {
        return Ok(0);
    }

    // Insert before the LAST @-line in vault (preserves structural ordering),
    // or append after a blank line if no prior @-line exists.
    let mut vault_lines: Vec<String> = vault_text.split('\n').map(String::from).collect();
    let last_at_idx = vault_lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with('@'))
        .map(|(i, _)| i)
        .next_back();

    match last_at_idx {
        Some(i) => {
            // splice(i, 0, ...newImports) — insert before vault_lines[i].
            for (offset, imp) in new_imports.iter().enumerate() {
                vault_lines.insert(i + offset, imp.clone());
            }
        }
        None => {
            vault_lines.push(String::new());
            for imp in &new_imports {
                vault_lines.push(imp.clone());
            }
        }
    }

    let merged = vault_lines.join("\n");
    fs::write(&dest_path, merged)?;
    Ok(new_imports.len() as u64)
}

/// Merge all three harness files. Returns total imports added.
pub fn merge_harness_files(extracted_dir: &Path, vault_root: &Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    for filename in HARNESS_FILES {
        total += merge_harness_file(extracted_dir, vault_root, filename)?;
    }
    Ok(total)
}

fn count_at_lines(text: &str) -> u64 {
    text.split('\n').filter(|l| l.starts_with('@')).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(p: &Path, s: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, s).unwrap();
    }

    #[test]
    fn new_import_inserted_before_last_at_line() {
        let dir = tempdir().unwrap();
        let ext = dir.path().join("ext");
        let vault = dir.path().join("vault");
        write(
            &vault.join("CLAUDE.md"),
            "# My Config\n\n@.claude/plugins/onebrain/INSTRUCTIONS.md\n",
        );
        write(
            &ext.join("CLAUDE.md"),
            "@.claude/plugins/onebrain/INSTRUCTIONS.md\n@.claude/plugins/onebrain/NEW.md\n",
        );
        let added = merge_harness_file(&ext, &vault, "CLAUDE.md").unwrap();
        assert!(added >= 1);
        let merged = fs::read_to_string(vault.join("CLAUDE.md")).unwrap();
        assert!(merged.contains("@.claude/plugins/onebrain/NEW.md"));
        assert!(merged.contains("# My Config"));
        // No dup of existing import
        let count = merged
            .matches("@.claude/plugins/onebrain/INSTRUCTIONS.md")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn duplicate_imports_not_reinjected() {
        let dir = tempdir().unwrap();
        let ext = dir.path().join("ext");
        let vault = dir.path().join("vault");
        let existing = "@.claude/plugins/onebrain/INSTRUCTIONS.md\n";
        write(&vault.join("CLAUDE.md"), existing);
        write(&ext.join("CLAUDE.md"), existing);
        let added = merge_harness_file(&ext, &vault, "CLAUDE.md").unwrap();
        assert_eq!(added, 0);
    }

    #[test]
    fn missing_vault_file_writes_repo_version_and_counts_imports() {
        let dir = tempdir().unwrap();
        let ext = dir.path().join("ext");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        write(
            &ext.join("CLAUDE.md"),
            "@.claude/plugins/onebrain/A.md\n@.claude/plugins/onebrain/B.md\n",
        );
        let added = merge_harness_file(&ext, &vault, "CLAUDE.md").unwrap();
        assert_eq!(added, 2);
        let written = fs::read_to_string(vault.join("CLAUDE.md")).unwrap();
        assert!(written.contains("A.md"));
        assert!(written.contains("B.md"));
    }

    #[test]
    fn vault_with_no_at_line_appends_after_blank() {
        let dir = tempdir().unwrap();
        let ext = dir.path().join("ext");
        let vault = dir.path().join("vault");
        write(&vault.join("CLAUDE.md"), "# Just text\n");
        write(&ext.join("CLAUDE.md"), "@.claude/plugins/onebrain/X.md\n");
        let added = merge_harness_file(&ext, &vault, "CLAUDE.md").unwrap();
        assert_eq!(added, 1);
        let merged = fs::read_to_string(vault.join("CLAUDE.md")).unwrap();
        assert!(merged.contains("# Just text"));
        assert!(merged.contains("@.claude/plugins/onebrain/X.md"));
    }

    #[test]
    fn merge_three_files_sums_correctly() {
        let dir = tempdir().unwrap();
        let ext = dir.path().join("ext");
        let vault = dir.path().join("vault");
        for f in HARNESS_FILES {
            write(&ext.join(f), "@x\n@y\n");
        }
        // vault has nothing → each file creates a fresh write counting 2.
        fs::create_dir_all(&vault).unwrap();
        let total = merge_harness_files(&ext, &vault).unwrap();
        assert_eq!(total, 6);
    }
}
