//! `onebrain vault-sync` — pull the upstream `onebrain-ai/onebrain` release
//! tarball and overlay the bundled plugin / harness / docs onto the current
//! vault. Mirrors Bun's `vaultSyncCommand`: exit 1 on `!result.ok`, otherwise 0.

use anyhow::{anyhow, bail, Context, Result};
use onebrain_core::find_vault_root;
use onebrain_fs::{run_vault_sync, VaultSyncOptions};
use std::env;
use std::path::{Path, PathBuf};

/// Entry point — returns `Ok(0)` on success, `Ok(1)` on any critical failure.
/// Side-effects: writes to the vault filesystem; prints `vault-sync: …` lines
/// to stdout in non-TTY mode (Bun parity).
///
/// `branch` mirrors Bun v2.3.3's `--branch <branch>` CLI flag and overrides the
/// branch resolution that otherwise reads `vault.yml::update_channel`. When
/// `None`, the orchestrator falls back to `resolve_branch(update_channel)`.
///
/// The orchestrator already prints one of `vault-sync: download failed: …` /
/// `vault-sync: plugin sync failed: …` / `vault-sync: harness merge failed:
/// …` / `vault-sync: vault.yml update failed: …` to stderr on every known
/// failure path, so the handler keeps its own output minimal (Bun's
/// `vaultSyncCommand` does `process.exit(1)` with no extra logging). We
/// still re-print `result.error` as a defensive backstop for any future
/// failure path that sets `result.error` without logging to stderr — better
/// to occasionally duplicate one line than leave the user with a silent
/// non-zero exit.
pub fn run(vault_root_override: Option<PathBuf>, branch: Option<String>) -> Result<i32> {
    // Bun v2.3.3 parity: optional positional `<vault_root>` lets the caller
    // supply the vault path directly (skipping the cwd walk-up). When absent,
    // walk up from cwd as before.
    let vault_root_path = match vault_root_override {
        Some(p) => p,
        None => {
            let cwd = env::current_dir().context("read current directory")?;
            find_vault_root(&cwd)
                .ok_or_else(|| anyhow!("not inside a vault (no vault.yml found)"))?
                .as_path()
                .to_path_buf()
        }
    };

    // Safety guard — vault-sync writes destructively into the target directory
    // (`.claude/plugins/onebrain/`, root markdown files, etc.). Refusing the
    // obvious foot-cannons (`/`, `$HOME`) prevents accidental splattering when
    // a user runs `onebrain vault-sync ~` by mistake. The list is deliberately
    // narrow — any other path is the caller's responsibility.
    refuse_dangerous_vault_path(&vault_root_path)?;

    let result = run_vault_sync(
        vault_root_path.as_path(),
        VaultSyncOptions {
            branch,
            ..VaultSyncOptions::default()
        },
    );

    if !result.ok {
        if let Some(err) = result.error.as_ref() {
            eprintln!("vault-sync: failed: {err}");
        } else {
            // No structured error captured — still surface non-zero with a
            // generic hint so schedulers/CI can see something happened.
            eprintln!("vault-sync: failed (no error detail captured)");
        }
        return Ok(1);
    }
    Ok(0)
}

/// Refuse the obvious "I did not mean to write here" paths. Currently:
///   - any filesystem root (`/` on Unix · `C:\` / `D:\` / `\` on Windows)
///   - `$HOME` / `%USERPROFILE%` itself (literal match against the resolved
///     home dir)
///
/// Anything else (including arbitrary subdirectories of `$HOME` and `/tmp/...`)
/// is allowed — vault-sync's bootstrap-from-empty-dir behavior matches Bun and
/// is intentional. Callers wanting to override this for tests can bypass via
/// the lower-level `run_vault_sync(...)` library entry point.
fn refuse_dangerous_vault_path(p: &Path) -> Result<()> {
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
        // `std::env::temp_dir()` always has a root ancestor on the platform;
        // walk up to it via repeated `.parent()` calls so the test stays
        // portable across Unix and Windows.
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
        // Inject a stub home dir so the test doesn't depend on whether the
        // runner has HOME/USERPROFILE set. Both vars are stubbed to keep the
        // body identical across platforms.
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
        std::env::set_var("HOME", d.path());
        std::env::set_var("USERPROFILE", d.path());
        let sub = d.path().join("my-vault");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(refuse_dangerous_vault_path(&sub).is_ok());
    }
}
