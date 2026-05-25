use crate::error::CoreError;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRoot(PathBuf);

impl VaultRoot {
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, child: impl AsRef<Path>) -> PathBuf {
        self.0.join(child)
    }

    /// Vault display name — the final path component (e.g., `ob-1`). Falls
    /// back to the full path string when the basename can't be extracted
    /// (e.g., a vault at filesystem root, which never happens in practice).
    pub fn name(&self) -> String {
        self.0
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.0.display().to_string())
    }

    /// Construct a `VaultRoot` from an explicit path. Validates that
    /// `vault.yml` exists in the directory. Returns
    /// [`CoreError::NotAVault`] when the path is not a vault root.
    pub fn from_path(path: &Path) -> crate::error::Result<Self> {
        if path.join("vault.yml").is_file() {
            Ok(Self(path.to_path_buf()))
        } else {
            Err(CoreError::NotAVault {
                path: path.to_path_buf(),
            })
        }
    }
}

/// Walk up from `start` looking for the nearest directory containing a
/// `vault.yml`. Returns `None` if none is found before the filesystem root.
///
/// **Resolution ≠ validation.** This function only checks that
/// `vault.yml` exists as a file (`Path::is_file()` returns true for
/// readable regular files and follows symlinks to a regular file). It does
/// NOT parse the file or verify it's valid YAML. Broken symlinks,
/// permission-denied reads, and malformed YAML are all the caller's
/// responsibility:
///
/// - Hook-protocol commands (`session init`) already pattern-match
///   `load_vault_config` errors and emit a `decision:"block"` JSON.
/// - Vault-required commands run `load_vault_config` separately and let
///   the resulting `CoreError::InvalidYaml` exit 65.
/// - `vault current` validates by calling `load_vault_config` and
///   gates its `detected: true` envelope field on success.
pub fn find_vault_root(start: &Path) -> Option<VaultRoot> {
    let mut current = start.to_path_buf();
    loop {
        if current.join("vault.yml").is_file() {
            return Some(VaultRoot(current));
        }
        if !current.pop() {
            return None;
        }
    }
}

/// How a vault was resolved. Stored in [`ResolvedVault`] and surfaced by
/// `onebrain vault current` so users can see why a particular vault was
/// chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultSource {
    /// Resolved from the `--vault <PATH>` CLI flag (highest priority).
    Flag,
    /// Resolved from the `ONEBRAIN_VAULT` environment variable.
    Env,
    /// Resolved by walking up from `$PWD` finding `vault.yml`.
    WalkUp,
}

impl VaultSource {
    /// Human-friendly label per skill-alignment §4.7 / `vault current` UX.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Flag => "--vault flag",
            Self::Env => "ONEBRAIN_VAULT env",
            Self::WalkUp => "walk-up",
        }
    }
}

/// A vault that has been located AND the mechanism that located it. Returned
/// by [`resolve_vault`] / [`require_vault`].
#[derive(Debug, Clone)]
pub struct ResolvedVault {
    pub root: VaultRoot,
    pub source: VaultSource,
}

/// Inputs for the vault resolution chain. Centralised so unit tests can
/// inject deterministic values without touching the process env / cwd.
#[derive(Debug, Clone, Default)]
pub struct VaultResolveInputs {
    /// `--vault <PATH>` value from clap. Highest priority.
    pub flag: Option<PathBuf>,
    /// `ONEBRAIN_VAULT` env var value. Second priority. Clap's `env = ...`
    /// attribute already merges this into `flag` when the flag is absent, but
    /// we expose it separately so tests can verify the priority chain
    /// explicitly.
    pub env: Option<PathBuf>,
    /// Walk-up starting directory (typically `std::env::current_dir()`).
    /// Third priority.
    pub cwd: PathBuf,
}

/// Resolve the active vault per skill-alignment §4.7 priority chain:
///   1. `--vault <PATH>` flag
///   2. `ONEBRAIN_VAULT` env var
///   3. Walk-up from `cwd`
///
/// Returns `Ok(None)` when no vault is found anywhere. Hook-protocol commands
/// translate this to a `{"decision":"block"}` JSON; vault-required commands
/// translate it to [`CoreError::VaultNotFound`].
///
/// When a flag/env path is supplied but doesn't contain a vault.yml, returns
/// [`CoreError::NotAVault`] — explicit "I told you to use this vault but it's
/// not actually one" rather than silently falling through to walk-up. This
/// avoids the surprise where a user typos `--vault /tmp/wrong` and ends up
/// operating on whatever vault happens to be above cwd.
pub fn resolve_vault(inputs: &VaultResolveInputs) -> crate::error::Result<Option<ResolvedVault>> {
    if let Some(path) = &inputs.flag {
        let root = VaultRoot::from_path(path)?;
        return Ok(Some(ResolvedVault {
            root,
            source: VaultSource::Flag,
        }));
    }
    if let Some(path) = &inputs.env {
        let root = VaultRoot::from_path(path)?;
        return Ok(Some(ResolvedVault {
            root,
            source: VaultSource::Env,
        }));
    }
    if let Some(root) = find_vault_root(&inputs.cwd) {
        return Ok(Some(ResolvedVault {
            root,
            source: VaultSource::WalkUp,
        }));
    }
    Ok(None)
}

/// Same as [`resolve_vault`] but maps `None` to [`CoreError::VaultNotFound`]
/// so callers can use the `?` operator. Use for vault-required commands.
pub fn require_vault(inputs: &VaultResolveInputs) -> crate::error::Result<ResolvedVault> {
    resolve_vault(inputs)?.ok_or_else(|| CoreError::VaultNotFound {
        cwd: inputs.cwd.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_vault(dir: &Path) {
        std::fs::write(dir.join("vault.yml"), "").unwrap();
    }

    #[test]
    fn finds_vault_in_starting_dir() {
        let dir = tempdir().unwrap();
        make_vault(dir.path());
        let result = find_vault_root(dir.path());
        assert_eq!(result.unwrap().as_path(), dir.path());
    }

    #[test]
    fn walks_up_from_subdirectory() {
        let dir = tempdir().unwrap();
        make_vault(dir.path());
        let sub = dir.path().join("00-inbox").join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        let result = find_vault_root(&sub);
        assert_eq!(result.unwrap().as_path(), dir.path());
    }

    #[test]
    fn returns_none_when_no_vault_found() {
        let dir = tempdir().unwrap();
        assert!(find_vault_root(dir.path()).is_none());
    }

    #[test]
    fn vault_root_from_path_rejects_non_vault() {
        let dir = tempdir().unwrap();
        let err = VaultRoot::from_path(dir.path()).unwrap_err();
        assert!(matches!(err, CoreError::NotAVault { .. }));
    }

    #[test]
    fn vault_root_name_is_basename() {
        let dir = tempdir().unwrap();
        // Use a stable subdirectory so the assertion isn't sensitive to the
        // random tempdir prefix.
        let vault_dir = dir.path().join("my-vault");
        std::fs::create_dir(&vault_dir).unwrap();
        make_vault(&vault_dir);
        let root = VaultRoot::from_path(&vault_dir).unwrap();
        assert_eq!(root.name(), "my-vault");
    }

    #[test]
    fn resolver_flag_wins_over_env_and_walkup() {
        let flag_dir = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cwd_dir = tempdir().unwrap();
        make_vault(flag_dir.path());
        make_vault(env_dir.path());
        make_vault(cwd_dir.path());

        let resolved = resolve_vault(&VaultResolveInputs {
            flag: Some(flag_dir.path().to_path_buf()),
            env: Some(env_dir.path().to_path_buf()),
            cwd: cwd_dir.path().to_path_buf(),
        })
        .unwrap()
        .unwrap();

        assert_eq!(resolved.source, VaultSource::Flag);
        assert_eq!(resolved.root.as_path(), flag_dir.path());
    }

    #[test]
    fn resolver_env_wins_over_walkup() {
        let env_dir = tempdir().unwrap();
        let cwd_dir = tempdir().unwrap();
        make_vault(env_dir.path());
        make_vault(cwd_dir.path());

        let resolved = resolve_vault(&VaultResolveInputs {
            flag: None,
            env: Some(env_dir.path().to_path_buf()),
            cwd: cwd_dir.path().to_path_buf(),
        })
        .unwrap()
        .unwrap();

        assert_eq!(resolved.source, VaultSource::Env);
        assert_eq!(resolved.root.as_path(), env_dir.path());
    }

    #[test]
    fn resolver_walkup_when_no_flag_or_env() {
        let cwd_dir = tempdir().unwrap();
        make_vault(cwd_dir.path());

        let resolved = resolve_vault(&VaultResolveInputs {
            flag: None,
            env: None,
            cwd: cwd_dir.path().to_path_buf(),
        })
        .unwrap()
        .unwrap();

        assert_eq!(resolved.source, VaultSource::WalkUp);
        assert_eq!(resolved.root.as_path(), cwd_dir.path());
    }

    #[test]
    fn resolver_returns_none_when_nothing_anywhere() {
        let cwd_dir = tempdir().unwrap();
        // No vault.yml anywhere.
        let resolved = resolve_vault(&VaultResolveInputs {
            flag: None,
            env: None,
            cwd: cwd_dir.path().to_path_buf(),
        })
        .unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn require_vault_errors_when_none_found() {
        let cwd_dir = tempdir().unwrap();
        let err = require_vault(&VaultResolveInputs {
            flag: None,
            env: None,
            cwd: cwd_dir.path().to_path_buf(),
        })
        .unwrap_err();
        assert!(matches!(err, CoreError::VaultNotFound { .. }));
        assert_eq!(err.error_code(), "E_VAULT_NOT_FOUND");
    }

    #[test]
    fn resolver_rejects_explicit_flag_when_not_a_vault() {
        let bogus = tempdir().unwrap();
        // No vault.yml at the flag path.
        let cwd_dir = tempdir().unwrap();
        let err = resolve_vault(&VaultResolveInputs {
            flag: Some(bogus.path().to_path_buf()),
            env: None,
            cwd: cwd_dir.path().to_path_buf(),
        })
        .unwrap_err();
        assert!(matches!(err, CoreError::NotAVault { .. }));
    }

    #[test]
    fn vault_source_labels() {
        assert_eq!(VaultSource::Flag.label(), "--vault flag");
        assert_eq!(VaultSource::Env.label(), "ONEBRAIN_VAULT env");
        assert_eq!(VaultSource::WalkUp.label(), "walk-up");
    }
}
