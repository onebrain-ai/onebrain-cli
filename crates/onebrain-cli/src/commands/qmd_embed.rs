//! `onebrain qmd embed` — generate/refresh vector embeddings via `qmd embed`.
//!
//! Vault-required (exit 64 outside a vault · parity with the other qmd verbs).
//! Runs `qmd embed` in the FOREGROUND, inheriting stdio so the user watches
//! qmd's progress, then surfaces a non-zero `qmd` exit as an error. Contrast
//! `qmd reindex`, which spawns `qmd update` detached for the PostToolUse hook.

use crate::vault_ctx;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub fn run(vault_flag: Option<PathBuf>) -> Result<()> {
    // Vault-required: exit 64 (E_VAULT_NOT_FOUND) outside a vault.
    vault_ctx::require(vault_flag)?;

    // Foreground · `status()` inherits stdin/stdout/stderr, so qmd's progress
    // streams straight to the user; then propagate qmd's exit status.
    let status = build_qmd_embed_command()
        .status()
        .context("run `qmd embed` — is the `qmd` binary installed and on PATH?")?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "qmd embed failed ({})",
            status
                .code()
                .map(|c| format!("exit {c}"))
                .unwrap_or_else(|| "terminated by signal".to_string())
        ))
    }
}

/// Build the `qmd embed` invocation, platform-wrapped. On Windows qmd ships as
/// a `.cmd`/`.ps1` shim that can't be spawned directly, so route through
/// `powershell.exe` (mirrors `onebrain-cache::qmd`).
#[cfg(windows)]
fn build_qmd_embed_command() -> Command {
    let mut c = Command::new("powershell.exe");
    c.args(["-NoProfile", "-Command", "qmd embed"]);
    c
}

#[cfg(not(windows))]
fn build_qmd_embed_command() -> Command {
    let mut c = Command::new("qmd");
    c.arg("embed");
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_qmd_embed_invocation() {
        let cmd = build_qmd_embed_command();
        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        #[cfg(windows)]
        {
            assert_eq!(program, "powershell.exe");
            assert!(
                args.iter().any(|a| a.contains("qmd embed")),
                "args: {args:?}"
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(program, "qmd");
            assert_eq!(args, vec!["embed"]);
        }
    }
}
