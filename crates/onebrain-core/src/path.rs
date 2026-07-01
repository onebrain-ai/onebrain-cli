use crate::error::CoreError;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Canonical OneBrain config filename (v3.1+).
pub const CONFIG_FILENAME: &str = "onebrain.yml";
/// Legacy OneBrain config filename (v3.0 and earlier). Still readable for
/// back-compat in v3.1.x · removed in v4.0.0.
pub const LEGACY_CONFIG_FILENAME: &str = "vault.yml";

/// Suppress the legacy-filename deprecation warning. Honoured by
/// `deprecation_warning_once`.
const ENV_QUIET_DEPRECATION: &str = "ONEBRAIN_QUIET_VAULT_YML_DEPRECATION";

/// Process-global flag — guarantees the deprecation warning fires at most
/// once per process even when many subsystems traverse `find_config_file`.
static LEGACY_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);

/// Locate the active config file in `dir`. Prefers the canonical
/// `onebrain.yml`; falls back to legacy `vault.yml` and emits a one-time
/// stderr deprecation warning. Returns `None` when neither is present.
///
/// Centralises every `<dir>/vault.yml` lookup the CLI used to perform
/// inline. Callers that need the in-memory parse should still use
/// [`crate::load_vault_config`] / [`crate::load_vault_config_at`] which
/// both route through this helper.
pub fn find_config_file(dir: &Path) -> Option<PathBuf> {
    let canonical = dir.join(CONFIG_FILENAME);
    if canonical.is_file() {
        return Some(canonical);
    }
    let legacy = dir.join(LEGACY_CONFIG_FILENAME);
    if legacy.is_file() {
        emit_legacy_deprecation_warning_once();
        return Some(legacy);
    }
    None
}

/// Emit the `vault.yml` deprecation warning to stderr — at most once per
/// process. Suppressed entirely when `ONEBRAIN_QUIET_VAULT_YML_DEPRECATION=1`.
///
/// Library-side warning so every consumer (CLI, cache crate, fs crate)
/// gets the same single emission point. CLI envelope/JSON modes that want
/// to surface the warning structurally can check
/// [`legacy_warning_was_emitted`] and emit a `Warning { code:
/// W_VAULT_YML_DEPRECATED }` themselves; the stderr line is just the
/// human-mode fallback.
pub fn emit_legacy_deprecation_warning_once() {
    if std::env::var(ENV_QUIET_DEPRECATION)
        .ok()
        .as_deref()
        .is_some_and(|v| v == "1" || v == "true")
    {
        // Still mark as emitted so structured-output callers can pick it
        // up via `legacy_warning_was_emitted` without a parallel state
        // variable. The user asked to quiet the stderr line, not to hide
        // the underlying signal.
        LEGACY_WARNING_EMITTED.store(true, Ordering::Relaxed);
        return;
    }
    if !LEGACY_WARNING_EMITTED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "warning: vault.yml is deprecated — run `onebrain doctor --fix` to migrate to onebrain.yml (removed in v4.0.0)"
        );
    }
}

/// True when [`emit_legacy_deprecation_warning_once`] has fired in this
/// process. Used by structured-output callers to surface a `W_VAULT_YML_DEPRECATED`
/// warning in their envelope alongside (or in place of) the stderr line.
pub fn legacy_warning_was_emitted() -> bool {
    LEGACY_WARNING_EMITTED.load(Ordering::Relaxed)
}

/// Test-only reset of the deprecation-warning sentinel. Lets unit tests
/// observe the "fires exactly once" contract without leaking state across
/// `#[test]` runs.
#[doc(hidden)]
pub fn reset_legacy_warning_for_test() {
    LEGACY_WARNING_EMITTED.store(false, Ordering::Relaxed);
}

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

    /// Path to the active config file (`onebrain.yml` preferred, else
    /// `vault.yml`). Returns `None` when neither exists, which only
    /// happens if the file was deleted after `from_path` / `find_vault_root`
    /// succeeded.
    pub fn config_path(&self) -> Option<PathBuf> {
        find_config_file(&self.0)
    }

    /// Construct a `VaultRoot` from an explicit path. Validates that
    /// either `onebrain.yml` or `vault.yml` exists in the directory.
    /// Emits a one-time deprecation warning on legacy hit. Returns
    /// [`CoreError::NotAVault`] when the path is not a vault root.
    pub fn from_path(path: &Path) -> crate::error::Result<Self> {
        if find_config_file(path).is_some() {
            Ok(Self(path.to_path_buf()))
        } else {
            Err(CoreError::NotAVault {
                path: path.to_path_buf(),
            })
        }
    }
}

/// Walk up from `start` looking for the nearest directory containing
/// either `onebrain.yml` (canonical) or `vault.yml` (legacy, with
/// deprecation warning). Returns `None` if none is found before the
/// filesystem root.
///
/// **Resolution ≠ validation.** This function only checks that the file
/// exists (`Path::is_file()` returns true for readable regular files and
/// follows symlinks to a regular file). It does NOT parse the file or
/// verify it's valid YAML. Broken symlinks, permission-denied reads, and
/// malformed YAML are all the caller's responsibility:
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
        if find_config_file(&current).is_some() {
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
    /// Resolved by walking up from `$PWD` finding `onebrain.yml` (or
    /// legacy `vault.yml`).
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
/// When a flag/env path is supplied but doesn't contain an `onebrain.yml`
/// (or legacy `vault.yml`), returns [`CoreError::NotAVault`] — explicit
/// "I told you to use this vault but it's not actually one" rather than
/// silently falling through to walk-up. This avoids the surprise where a
/// user typos `--vault /tmp/wrong` and ends up operating on whatever
/// vault happens to be above cwd.
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
        std::fs::write(dir.join(CONFIG_FILENAME), "").unwrap();
    }

    fn make_legacy_vault(dir: &Path) {
        std::fs::write(dir.join(LEGACY_CONFIG_FILENAME), "").unwrap();
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
        // No vault config (neither onebrain.yml nor vault.yml) anywhere.
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
        // No vault config at the flag path.
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

    // ─── v3.1 dual-read transition ─────────────────────────────────────────

    #[test]
    fn find_vault_root_prefers_onebrain_yml_over_vault_yml() {
        let dir = tempdir().unwrap();
        // Both files present — onebrain.yml wins.
        std::fs::write(dir.path().join(CONFIG_FILENAME), "").unwrap();
        std::fs::write(dir.path().join(LEGACY_CONFIG_FILENAME), "").unwrap();

        let root = find_vault_root(dir.path()).unwrap();
        // VaultRoot's `config_path` MUST report the canonical filename when
        // both exist — proves the preference, not just that the walk-up
        // hit the directory.
        let cfg = root.config_path().unwrap();
        assert_eq!(cfg.file_name().unwrap(), CONFIG_FILENAME);
    }

    /// Serializes the two tests that mutate the process-global
    /// `LEGACY_WARNING_EMITTED` sentinel (both `reset` then `assert`). Without
    /// it, parallel test execution lets one test's `reset` flip the flag off
    /// between the other's emit and its assert — an order-dependent flake
    /// surfaced under the coverage run's test scheduling.
    static SENTINEL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn find_vault_root_falls_back_to_vault_yml_with_warning() {
        let _sentinel_guard = SENTINEL_TEST_LOCK.lock().unwrap();
        reset_legacy_warning_for_test();
        let dir = tempdir().unwrap();
        make_legacy_vault(dir.path());

        // Suppress the stderr line in tests; the sentinel still flips so
        // we can assert the fallback fired.
        std::env::set_var(ENV_QUIET_DEPRECATION, "1");
        let root = find_vault_root(dir.path()).unwrap();
        let cfg = root.config_path().unwrap();
        std::env::remove_var(ENV_QUIET_DEPRECATION);

        assert_eq!(cfg.file_name().unwrap(), LEGACY_CONFIG_FILENAME);
        assert!(
            legacy_warning_was_emitted(),
            "deprecation sentinel did not flip on legacy fallback"
        );
    }

    #[test]
    fn deprecation_warning_emits_once_per_process() {
        let _sentinel_guard = SENTINEL_TEST_LOCK.lock().unwrap();
        reset_legacy_warning_for_test();
        let dir = tempdir().unwrap();
        make_legacy_vault(dir.path());

        // Quiet the stderr line — we only care about the sentinel + that
        // it's idempotent across multiple resolution attempts.
        std::env::set_var(ENV_QUIET_DEPRECATION, "1");
        let _ = find_vault_root(dir.path()).unwrap();
        assert!(legacy_warning_was_emitted());
        // Second call must not toggle the sentinel back off; the
        // `swap` inside `emit_legacy_deprecation_warning_once` makes the
        // first emit set it to true and subsequent calls observe true and
        // return without re-emitting.
        let _ = find_vault_root(dir.path()).unwrap();
        assert!(legacy_warning_was_emitted());
        std::env::remove_var(ENV_QUIET_DEPRECATION);
    }

    #[test]
    fn find_config_file_returns_none_when_neither_present() {
        let dir = tempdir().unwrap();
        assert!(find_config_file(dir.path()).is_none());
    }
}
