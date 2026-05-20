//! Shared safety guards for filesystem-mutating commands.
//!
//! Currently exposes `refuse_dangerous_vault_path` — used by both
//! `onebrain vault-sync` (the user-facing subcommand) and
//! `onebrain doctor --fix` (the `plugin-files` recipe, which invokes the
//! same overlay path internally). Keeping the guard in one place ensures
//! the recipe cannot accidentally bypass a refusal the subcommand enforces.

use anyhow::{bail, Result};
use std::env;
use std::path::{Path, PathBuf};

/// Refuse the obvious "I did not mean to write here" paths. Currently:
///   - any filesystem root (`/` on Unix · `C:\` / `D:\` / `\` on Windows)
///   - `$HOME` / `%USERPROFILE%` itself (literal match against the resolved
///     home dir)
///
/// Anything else (including arbitrary subdirectories of `$HOME` and `/tmp/...`)
/// is allowed — vault-sync's bootstrap-from-empty-dir behavior matches Bun and
/// is intentional.
pub fn refuse_dangerous_vault_path(p: &Path) -> Result<()> {
    let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());

    // Portable filesystem-root check — works for `/` on Unix and drive roots
    // (`C:\`, `D:\`, etc.) on Windows. Both produce `Path::parent() == None`.
    if canonical.parent().is_none() {
        bail!(
            "refusing to vault-sync at filesystem root '{}' — pass an explicit vault directory",
            canonical.display()
        );
    }

    // `$HOME` on Unix / `USERPROFILE` on Windows — checked literally against
    // the canonicalized target so symlinks to home are also caught.
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = env::var_os(home_var).map(PathBuf::from) {
        let canonical_home = home.canonicalize().unwrap_or(home);
        if canonical == canonical_home {
            bail!(
                "refusing to vault-sync at home directory ({}) — create a dedicated vault directory first",
                canonical.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Filesystem root is platform-specific: `/` on Unix, drive root on
    /// Windows (`canonicalize` of `\` resolves to `\\?\C:\` typically). The
    /// portable signal is `parent().is_none()` — verify via the platform's
    /// own root accessor rather than hard-coding `/`.
    #[test]
    fn refuses_filesystem_root() {
        let mut root = std::env::temp_dir();
        while let Some(parent) = root.parent() {
            root = parent.to_path_buf();
        }
        let err = refuse_dangerous_vault_path(&root).unwrap_err();
        assert!(
            err.to_string().contains("filesystem root"),
            "error must mention filesystem root · got: {err}"
        );
    }

    #[test]
    fn refuses_home_directory() {
        let d = tempdir().unwrap();
        std::env::set_var("HOME", d.path());
        std::env::set_var("USERPROFILE", d.path());
        let err = refuse_dangerous_vault_path(d.path()).unwrap_err();
        assert!(
            err.to_string().contains("home directory"),
            "error must mention home directory · got: {err}"
        );
    }

    #[test]
    fn allows_dedicated_subdirectory() {
        let d = tempdir().unwrap();
        let sub = d.path().join("vault");
        std::fs::create_dir_all(&sub).unwrap();
        refuse_dangerous_vault_path(&sub).expect("dedicated subdirectory should be allowed");
    }
}
