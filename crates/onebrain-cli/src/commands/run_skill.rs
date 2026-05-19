//! `onebrain run-skill` — spawn `claude -p "<prompt>" --add-dir <vault>` with
//! the vault as `cwd`, inheriting the parent process env (so PATH/HOME
//! survive for Homebrew lookups). Used by the launchd scheduler to dispatch
//! OneBrain skills headlessly.
//!
//! Exit codes mirror Bun's `runSkillCommand`:
//!
//! - `78` (EX_CONFIG) — vault.yml missing
//! - `127` — spawn failed (e.g. claude not on disk)
//! - `128 + signal` — child terminated by signal (Unix only)
//! - any other code — propagated from child verbatim
//! - `1` — fallback when child exited with no code and no signal

use anyhow::{anyhow, Context, Result};
use onebrain_fs::{build_prompt, resolve_claude_bin};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Entry point invoked from `main.rs`. Returns the exit code the binary
/// should call `std::process::exit` with.
pub fn run(vault: &str, skill: &str, args: &[String]) -> Result<i32> {
    let vault_path = PathBuf::from(vault);
    if !vault_path.join("vault.yml").is_file() {
        eprintln!("Vault not found at {vault} (no vault.yml present)");
        return Ok(78); // EX_CONFIG (sysexits.h)
    }

    let pairs = parse_args(args)?;
    let prompt = build_prompt(skill, &pairs).map_err(|e| anyhow!(e))?;

    let resolution = resolve_claude_bin(
        None,
        |k| std::env::var(k).ok(),
        |p| p.exists(),
        std::env::var("HOME").ok().as_deref(),
    );
    if let Some(warning) = &resolution.warning {
        eprintln!("{warning}");
    }
    let claude_bin = resolution.path;

    spawn_claude(&claude_bin, &prompt, &vault_path)
}

/// Convert clap's `Vec<String>` of `key=value` tokens into ordered pairs.
/// Reject malformed entries early — the scheduler is the only caller and we
/// want loud failure rather than silent passthrough.
fn parse_args(raw: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(raw.len());
    for token in raw {
        let (k, v) = token
            .split_once('=')
            .with_context(|| format!("--arg expects key=value, got: {token}"))?;
        if k.is_empty() {
            return Err(anyhow!("--arg has empty key: {token}"));
        }
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

fn spawn_claude(claude_bin: &Path, prompt: &str, vault: &Path) -> Result<i32> {
    // No `env` override: child inherits parent env so PATH/HOME survive.
    // stdio is inherited by default with `std::process::Command::status()`.
    let status = Command::new(claude_bin)
        .arg("-p")
        .arg(prompt)
        .arg("--add-dir")
        .arg(vault)
        .current_dir(vault)
        .status();

    match status {
        Ok(s) => Ok(translate_exit(&s)),
        Err(e) => {
            eprintln!("Failed to spawn claude ({}): {e}", claude_bin.display());
            Ok(127)
        }
    }
}

#[cfg(unix)]
fn translate_exit(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(sig) = status.signal() {
        // POSIX convention: 128 + signal number. SIGTERM (15) -> 143, SIGKILL (9) -> 137.
        return 128 + sig;
    }
    status.code().unwrap_or(1)
}

#[cfg(not(unix))]
fn translate_exit(status: &std::process::ExitStatus) -> i32 {
    // Windows has no POSIX signals · just propagate the code or fall back to 1.
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_empty_returns_empty() {
        assert_eq!(parse_args(&[]).unwrap(), Vec::<(String, String)>::new());
    }

    #[test]
    fn parse_args_preserves_insertion_order() {
        let raw = vec![
            "first=1".to_string(),
            "second=2".to_string(),
            "third=3".to_string(),
        ];
        let parsed = parse_args(&raw).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("first".to_string(), "1".to_string()),
                ("second".to_string(), "2".to_string()),
                ("third".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn parse_args_allows_empty_value() {
        // `topic=` is valid · the skill may interpret an empty value.
        let raw = vec!["topic=".to_string()];
        assert_eq!(
            parse_args(&raw).unwrap(),
            vec![("topic".to_string(), "".to_string())]
        );
    }

    #[test]
    fn parse_args_allows_equals_in_value() {
        let raw = vec!["filter=a=b".to_string()];
        assert_eq!(
            parse_args(&raw).unwrap(),
            vec![("filter".to_string(), "a=b".to_string())]
        );
    }

    #[test]
    fn parse_args_rejects_missing_equals() {
        let err = parse_args(&["badtoken".to_string()]).unwrap_err();
        assert!(err.to_string().contains("key=value"));
    }

    #[test]
    fn parse_args_rejects_empty_key() {
        let err = parse_args(&["=value".to_string()]).unwrap_err();
        assert!(err.to_string().contains("empty key"));
    }
}
