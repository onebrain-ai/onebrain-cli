//! Machine-local scheduler log directory — the #315 fix.
//!
//! Scheduler job logs used to live inside the vault (`07-logs/scheduler/`).
//! On an iCloud-synced vault those files eventually reach a state launchd
//! cannot open for append (`com.apple.decmpfs` transparent compression was the
//! confirmed mechanism). launchd opens `StandardOutPath`/`StandardErrorPath`
//! **before exec**, so the job died during setup with `EX_CONFIG` (78) and
//! wrote nothing anywhere — the channel that would have reported the failure
//! was the thing that failed.
//!
//! Job logs are machine-local operational output, not vault content. They do
//! not sync, so they live under each platform's state directory instead.

use std::path::{Component, Path, PathBuf};

/// Machine-local directory for scheduler job logs. Never inside the vault.
///
/// | OS      | destination                                              |
/// |---------|----------------------------------------------------------|
/// | macOS   | `~/Library/Logs/onebrain`                                |
/// | Linux   | `$XDG_STATE_HOME/onebrain/log` → `~/.local/state/onebrain/log` |
/// | Windows | `%LOCALAPPDATA%\onebrain\logs` → `<home>\AppData\Local\onebrain\logs` |
///
/// `env` is injected rather than read from the process so the `XDG_STATE_HOME`
/// and `LOCALAPPDATA` branches are testable on any host. (On macOS the closure
/// is unused — the path is fixed by convention.)
pub fn default_log_dir(homedir: &Path, env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let _ = env;
        homedir.join("Library/Logs/onebrain")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        match env("XDG_STATE_HOME") {
            Some(state) if !state.is_empty() => PathBuf::from(state).join("onebrain/log"),
            _ => homedir.join(".local/state/onebrain/log"),
        }
    }
    #[cfg(windows)]
    {
        match env("LOCALAPPDATA") {
            Some(local) if !local.is_empty() => PathBuf::from(local).join("onebrain").join("logs"),
            _ => homedir
                .join("AppData")
                .join("Local")
                .join("onebrain")
                .join("logs"),
        }
    }
}

/// True when a configured log base still points inside the vault — the #315
/// shape that `register` migrates away from.
///
/// Compares path **components**, never raw string prefixes: `/v/ob-1-backup`
/// must not read as inside `/v/ob-1`.
pub fn is_in_vault(log_base: &Path, vault: &Path) -> bool {
    let base: Vec<Component> = log_base.components().collect();
    let root: Vec<Component> = vault.components().collect();
    base.len() >= root.len() && base[..root.len()] == root[..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_uses_library_logs() {
        assert_eq!(
            default_log_dir(Path::new("/Users/u"), &no_env),
            PathBuf::from("/Users/u/Library/Logs/onebrain")
        );
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn linux_prefers_xdg_state_home() {
        let env = |k: &str| (k == "XDG_STATE_HOME").then(|| "/home/u/.state".to_string());
        assert_eq!(
            default_log_dir(Path::new("/home/u"), &env),
            PathBuf::from("/home/u/.state/onebrain/log")
        );
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn linux_falls_back_when_xdg_unset() {
        assert_eq!(
            default_log_dir(Path::new("/home/u"), &no_env),
            PathBuf::from("/home/u/.local/state/onebrain/log")
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_prefers_localappdata() {
        let env = |k: &str| (k == "LOCALAPPDATA").then(|| r"C:\Users\u\AppData\Local".to_string());
        assert_eq!(
            default_log_dir(Path::new(r"C:\Users\u"), &env),
            PathBuf::from(r"C:\Users\u\AppData\Local\onebrain\logs")
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_falls_back_when_localappdata_unset() {
        assert_eq!(
            default_log_dir(Path::new(r"C:\Users\u"), &no_env),
            PathBuf::from(r"C:\Users\u\AppData\Local\onebrain\logs")
        );
    }

    #[test]
    fn a_log_base_inside_the_vault_is_detected() {
        let vault = Path::new("/v/ob-1");
        assert!(is_in_vault(Path::new("/v/ob-1/07-logs/scheduler"), vault));
        assert!(is_in_vault(vault, vault), "the vault root itself counts");
        assert!(!is_in_vault(
            Path::new("/Users/u/Library/Logs/onebrain"),
            vault
        ));
    }

    #[test]
    fn a_sibling_directory_is_not_inside_the_vault() {
        // The decisive case: a naive string starts_with passes every other
        // test in this module and fails this one.
        assert!(!is_in_vault(
            Path::new("/v/ob-1-backup/logs"),
            Path::new("/v/ob-1")
        ));
    }
}
