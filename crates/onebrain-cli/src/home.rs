//! Home-directory resolution that actually observes the `$HOME` /
//! `%USERPROFILE%` environment override on EVERY platform.
//!
//! `dirs::home_dir()` does not: on Unix it reads `$HOME` (dirs-sys
//! `home_dir()` → `env::var_os("HOME")`), but on Windows it is
//! `dirs_sys::known_folder_profile()` → `SHGetKnownFolderPath(FOLDERID_Profile)`
//! — a Win32 shell call that asks the OS for the *real* account profile and
//! ignores `%USERPROFILE%` entirely (verified against dirs 5.0.1 /
//! dirs-sys 0.4.1, the versions this workspace pins).
//!
//! That difference is invisible in production — Windows sets `%USERPROFILE%`
//! to the same path the Known Folder API returns — but it silently breaks
//! every sandboxed child process: integration tests spawn the real binary
//! with `HOME`/`USERPROFILE` pointed at a tempdir so `~/.onebrain/...` never
//! touches the developer's or CI runner's real home, and on Windows the child
//! walked straight past the sandbox into the runner's actual profile.
//!
//! Use this helper for anything under `~/.onebrain` (or any other path a test
//! may need to redirect). `dirs::home_dir()` stays correct for the
//! `#[cfg(unix)]`-gated call sites that already document the limitation.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// The user's home directory, preferring the `%USERPROFILE%` env var so a
/// test-set (or deliberately overridden) home is honoured, and falling back
/// to `dirs::home_dir()`'s Known Folder lookup when it is unset or empty.
#[cfg(windows)]
pub(crate) fn home_dir() -> Result<PathBuf> {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.trim().is_empty() {
            return Ok(PathBuf::from(profile));
        }
    }
    dirs::home_dir().context("resolve home directory (USERPROFILE unset and no fallback)")
}

/// The user's home directory. `dirs::home_dir()` already reads `$HOME` first
/// on Unix, so no extra probe is needed here.
#[cfg(not(windows))]
pub(crate) fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("resolve home directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the helper: the env override wins on every
    /// platform. `HOME` steers the Unix path, `USERPROFILE` the Windows one —
    /// set both under the shared `test_env` lock so this single test proves
    /// the contract wherever it runs.
    #[test]
    fn env_override_wins_on_every_platform() {
        let d = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("HOME", d.path().as_os_str()),
            ("USERPROFILE", d.path().as_os_str()),
        ]);
        // Compare canonicalized: macOS `tempdir()` hands back `/var/...`,
        // which is a symlink to `/private/var/...`.
        assert_eq!(
            home_dir().unwrap().canonicalize().unwrap(),
            d.path().canonicalize().unwrap(),
        );
    }

    /// `dirs::home_dir()` — the fallback — must still be reachable, i.e. an
    /// empty override is treated as "unset", not as an empty home path.
    #[test]
    fn empty_override_falls_back_instead_of_yielding_an_empty_path() {
        let _env =
            crate::test_env::set_vars(&[("HOME", "".as_ref()), ("USERPROFILE", "".as_ref())]);
        if let Ok(home) = home_dir() {
            assert!(
                !home.as_os_str().is_empty(),
                "an empty override must never resolve to an empty home path"
            );
        }
    }
}
