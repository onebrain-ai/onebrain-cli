//! `onebrain skill run` — spawn a headless agent harness on a OneBrain skill
//! with the vault as `cwd`, inheriting the parent process env (so PATH/HOME
//! survive for Homebrew lookups) with the `onebrain` binary's own directory
//! prepended to PATH (see [`child_path_with_exe_dir`] — under launchd the
//! parent has a minimal PATH, so this keeps nested `onebrain` calls from the
//! skill's own hooks resolving; #124). Used by the launchd scheduler to
//! dispatch OneBrain skills headlessly, and runnable manually from inside a
//! vault.
//!
//! Harnesses (`--harness`, default `claude`):
//!   - claude → `claude -p "<prompt>" --add-dir <vault> [--model <m>]`
//!   - gemini → `gemini -p "<prompt>" --include-directories <vault>
//!     --approval-mode yolo [-m <m>]` (yolo so a skill that runs `onebrain`
//!     shell commands or writes files isn't blocked on an approval prompt in
//!     an unattended run — same trust model as `claude -p` under the
//!     scheduler's settings allow-list).
//!
//! `ONEBRAIN_HEADLESS=1` is set on the child so `onebrain session init`
//! reports `headless: true`, which lets INSTRUCTIONS.md skip the interactive
//! startup ceremony (greeting + status + memory/inbox/task/orphan scans) and
//! go straight to the requested skill.
//!
//! The child's stdin is redirected from `/dev/null`: `claude -p` appends any
//! piped stdin to the prompt, so an inherited interactive TTY (no EOF) makes
//! it block forever. launchd already gives the child a null stdin, but a
//! manual terminal run does not — so we set it explicitly to keep both paths
//! non-blocking. stdout/stderr stay inherited so the user sees the output.
//!
//! Exit codes mirror Bun's `runSkillCommand`:
//!
//! - `78` (EX_CONFIG) — no OneBrain config (onebrain.yml / vault.yml) present.
//!   NOTE (#263): both CLI entry points (`skill run` and the legacy
//!   `run-skill` alias) now pre-resolve the vault via `vault_ctx::require`
//!   before calling this function, so a missing/invalid vault surfaces as
//!   `E_VAULT_NOT_FOUND` (exit 64) at the dispatch layer and this `78` guard
//!   is effectively unreachable through the CLI. It's kept only as
//!   defense-in-depth for any direct in-process caller of [`run`].
//! - `127` — couldn't run/track the child (spawn failed, e.g. the harness
//!   binary not on disk; or a fault while waiting on it)
//! - `128 + signal` — child terminated by signal (Unix only)
//! - any other code — propagated from child verbatim
//! - `1` — fallback when child exited with no code and no signal

use crate::cli::HarnessArg;
use anyhow::{anyhow, Context, Result};
use onebrain_core::find_config_file;
use onebrain_fs::{build_prompt, resolve_claude_bin, resolve_gemini_bin};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Entry point invoked from `main.rs`. Returns the exit code the binary
/// should call `std::process::exit` with.
pub fn run(
    vault: &str,
    skill: &str,
    args: &[String],
    harness: HarnessArg,
    model: Option<&str>,
    want_json: bool,
) -> Result<i32> {
    let vault_path = PathBuf::from(vault);
    if find_config_file(&vault_path).is_none() {
        eprintln!(
            "✗ Vault not found at {vault} (no onebrain.yml present)\n\
             💡 run this from inside a vault, or pass `--vault <path>` pointing at one"
        );
        return Ok(78); // EX_CONFIG (sysexits.h)
    }

    let pairs = parse_args(args)?;
    let prompt = build_prompt(skill, &pairs).map_err(|e| anyhow!(e))?;

    let env_lookup = |k: &str| std::env::var(k).ok();
    let path_exists = |p: &Path| p.exists();
    let home = std::env::var("HOME").ok();
    let resolution = match harness {
        HarnessArg::Claude => resolve_claude_bin(None, env_lookup, path_exists, home.as_deref()),
        HarnessArg::Gemini => resolve_gemini_bin(None, env_lookup, path_exists, home.as_deref()),
    };
    if let Some(warning) = &resolution.warning {
        // `warning`'s own text is kept stable (Bun-parity, see
        // `resolve_bin`'s doc comment) — only the wrapping hint is new.
        eprintln!("⚠ {warning}\n💡 fix or unset the env var above to silence this");
    }

    // Skills always run with-context: --add-dir / --include-directories the vault.
    let argv = harness_argv(harness, &prompt, Some(vault), model, want_json);
    spawn_harness(&resolution.path, &argv, &vault_path, harness, "the skill")
}

/// Build the argv (after the binary name) for the chosen harness. Pure so the
/// per-harness flag mapping is unit-testable. `pub(crate)` so `harness run`
/// can reuse it with a raw prompt instead of the `/onebrain:<skill>` form.
///
/// `context_dir = Some(<vault>)` injects `--add-dir <vault>` (claude) /
/// `--include-directories <vault>` (gemini) so the harness loads OneBrain's
/// CLAUDE.md / GEMINI.md. `context_dir = None` skips that flag — the
/// `--mode ad-hoc` path for `onebrain harness run`. Gemini always gets
/// `--approval-mode yolo` (we pipe stdin null so the harness can't answer an
/// approval prompt).
pub(crate) fn harness_argv(
    harness: HarnessArg,
    prompt: &str,
    context_dir: Option<&str>,
    model: Option<&str>,
    want_json: bool,
) -> Vec<String> {
    let mut argv = vec!["-p".to_string(), prompt.to_string()];
    match harness {
        HarnessArg::Claude => {
            if let Some(dir) = context_dir {
                argv.push("--add-dir".to_string());
                argv.push(dir.to_string());
            }
            if let Some(m) = model {
                argv.push("--model".to_string());
                argv.push(m.to_string());
            }
            if want_json {
                argv.push("--output-format".to_string());
                argv.push("json".to_string());
            }
        }
        HarnessArg::Gemini => {
            if let Some(dir) = context_dir {
                argv.push("--include-directories".to_string());
                argv.push(dir.to_string());
            }
            // Auto-approve tools: the harness child has null stdin and can't
            // answer an approval prompt, and OneBrain skills run `onebrain`
            // shell commands + write vault files.
            argv.push("--approval-mode".to_string());
            argv.push("yolo".to_string());
            if let Some(m) = model {
                argv.push("-m".to_string());
                argv.push(m.to_string());
            }
            if want_json {
                argv.push("--output-format".to_string());
                argv.push("json".to_string());
            }
        }
    }
    argv
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

/// Prepend `exe_dir` to `existing_path` (the child process PATH) so a headless
/// `claude` can find the `onebrain` binary even under launchd's minimal PATH.
/// Idempotent: if `existing_path` already starts with `exe_dir`, returns it
/// unchanged. Uses the platform PATH separator.
fn child_path_with_exe_dir(exe_dir: &str, existing_path: &str) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    if existing_path.is_empty() {
        exe_dir.to_string()
    } else if existing_path == exe_dir || existing_path.starts_with(&format!("{exe_dir}{sep}")) {
        existing_path.to_string()
    } else {
        format!("{exe_dir}{sep}{existing_path}")
    }
}

/// Spawn the chosen harness binary on the prepared argv. Shared by `skill run`
/// and `harness run` — `subject` is the noun in the spinner message
/// ("the skill" vs "the prompt") so the watched-run UI matches whichever
/// command the user invoked.
pub(crate) fn spawn_harness(
    bin: &Path,
    argv: &[String],
    cwd: &Path,
    harness: HarnessArg,
    subject: &str,
) -> Result<i32> {
    use std::io::IsTerminal;
    let label = harness.as_str();
    let bin_env_var = match harness {
        HarnessArg::Claude => "CLAUDE_BIN",
        HarnessArg::Gemini => "GEMINI_BIN",
    };
    // No `env` override beyond ONEBRAIN_HEADLESS: child inherits parent env so
    // PATH/HOME survive. stdout/stderr stay inherited so the harness's output
    // (and colour) reach the terminal verbatim; stdin is forced to null so
    // `claude -p` / `gemini -p` never block reading an interactive TTY — see
    // module docs. ONEBRAIN_HEADLESS=1 drives the session-init handshake that
    // lets the skill skip the interactive startup ceremony.
    let mut command = Command::new(bin);
    command
        .args(argv)
        .current_dir(cwd)
        .env("ONEBRAIN_HEADLESS", "1")
        .stdin(Stdio::null());

    // Under launchd, the parent `onebrain` process (spawned by launchd itself)
    // has a minimal PATH without e.g. `/opt/homebrew/bin`. The headless
    // `claude`/`gemini` child then can't resolve bare `onebrain` invocations
    // from its own skill hooks (session-init, checkpoint, etc.), which fail
    // with exit 78 (#124). Prepend our own binary's directory to the child's
    // PATH so nested `onebrain` calls resolve. Safe for interactive runs too
    // (the user's PATH already has it, so this is a no-op there).
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
    {
        if let Some(dir) = exe_dir.to_str() {
            let existing = std::env::var("PATH").unwrap_or_default();
            command.env("PATH", child_path_with_exe_dir(dir, &existing));
        }
    }

    // Non-interactive (launchd scheduler, piped, CI): block and let the
    // captured StandardOutPath / pipe collect the output. No progress lines —
    // they'd only litter a log file.
    if !std::io::stderr().is_terminal() {
        return match command.status() {
            Ok(s) => Ok(translate_exit(&s)),
            Err(e) => {
                eprintln!(
                    "✗ Failed to spawn {label} ({}): {e}\n\
                     💡 make sure `{label}` is installed and on PATH (check with `which {label}`), \
                     or set `{bin_env_var}` to its full path",
                    bin.display()
                );
                Ok(127)
            }
        };
    }

    // Interactive: the harness runs a full headless agent session (plugin load,
    // startup, the skill's own work, LLM inference) and buffers its result
    // until done — often tens of seconds. We pipe its stdout/stderr, show an
    // in-place `indicatif` spinner during the wait, then dump the captured
    // streams once on exit.
    //
    // ASSUMES: `claude -p` and `gemini -p` are buffered-flush-at-end (no
    // real-time streaming in their headless modes today). Capturing therefore
    // loses nothing visible and removes the spinner-vs-flush race that was the
    // v3.2.4 tradeoff that turned the wait into a wall of "still running"
    // newlines. If a future harness version starts streaming via `-p`, this
    // branch will swallow that streaming until exit — revisit then.
    //
    // The child has a null stdin (see module docs), so a harness can never
    // answer an approval prompt — gemini runs with `--approval-mode yolo`.
    // Note that explicitly on a watched run so it isn't a surprise.
    use indicatif::{ProgressBar, ProgressStyle};
    use std::io::{Read, Write};

    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let approval_note = match harness {
        HarnessArg::Gemini => " (auto-approving tools)",
        HarnessArg::Claude => "",
    };
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg} · {elapsed}")
            .expect("static template is valid")
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
    );
    spinner.set_message(format!(
        "Running {label} on {subject} headlessly{approval_note}"
    ));
    spinner.enable_steady_tick(std::time::Duration::from_millis(120));

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            spinner.finish_and_clear();
            eprintln!(
                "✗ Failed to spawn {label} ({}): {e}\n\
                 💡 make sure `{label}` is installed and on PATH (check with `which {label}`), \
                 or set `{bin_env_var}` to its full path",
                bin.display()
            );
            return Ok(127);
        }
    };

    // Drain both pipes on spawned threads (avoids the pipe-deadlock that
    // `wait_with_output` handles internally) while we keep the child handle.
    // That way, if `child.wait()` later errors with the child still running,
    // we can still `kill()` + `wait()` — `wait_with_output` consumes the
    // handle and would leak an orphan harness still burning API tokens.
    let mut stdout_pipe = child
        .stdout
        .take()
        .expect("stdout was piped, must be present");
    let mut stderr_pipe = child
        .stderr
        .take()
        .expect("stderr was piped, must be present");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            // Couldn't observe the child — a harness/OS fault, not a skill
            // failure, so 127 (the "couldn't run it properly" class) rather
            // than 1. kill+wait reaps the child so it doesn't keep running
            // (and burning API tokens) after we return.
            spinner.finish_and_clear();
            eprintln!(
                "✗ Lost track of {label} while it was running: {e}\n\
                 💡 this is an OS/process fault, not a skill problem — rerun `onebrain skill run`; \
                 if it persists, check system resource limits (open files/processes)"
            );
            let _ = child.kill();
            let _ = child.wait();
            return Ok(127);
        }
    };
    let stdout_bytes = stdout_thread.join().unwrap_or_default();
    let stderr_bytes = stderr_thread.join().unwrap_or_default();
    spinner.finish_and_clear();

    // Flush captured streams in the natural order: stderr first (warnings,
    // YOLO note), then stdout (the actual result). Raw `write_all` so any
    // ANSI colour the harness emitted (when it kept colour despite the pipe)
    // reaches the terminal verbatim. Errors are swallowed — the run already
    // completed; a downstream pipe closing after success shouldn't flip the
    // exit code. Explicit `flush` covers the block-buffered piped-stdout case
    // (e.g. `onebrain skill run … | tee log`).
    {
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        let _ = handle.write_all(&stderr_bytes);
        let _ = handle.flush();
    }
    {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(&stdout_bytes);
        let _ = handle.flush();
    }

    Ok(translate_exit(&status))
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

    // ---- child_path_with_exe_dir (#124) ----

    #[test]
    fn child_path_empty_existing_returns_exe_dir() {
        assert_eq!(
            child_path_with_exe_dir("/opt/homebrew/bin", ""),
            "/opt/homebrew/bin"
        );
    }

    #[test]
    fn child_path_prepends_with_platform_separator() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        assert_eq!(
            child_path_with_exe_dir("/opt/homebrew/bin", "/usr/bin:/bin"),
            format!("/opt/homebrew/bin{sep}/usr/bin:/bin")
        );
    }

    #[test]
    fn child_path_idempotent_when_existing_equals_exe_dir() {
        // Already exactly the exe dir · no change.
        assert_eq!(
            child_path_with_exe_dir("/opt/homebrew/bin", "/opt/homebrew/bin"),
            "/opt/homebrew/bin"
        );
    }

    #[test]
    fn child_path_idempotent_when_existing_already_prefixed() {
        // exe_dir already leads the PATH · no double-prepend. Build the input
        // with the platform separator so this holds on Windows (`;`) too — the
        // function's dedup guard checks the platform-specific `{exe_dir}{sep}`.
        let sep = if cfg!(windows) { ";" } else { ":" };
        let existing = format!("/opt/homebrew/bin{sep}/usr/bin");
        assert_eq!(
            child_path_with_exe_dir("/opt/homebrew/bin", &existing),
            existing
        );
    }

    #[test]
    fn child_path_does_not_false_dedup_on_shared_prefix() {
        // `/opt/homebrew/bin2` merely *starts with* the string
        // "/opt/homebrew/bin" but is a different directory · must still
        // prepend. This is why the guard checks `== exe_dir` or
        // `starts_with("{exe_dir}{sep}")`, not a bare `starts_with(exe_dir)`.
        let sep = if cfg!(windows) { ";" } else { ":" };
        assert_eq!(
            child_path_with_exe_dir("/opt/homebrew/bin", "/opt/homebrew/bin2:/usr/bin"),
            format!("/opt/homebrew/bin{sep}/opt/homebrew/bin2:/usr/bin")
        );
    }

    // ---- harness_argv ----

    #[test]
    fn harness_argv_claude_uses_add_dir_and_no_model_by_default() {
        let argv = harness_argv(
            HarnessArg::Claude,
            "/onebrain:daily",
            Some("/vault"),
            None,
            false,
        );
        assert_eq!(argv, vec!["-p", "/onebrain:daily", "--add-dir", "/vault"]);
    }

    #[test]
    fn harness_argv_claude_appends_model_flag() {
        let argv = harness_argv(
            HarnessArg::Claude,
            "/onebrain:daily",
            Some("/vault"),
            Some("claude-haiku-4-5"),
            false,
        );
        assert_eq!(
            argv,
            vec![
                "-p",
                "/onebrain:daily",
                "--add-dir",
                "/vault",
                "--model",
                "claude-haiku-4-5"
            ]
        );
    }

    #[test]
    fn harness_argv_gemini_uses_include_directories_and_yolo() {
        let argv = harness_argv(
            HarnessArg::Gemini,
            "/onebrain:daily",
            Some("/vault"),
            None,
            false,
        );
        assert_eq!(
            argv,
            vec![
                "-p",
                "/onebrain:daily",
                "--include-directories",
                "/vault",
                "--approval-mode",
                "yolo"
            ]
        );
    }

    #[test]
    fn harness_argv_claude_omits_add_dir_when_context_dir_none() {
        // `--mode ad-hoc`: no `--add-dir` flag, no vault context loaded.
        let argv = harness_argv(HarnessArg::Claude, "what is 2+2?", None, None, false);
        assert_eq!(argv, vec!["-p", "what is 2+2?"]);
    }

    #[test]
    fn harness_argv_gemini_omits_include_dirs_but_keeps_yolo_for_ad_hoc() {
        // `--mode ad-hoc`: no `--include-directories`, but yolo stays because
        // the child still has null stdin and can't answer approval prompts.
        let argv = harness_argv(HarnessArg::Gemini, "what is 2+2?", None, None, false);
        assert_eq!(argv, vec!["-p", "what is 2+2?", "--approval-mode", "yolo"]);
    }

    #[test]
    fn harness_argv_claude_appends_output_format_json_when_want_json() {
        // `--json` at the OneBrain CLI maps to `--output-format json` on the
        // claude harness — passthrough so the captured stdout is native JSON.
        let argv = harness_argv(HarnessArg::Claude, "hi", None, None, true);
        assert_eq!(argv, vec!["-p", "hi", "--output-format", "json"]);
    }

    #[test]
    fn harness_argv_gemini_appends_output_format_json_when_want_json() {
        // Gemini supports `--output-format json` too. Sits after `--approval-mode
        // yolo` and after `-m <model>` if present.
        let argv = harness_argv(HarnessArg::Gemini, "hi", None, None, true);
        assert_eq!(
            argv,
            vec![
                "-p",
                "hi",
                "--approval-mode",
                "yolo",
                "--output-format",
                "json"
            ]
        );
    }

    #[test]
    fn harness_argv_gemini_appends_short_model_flag() {
        let argv = harness_argv(
            HarnessArg::Gemini,
            "/onebrain:daily",
            Some("/vault"),
            Some("gemini-2.5-flash"),
            false,
        );
        assert_eq!(
            argv,
            vec![
                "-p",
                "/onebrain:daily",
                "--include-directories",
                "/vault",
                "--approval-mode",
                "yolo",
                "-m",
                "gemini-2.5-flash"
            ]
        );
    }

    /// Exercises the spawn path + exit-code translation with the harness stubbed
    /// by a script that exits 43 on EOF / 42 after reading a line.
    ///
    /// NOTE: under `cargo test` the harness's own stdin is already at EOF, so
    /// this cannot *distinguish* a null stdin from an inherited one (both yield
    /// 43) — it pins the EOF→exit-43 contract, not the `.stdin(null)` line
    /// itself. The real fix (an interactive TTY no longer hangs `-p`) is
    /// verified manually; a hermetic guard would need to inject a live parent
    /// stdin, which `spawn_harness` (it reads the process's fd 0) can't take
    /// without an API change made solely for the test.
    #[cfg(unix)]
    #[test]
    fn spawn_harness_gives_child_a_non_blocking_stdin() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("harness_stub.sh");
        {
            let mut f = std::fs::File::create(&stub).unwrap();
            writeln!(
                f,
                "#!/bin/sh\nif IFS= read -r _; then exit 42; else exit 43; fi"
            )
            .unwrap();
        }
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let argv = vec!["-p".to_string(), "/daily".to_string()];
        let code = spawn_harness(&stub, &argv, dir.path(), HarnessArg::Claude, "the test").unwrap();
        assert_eq!(code, 43, "child should see EOF on a null stdin, not block");
    }
}
