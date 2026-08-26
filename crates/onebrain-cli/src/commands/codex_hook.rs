//! Cache-independent bridge for Codex plugin hooks.
//!
//! Codex retains hook commands for the lifetime of an active task. Keeping
//! the bridge in the installed CLI avoids binding those commands to a plugin
//! cache directory that the plugin manager may replace mid-task.

use crate::cli::CodexHookMode;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(7);
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(2);
const MIN_HOOK_CHILD_VERSION: &str = "3.4.25";

#[derive(Clone, Debug)]
struct ChildResult {
    success: bool,
    stdout: String,
}

trait HookRunner {
    fn executable(&self) -> &Path;
    fn invoke(
        &self,
        args: &[&str],
        session_id: &str,
        timeout: Duration,
        capture_stdout: bool,
    ) -> Option<ChildResult>;
}

struct ProcessRunner {
    executable: PathBuf,
}

impl ProcessRunner {
    fn resolve(mode: CodexHookMode) -> Option<Self> {
        if let Some(executable) = std::env::var_os("ONEBRAIN_BIN")
            .filter(|value| !value.is_empty())
            .map(resolve_command_path)
        {
            let runner = Self { executable };
            let is_current = std::env::current_exe()
                .ok()
                .is_some_and(|current| same_executable(runner.executable(), &current));
            let needs_contract_probe = mode == CodexHookMode::SessionStart && !is_current;
            return (!needs_contract_probe || runner.supports_hook_contract()).then_some(runner);
        }

        let executable = std::env::current_exe()
            .ok()
            .or_else(|| {
                std::env::args_os()
                    .next()
                    .map(PathBuf::from)
                    .map(resolve_command_path)
            })
            .unwrap_or_else(|| PathBuf::from("onebrain"));
        Some(Self { executable })
    }
}

fn same_executable(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn resolve_command_path(candidate: impl Into<PathBuf>) -> PathBuf {
    let candidate = candidate.into();
    if candidate.is_absolute() {
        return candidate;
    }
    if candidate.components().count() == 1 {
        return which::which(&candidate).unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|cwd| cwd.join(&candidate))
                .unwrap_or(candidate)
        });
    }
    let joined = std::env::current_dir()
        .map(|cwd| cwd.join(&candidate))
        .unwrap_or(candidate);
    joined.canonicalize().unwrap_or(joined)
}

impl HookRunner for ProcessRunner {
    fn executable(&self) -> &Path {
        &self.executable
    }

    fn invoke(
        &self,
        args: &[&str],
        session_id: &str,
        timeout: Duration,
        capture_stdout: bool,
    ) -> Option<ChildResult> {
        let mut command = Command::new(&self.executable);
        command
            .args(args)
            .env("CODEX_SESSION_ID", session_id)
            .stdin(Stdio::null())
            .stdout(if capture_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::null());
        let mut child = command.spawn().ok()?;
        let started = Instant::now();
        let stdout_receiver = if capture_stdout {
            let mut pipe = child.stdout.take()?;
            let (sender, receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = pipe.read_to_end(&mut bytes);
                let _ = sender.send(bytes);
            });
            Some(receiver)
        } else {
            None
        };

        let status = match child.wait_timeout(timeout) {
            Ok(Some(status)) => status,
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };
        let mut stdout = String::new();
        if let Some(receiver) = stdout_receiver {
            let remaining = timeout.saturating_sub(started.elapsed());
            let bytes = receiver.recv_timeout(remaining).ok()?;
            stdout = String::from_utf8(bytes).ok()?;
        }
        Some(ChildResult {
            success: status.success(),
            stdout,
        })
    }
}

impl ProcessRunner {
    /// `ONEBRAIN_BIN` is an explicit child override used by managed skill runs
    /// and integration tests. Refuse a stale override rather than injecting an
    /// executable that lacks the hook/task contracts required by this plugin.
    /// The normal `current_exe` path needs no extra probe: reaching this command
    /// already proves that binary contains the v3.4.25 hook runner.
    fn supports_hook_contract(&self) -> bool {
        let Some(result) = self.invoke(&["--version"], "", FOREGROUND_TIMEOUT, true) else {
            return false;
        };
        if !result.success {
            return false;
        }
        let minimum =
            semver::Version::parse(MIN_HOOK_CHILD_VERSION).expect("valid minimum version");
        result
            .stdout
            .split_whitespace()
            .filter_map(|part| semver::Version::parse(part.trim_start_matches('v')).ok())
            .any(|version| version >= minimum)
    }
}

/// Hook failures are deliberately fail-open: Codex work must continue even
/// when OneBrain is missing, slow, or returns malformed data.
pub fn run(mode: CodexHookMode) -> Result<()> {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Ok(());
    }
    let Some(runner) = ProcessRunner::resolve(mode) else {
        return Ok(());
    };
    handle(mode, &input, &runner, &mut std::io::stdout().lock());
    Ok(())
}

fn handle(mode: CodexHookMode, input: &str, runner: &impl HookRunner, output: &mut impl Write) {
    let Ok(payload) = serde_json::from_str::<Value>(input) else {
        return;
    };
    let Some(session_id) = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return;
    };

    let (args, timeout) = match mode {
        CodexHookMode::SessionStart => {
            (["session", "init", "--json"].as_slice(), FOREGROUND_TIMEOUT)
        }
        CodexHookMode::Checkpoint => (
            ["checkpoint", "stop", "--json"].as_slice(),
            FOREGROUND_TIMEOUT,
        ),
        CodexHookMode::Lex => (
            ["search", "reindex", "--lex-only", "--json"].as_slice(),
            BACKGROUND_TIMEOUT,
        ),
        CodexHookMode::Pending => (
            ["search", "reindex", "--pending-only", "--json"].as_slice(),
            BACKGROUND_TIMEOUT,
        ),
    };
    let capture_stdout = matches!(
        mode,
        CodexHookMode::SessionStart | CodexHookMode::Checkpoint
    );
    let Some(child) = runner.invoke(args, session_id, timeout, capture_stdout) else {
        return;
    };
    if !child.success {
        return;
    }

    match mode {
        CodexHookMode::SessionStart => {
            emit_session_start(&child.stdout, runner.executable(), output)
        }
        CodexHookMode::Checkpoint => {
            let _ = output.write_all(child.stdout.as_bytes());
        }
        CodexHookMode::Lex | CodexHookMode::Pending => {}
    }
}

fn emit_session_start(raw: &str, executable: &Path, output: &mut impl Write) {
    let Ok(metadata) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    let Some(token) = metadata
        .get("session_token")
        .or_else(|| metadata.pointer("/data/session_token"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return;
    };

    let executable = executable.to_string_lossy();
    let posix_path = posix_quote(&executable);
    let powershell_path = executable.replace('\'', "''");
    let compact_metadata = serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".into());
    let context = format!(
        "OneBrain Codex session_token: {token}. Preserve this token for checkpoint and wrapup isolation in this chat. Session initialization already completed inside the hook; do not invoke `session init` again. Startup metadata: {compact_metadata}. Use this exact executable path for every later OneBrain CLI call in this chat: POSIX {posix_path}; Windows PowerShell & '{powershell_path}'."
    );
    let response = json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": context,
        }
    });
    let _ = serde_json::to_writer(output, &response);
}

fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Debug, Eq, PartialEq)]
    struct Call {
        args: Vec<String>,
        session_id: String,
        timeout: Duration,
        capture_stdout: bool,
    }

    struct FakeRunner {
        executable: PathBuf,
        result: Option<ChildResult>,
        calls: RefCell<Vec<Call>>,
    }

    impl FakeRunner {
        fn successful(stdout: &str) -> Self {
            Self {
                executable: PathBuf::from("/opt/homebrew/bin/onebrain"),
                result: Some(ChildResult {
                    success: true,
                    stdout: stdout.into(),
                }),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl HookRunner for FakeRunner {
        fn executable(&self) -> &Path {
            &self.executable
        }

        fn invoke(
            &self,
            args: &[&str],
            session_id: &str,
            timeout: Duration,
            capture_stdout: bool,
        ) -> Option<ChildResult> {
            self.calls.borrow_mut().push(Call {
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                session_id: session_id.into(),
                timeout,
                capture_stdout,
            });
            self.result.clone()
        }
    }

    fn hook_input(session_id: &str) -> String {
        json!({"session_id": session_id}).to_string()
    }

    #[test]
    fn session_start_emits_context_from_precollected_metadata() {
        let runner = FakeRunner::successful(
            r#"{"session_token":"abc123","vault":"/tmp/vault","tasks_due":5}"#,
        );
        let mut output = Vec::new();

        handle(
            CodexHookMode::SessionStart,
            &hook_input("codex-session"),
            &runner,
            &mut output,
        );

        assert_eq!(
            runner.calls.into_inner(),
            vec![Call {
                args: vec!["session".into(), "init".into(), "--json".into()],
                session_id: "codex-session".into(),
                timeout: FOREGROUND_TIMEOUT,
                capture_stdout: true,
            }]
        );
        let response: Value = serde_json::from_slice(&output).unwrap();
        let context = response
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap();
        assert!(context.contains("session_token: abc123"));
        assert!(context.contains("Session initialization already completed"));
        assert!(context.contains(r#""tasks_due":5"#));
        assert!(context.contains("'/opt/homebrew/bin/onebrain'"));
    }

    #[test]
    fn checkpoint_forwards_cli_protocol_output() {
        let runner = FakeRunner::successful("{\"continue\":true}\n");
        let mut output = Vec::new();

        handle(
            CodexHookMode::Checkpoint,
            &hook_input("codex-session"),
            &runner,
            &mut output,
        );

        assert_eq!(String::from_utf8(output).unwrap(), "{\"continue\":true}\n");
        assert_eq!(
            runner.calls.into_inner()[0].args,
            ["checkpoint", "stop", "--json"]
        );
    }

    #[test]
    fn background_modes_suppress_child_stdout() {
        for (mode, expected_tail) in [
            (CodexHookMode::Lex, "--lex-only"),
            (CodexHookMode::Pending, "--pending-only"),
        ] {
            let runner = FakeRunner::successful("noisy child output");
            let mut output = Vec::new();

            handle(mode, &hook_input("codex-session"), &runner, &mut output);

            assert!(output.is_empty());
            let calls = runner.calls.into_inner();
            assert!(calls[0].args.iter().any(|arg| arg == expected_tail));
            assert_eq!(calls[0].timeout, BACKGROUND_TIMEOUT);
            assert!(!calls[0].capture_stdout);
        }
    }

    #[test]
    fn invalid_input_and_child_failure_fail_open() {
        let runner = FakeRunner::successful("unused");
        let mut output = Vec::new();
        handle(
            CodexHookMode::SessionStart,
            "not-json",
            &runner,
            &mut output,
        );
        assert!(runner.calls.into_inner().is_empty());
        assert!(output.is_empty());

        let failed = FakeRunner {
            executable: PathBuf::from("onebrain"),
            result: Some(ChildResult {
                success: false,
                stdout: "error".into(),
            }),
            calls: RefCell::new(Vec::new()),
        };
        handle(
            CodexHookMode::Checkpoint,
            &hook_input("codex-session"),
            &failed,
            &mut output,
        );
        assert!(output.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn same_executable_recognizes_a_stable_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("onebrain-real");
        let stable_link = temp.path().join("onebrain");
        std::fs::write(&executable, "binary").unwrap();
        std::os::unix::fs::symlink(&executable, &stable_link).unwrap();

        assert!(same_executable(&stable_link, &executable));
    }
}
