//! qmd-reindex command · spawns detached `qmd update -c <collection>` background process.

/// Target OS identifier matching `std::env::consts::OS` values.
/// Use this for cross-platform spawn-args branching · tests inject specific values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOs {
    Unix,
    Windows,
}

impl SpawnOs {
    /// Resolve from `std::env::consts::OS` ("linux", "macos", "windows", etc.).
    #[allow(dead_code)] // used by upcoming commands::qmd_reindex in Task 3
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
#[allow(dead_code)] // used by upcoming qmd_reindex entry in Task 2
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

#[cfg(test)]
mod tests {
    use super::*;

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
