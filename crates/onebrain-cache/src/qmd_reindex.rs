//! qmd-reindex command · spawns detached `qmd update -c <collection>` background process.

use onebrain_core::load_vault_config_at;
use std::io::Write;
use std::path::Path;

/// Target OS identifier matching `std::env::consts::OS` values.
/// Use this for cross-platform spawn-args branching · tests inject specific values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOs {
    Unix,
    Windows,
}

impl SpawnOs {
    /// Resolve from `std::env::consts::OS` ("linux", "macos", "windows", etc.).
    pub fn from_env() -> Self {
        match std::env::consts::OS {
            "windows" => SpawnOs::Windows,
            _ => SpawnOs::Unix, // macOS, Linux, BSD, etc.
        }
    }
}

/// Build the spawn args for `qmd update -c <collection>`.
///
/// On Windows, routes through PowerShell because `std::process::Command` cannot
/// invoke `.cmd`/`.ps1` scripts directly. The collection is single-quoted as a
/// PowerShell literal string; embedded single quotes are escaped by doubling
/// (`''`) — matches Bun's quoting at `qmd-reindex.ts:27-28`.
pub fn build_qmd_spawn_args(collection: &str, os: SpawnOs) -> Vec<String> {
    match os {
        SpawnOs::Unix => vec![
            "qmd".to_string(),
            "update".to_string(),
            "-c".to_string(),
            collection.to_string(),
        ],
        SpawnOs::Windows => {
            let safe = collection.replace('\'', "''");
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                format!("qmd update -c '{safe}'"),
            ]
        }
    }
}

/// Run qmd-reindex · spawns `qmd update -c <collection>` in a detached background
/// process. Always returns `Ok(())` · matches Bun's "exit 0 fire-and-forget" contract.
///
/// Silent skip when:
/// - vault.yml is missing or malformed (any `load_vault_config_at` Err)
/// - `qmd_collection` field is absent or empty string (Bun JS-truthiness parity)
///
/// On spawn failure: writes `qmd-reindex: <error>` to stderr, still returns `Ok(())`.
///
/// The `spawn_fn` closure is injected for testability — production callers pass a
/// closure that calls `std::process::Command::spawn` with platform-specific detach
/// flags. Tests pass a recorder closure.
pub fn qmd_reindex<F>(vault_root: &Path, os: SpawnOs, spawn_fn: F) -> std::io::Result<()>
where
    F: FnOnce(&[String]) -> std::io::Result<()>,
{
    let config = match load_vault_config_at(vault_root) {
        Ok(c) => c,
        Err(_) => return Ok(()), // silent · matches Bun catch-all (line 53)
    };
    // Use filter to treat empty string the same as None (Bun JS-truthiness: `if (!collection)`).
    let Some(collection) = config.qmd_collection.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(()); // silent · no collection configured
    };
    let args = build_qmd_spawn_args(collection, os);
    if let Err(e) = spawn_fn(&args) {
        let _ = writeln!(std::io::stderr(), "qmd-reindex: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- SpawnOs::from_env() ---

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn from_env_returns_unix_on_non_windows() {
        // Covers the `_ => SpawnOs::Unix` arm of from_env().
        assert_eq!(SpawnOs::from_env(), SpawnOs::Unix);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn from_env_returns_windows_on_windows() {
        // Covers the `"windows" => SpawnOs::Windows` arm of from_env().
        assert_eq!(SpawnOs::from_env(), SpawnOs::Windows);
    }

    // --- build_qmd_spawn_args ---

    #[test]
    fn build_args_unix_returns_direct_qmd() {
        let args = build_qmd_spawn_args("my-collection", SpawnOs::Unix);
        assert_eq!(args, vec!["qmd", "update", "-c", "my-collection"]);
    }

    #[test]
    fn build_args_windows_uses_powershell() {
        let args = build_qmd_spawn_args("my-collection", SpawnOs::Windows);
        assert_eq!(args[0], "powershell.exe");
        assert_eq!(args[1], "-NoProfile");
        assert_eq!(args[2], "-Command");
        assert!(args[3].starts_with("qmd update -c '"));
        assert!(args[3].contains("my-collection"));
        assert!(args[3].ends_with("'"));
    }

    #[test]
    fn build_args_windows_doubles_embedded_single_quotes() {
        // Bun parity: `col'lection` becomes `col''lection` inside single-quoted PS literal.
        let args = build_qmd_spawn_args("col'lection", SpawnOs::Windows);
        assert!(args[3].contains("col''lection"));
    }
}

#[cfg(test)]
mod entry_tests {
    use super::*;
    use std::cell::RefCell;

    /// A spawn recorder that captures calls without actually spawning a process.
    struct Recorder {
        calls: RefCell<Vec<Vec<String>>>,
        force_error: bool,
    }
    impl Recorder {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                force_error: false,
            }
        }
        fn with_error() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                force_error: true,
            }
        }
        fn spawn(&self, args: &[String]) -> std::io::Result<()> {
            self.calls.borrow_mut().push(args.to_vec());
            if self.force_error {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "qmd not in PATH",
                ))
            } else {
                Ok(())
            }
        }
        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
        fn last_args(&self) -> Vec<String> {
            self.calls.borrow().last().cloned().unwrap_or_default()
        }
    }

    #[test]
    fn no_spawn_when_vault_yml_missing() {
        let dir = tempfile::tempdir().unwrap();
        // No vault.yml inside dir.path()
        let rec = Recorder::new();
        let result = qmd_reindex(dir.path(), SpawnOs::Unix, |args| rec.spawn(args));
        assert!(result.is_ok());
        assert_eq!(rec.call_count(), 0);
    }

    #[test]
    fn no_spawn_when_qmd_collection_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "# no qmd_collection\n").unwrap();
        let rec = Recorder::new();
        let result = qmd_reindex(dir.path(), SpawnOs::Unix, |args| rec.spawn(args));
        assert!(result.is_ok());
        assert_eq!(rec.call_count(), 0);
    }

    #[test]
    fn no_spawn_when_qmd_collection_is_empty_string() {
        // Bun parity: `if (!collection)` treats empty string as falsy → no spawn.
        // Rust's `.filter(|s| !s.is_empty())` mirrors this · pin the contract.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: \"\"\n").unwrap();
        let rec = Recorder::new();
        let result = qmd_reindex(dir.path(), SpawnOs::Unix, |args| rec.spawn(args));
        assert!(result.is_ok());
        assert_eq!(rec.call_count(), 0);
    }

    #[test]
    fn spawns_with_correct_args_unix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("vault.yml"),
            "qmd_collection: my-collection\n",
        )
        .unwrap();
        let rec = Recorder::new();
        qmd_reindex(dir.path(), SpawnOs::Unix, |args| rec.spawn(args)).unwrap();
        assert_eq!(rec.call_count(), 1);
        assert_eq!(
            rec.last_args(),
            vec!["qmd", "update", "-c", "my-collection"]
        );
    }

    #[test]
    fn spawns_with_correct_args_windows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("vault.yml"),
            "qmd_collection: my-collection\n",
        )
        .unwrap();
        let rec = Recorder::new();
        qmd_reindex(dir.path(), SpawnOs::Windows, |args| rec.spawn(args)).unwrap();
        let args = rec.last_args();
        assert_eq!(args[0], "powershell.exe");
        assert_eq!(args[3], "qmd update -c 'my-collection'");
    }

    #[test]
    fn returns_ok_when_spawn_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: any\n").unwrap();
        let rec = Recorder::with_error();
        let result = qmd_reindex(dir.path(), SpawnOs::Unix, |args| rec.spawn(args));
        assert!(
            result.is_ok(),
            "spawn failure must not propagate · matches Bun catch-all"
        );
        // The spawn was attempted (and recorded) before failure.
        assert_eq!(rec.call_count(), 1);
    }

    #[test]
    fn returns_ok_when_vault_yml_malformed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "not: : valid yaml").unwrap();
        let rec = Recorder::new();
        // Bun line 53-55: `catch (err) { stderr.write(...) }` swallows everything · we mirror.
        let result = qmd_reindex(dir.path(), SpawnOs::Unix, |args| rec.spawn(args));
        assert!(result.is_ok());
        assert_eq!(rec.call_count(), 0);
    }
}
