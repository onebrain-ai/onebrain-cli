//! `onebrain run-skill` — spawn `claude -p "<prompt>" --add-dir <vault>` with
//! the vault as `cwd`, inheriting the parent process env (so PATH/HOME
//! survive for Homebrew lookups). Used by the launchd scheduler to dispatch
//! OneBrain skills headlessly, and runnable manually from inside a vault.
//!
//! The child's stdin is redirected from `/dev/null`: `claude -p` appends any
//! piped stdin to the prompt, so an inherited interactive TTY (no EOF) makes
//! it block forever. launchd already gives the child a null stdin, but a
//! manual terminal run does not — so we set it explicitly to keep both paths
//! non-blocking. stdout/stderr stay inherited so the user sees claude's output.
//!
//! Exit codes mirror Bun's `runSkillCommand`:
//!
//! - `78` (EX_CONFIG) — no OneBrain config (onebrain.yml / vault.yml) present
//! - `127` — couldn't run/track the child (spawn failed, e.g. claude not on
//!   disk; or a fault while waiting on it)
//! - `128 + signal` — child terminated by signal (Unix only)
//! - any other code — propagated from child verbatim
//! - `1` — fallback when child exited with no code and no signal

use anyhow::{anyhow, Context, Result};
use onebrain_core::find_config_file;
use onebrain_fs::{build_prompt, resolve_claude_bin};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Entry point invoked from `main.rs`. Returns the exit code the binary
/// should call `std::process::exit` with.
pub fn run(vault: &str, skill: &str, args: &[String]) -> Result<i32> {
    let vault_path = PathBuf::from(vault);
    if find_config_file(&vault_path).is_none() {
        eprintln!("Vault not found at {vault} (no onebrain.yml present)");
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
    use std::io::IsTerminal;
    // No `env` override: child inherits parent env so PATH/HOME survive.
    // stdout/stderr stay inherited so claude's output (and colour) reach the
    // terminal verbatim; stdin is forced to null so `claude -p` never blocks
    // reading an interactive TTY — see module docs.
    let mut command = Command::new(claude_bin);
    command
        .arg("-p")
        .arg(prompt)
        .arg("--add-dir")
        .arg(vault)
        .current_dir(vault)
        .stdin(Stdio::null());

    // Non-interactive (launchd scheduler, piped, CI): block and let the
    // captured StandardOutPath / pipe collect the output. No progress lines —
    // they'd only litter a log file.
    if !std::io::stderr().is_terminal() {
        return match command.status() {
            Ok(s) => Ok(translate_exit(&s)),
            Err(e) => {
                eprintln!("Failed to spawn claude ({}): {e}", claude_bin.display());
                Ok(127)
            }
        };
    }

    // Interactive: `claude -p` runs a full headless agent session (plugin load,
    // SessionStart hook, the skill's own work, LLM inference) and buffers its
    // result until done — often tens of seconds. Spawn it non-blocking and emit
    // an elapsed heartbeat on stderr so the terminal doesn't look frozen. We
    // use plain stderr lines (not a `\r` spinner) so they never clobber
    // claude's stdout output when it finally flushes.
    eprintln!("▶ Running {prompt} headlessly via claude — output appears when it completes.");
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to spawn claude ({}): {e}", claude_bin.display());
            return Ok(127);
        }
    };
    let started = std::time::Instant::now();
    let mut next_beat = std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(translate_exit(&status)),
            Ok(None) => {
                if started.elapsed() >= next_beat {
                    eprintln!("  … still running ({}s)", started.elapsed().as_secs());
                    next_beat += std::time::Duration::from_secs(10);
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(e) => {
                // Couldn't observe the child — a harness/OS fault, not a skill
                // failure, so use 127 (the "couldn't run it properly" class)
                // rather than 1 (which reads as "the skill ran and failed").
                // kill+wait reaps the child; this may truncate any output it was
                // mid-flush on, but we can no longer track it reliably.
                eprintln!("waiting on claude failed: {e}");
                let _ = child.kill();
                let _ = child.wait();
                return Ok(127);
            }
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

    /// Exercises the spawn path + exit-code translation with `claude` stubbed
    /// by a script that exits 43 on EOF / 42 after reading a line.
    ///
    /// NOTE: under `cargo test` the harness's own stdin is already at EOF, so
    /// this cannot *distinguish* a null stdin from an inherited one (both yield
    /// 43) — it pins the EOF→exit-43 contract, not the `.stdin(null)` line
    /// itself. The real fix (an interactive TTY no longer hangs `claude -p`) is
    /// verified manually; a hermetic guard would need to inject a live parent
    /// stdin, which `spawn_claude` (it reads the process's fd 0) can't take
    /// without an API change made solely for the test.
    #[cfg(unix)]
    #[test]
    fn spawn_claude_gives_child_a_non_blocking_stdin() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("claude_stub.sh");
        {
            let mut f = std::fs::File::create(&stub).unwrap();
            writeln!(
                f,
                "#!/bin/sh\nif IFS= read -r _; then exit 42; else exit 43; fi"
            )
            .unwrap();
        }
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let code = spawn_claude(&stub, "/daily", dir.path()).unwrap();
        assert_eq!(code, 43, "child should see EOF on a null stdin, not block");
    }
}
