//! Cache-independent lifecycle hook bridge shared by every supported harness.
//!
//! Harnesses retain hook commands for the lifetime of an active session. Keeping
//! this bridge in the installed CLI avoids binding commands to a plugin cache
//! directory that the plugin manager may replace mid-session.

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HookEvent {
    SessionStart,
    ToolCompleted,
    Stop,
}

impl HookEvent {
    fn from_payload(payload: &Value) -> Option<Self> {
        match payload.get("hook_event_name").and_then(Value::as_str) {
            Some("SessionStart") => Some(Self::SessionStart),
            Some("PostToolUse" | "AfterTool") => Some(Self::ToolCompleted),
            Some("Stop" | "AfterAgent") => Some(Self::Stop),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct ChildResult {
    success: bool,
    stdout: String,
}

trait HookRunner: Sync {
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
    fn resolve(needs_contract_probe: bool) -> Option<Self> {
        if let Some(executable) = std::env::var_os("ONEBRAIN_BIN")
            .filter(|value| !value.is_empty())
            .map(resolve_command_path)
        {
            let runner = Self { executable };
            let is_current = std::env::current_exe()
                .ok()
                .is_some_and(|current| same_executable(runner.executable(), &current));
            return (!needs_contract_probe || is_current || runner.supports_hook_contract())
                .then_some(runner);
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

    /// `ONEBRAIN_BIN` is an explicit child override used by managed skill runs
    /// and integration tests. Refuse a stale override rather than injecting an
    /// executable that lacks the lifecycle-hook contract required by this plugin.
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
            .env("ONEBRAIN_HOOK_SESSION_ID", session_id)
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

/// Hook failures are deliberately fail-open: agent work must continue even
/// when OneBrain is missing, slow, or returns malformed data.
pub fn run() -> Result<()> {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let mut output = std::io::stdout().lock();

    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        emit_empty(&mut output);
        return Ok(());
    };
    let Some(event) = HookEvent::from_payload(&payload) else {
        emit_empty(&mut output);
        return Ok(());
    };
    let Some(runner) = ProcessRunner::resolve(event == HookEvent::SessionStart) else {
        emit_empty(&mut output);
        return Ok(());
    };

    handle(event, &payload, &runner, &mut output);
    Ok(())
}

fn handle(
    event: HookEvent,
    payload: &Value,
    runner: &(impl HookRunner + ?Sized),
    output: &mut impl Write,
) {
    let Some(session_id) = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        emit_empty(output);
        return;
    };

    match event {
        HookEvent::SessionStart => {
            let result = runner.invoke(
                &["session", "init", "--json"],
                session_id,
                FOREGROUND_TIMEOUT,
                true,
            );
            if let Some(child) = result.filter(|child| child.success) {
                if emit_session_start(&child.stdout, runner.executable(), output) {
                    return;
                }
            }
            emit_empty(output);
        }
        HookEvent::ToolCompleted => {
            let _ = runner.invoke(
                &["search", "reindex", "--lex-only", "--json"],
                session_id,
                BACKGROUND_TIMEOUT,
                false,
            );
            emit_empty(output);
        }
        HookEvent::Stop => {
            let checkpoint = std::thread::scope(|scope| {
                let checkpoint = scope.spawn(|| {
                    runner.invoke(
                        &["checkpoint", "stop", "--json"],
                        session_id,
                        FOREGROUND_TIMEOUT,
                        true,
                    )
                });
                let pending = scope.spawn(|| {
                    runner.invoke(
                        &["search", "reindex", "--pending-only", "--json"],
                        session_id,
                        BACKGROUND_TIMEOUT,
                        false,
                    )
                });
                let checkpoint = checkpoint.join().ok().flatten();
                let _ = pending.join();
                checkpoint
            });
            if let Some(child) = checkpoint.filter(|child| child.success) {
                if emit_protocol_json(&child.stdout, output) {
                    return;
                }
            }
            emit_empty(output);
        }
    }
}

fn emit_session_start(raw: &str, executable: &Path, output: &mut impl Write) -> bool {
    let Ok(metadata) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    let Some(token) = metadata
        .get("session_token")
        .or_else(|| metadata.pointer("/data/session_token"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };

    let executable = executable.to_string_lossy();
    let posix_path = posix_quote(&executable);
    let powershell_path = executable.replace('\'', "''");
    let compact_metadata = serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".into());
    let context = format!(
        "OneBrain session_token: {token}. Preserve this token for checkpoint and wrapup isolation in this chat. Session initialization already completed inside the hook; do not invoke `session init` again. Startup metadata: {compact_metadata}. Use this exact executable path for every later OneBrain CLI call in this chat: POSIX {posix_path}; Windows PowerShell & '{powershell_path}'."
    );
    let response = json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": context,
        }
    });
    serde_json::to_writer(&mut *output, &response).is_ok() && output.write_all(b"\n").is_ok()
}

fn emit_protocol_json(raw: &str, output: &mut impl Write) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || !serde_json::from_str::<Value>(trimmed).is_ok_and(|value| value.is_object())
    {
        return false;
    }
    output.write_all(trimmed.as_bytes()).is_ok() && output.write_all(b"\n").is_ok()
}

fn emit_empty(output: &mut impl Write) {
    let _ = output.write_all(b"{}\n");
}

fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Debug, Eq, PartialEq)]
    struct Call {
        args: Vec<String>,
        session_id: String,
        timeout: Duration,
        capture_stdout: bool,
    }

    /// Canned child results.
    ///
    /// `Uniform` answers every invocation with the same result — enough for
    /// the single-child events. `PerArgs` keys results by exact argv, which
    /// `Stop` needs: its two children run on separate scoped threads, so the
    /// order in which they reach the runner is nondeterministic and a
    /// pop-a-queue model would hand the checkpoint child the pending child's
    /// stdout half the time. An argv absent from the map yields `None` (a
    /// child that produced no result).
    enum FakeResults {
        Uniform(ChildResult),
        PerArgs(HashMap<Vec<String>, ChildResult>),
    }

    struct FakeRunner {
        executable: PathBuf,
        results: FakeResults,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeRunner {
        fn successful(stdout: &str) -> Self {
            Self::new(FakeResults::Uniform(ChildResult {
                success: true,
                stdout: stdout.into(),
            }))
        }

        fn per_args<'a>(results: impl IntoIterator<Item = (&'a [&'a str], &'a str)>) -> Self {
            Self::new(FakeResults::PerArgs(
                results
                    .into_iter()
                    .map(|(args, stdout)| {
                        (
                            args.iter().map(|arg| (*arg).to_string()).collect(),
                            ChildResult {
                                success: true,
                                stdout: stdout.into(),
                            },
                        )
                    })
                    .collect(),
            ))
        }

        fn new(results: FakeResults) -> Self {
            Self {
                executable: PathBuf::from("/opt/homebrew/bin/onebrain"),
                results,
                calls: Mutex::new(Vec::new()),
            }
        }

        /// Recorded invocations, sorted by argv — `Stop` records its two
        /// children in nondeterministic order, so callers compare an
        /// unordered set rather than a sequence.
        fn calls(&self) -> Vec<Call> {
            let mut calls = std::mem::take(&mut *self.calls.lock().unwrap());
            calls.sort_by(|left, right| left.args.cmp(&right.args));
            calls
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
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
            self.calls.lock().unwrap().push(Call {
                args: args.clone(),
                session_id: session_id.into(),
                timeout,
                capture_stdout,
            });
            match &self.results {
                FakeResults::Uniform(result) => Some(result.clone()),
                FakeResults::PerArgs(results) => results.get(&args).cloned(),
            }
        }
    }

    fn hook_payload(event: &str, session_id: &str) -> Value {
        json!({"hook_event_name": event, "session_id": session_id})
    }

    #[test]
    fn event_names_map_across_supported_protocols() {
        for (name, expected) in [
            ("SessionStart", Some(HookEvent::SessionStart)),
            ("PostToolUse", Some(HookEvent::ToolCompleted)),
            ("AfterTool", Some(HookEvent::ToolCompleted)),
            ("Stop", Some(HookEvent::Stop)),
            ("AfterAgent", Some(HookEvent::Stop)),
            ("BeforeTool", None),
        ] {
            assert_eq!(
                HookEvent::from_payload(&json!({"hook_event_name": name})),
                expected
            );
        }
    }

    #[test]
    fn session_start_emits_context_from_precollected_metadata() {
        let runner = FakeRunner::successful(
            r#"{"session_token":"abc123","vault":"/tmp/vault","tasks_due":5}"#,
        );
        let mut output = Vec::new();

        handle(
            HookEvent::SessionStart,
            &hook_payload("SessionStart", "shared-session"),
            &runner,
            &mut output,
        );

        assert_eq!(
            runner.calls(),
            vec![Call {
                args: vec!["session".into(), "init".into(), "--json".into()],
                session_id: "shared-session".into(),
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
    fn tool_event_suppresses_child_stdout_and_emits_json() {
        let runner = FakeRunner::successful("noisy child output");
        let mut output = Vec::new();

        handle(
            HookEvent::ToolCompleted,
            &hook_payload("AfterTool", "shared-session"),
            &runner,
            &mut output,
        );

        assert_eq!(output, b"{}\n");
        assert_eq!(
            runner.calls(),
            vec![Call {
                args: vec![
                    "search".into(),
                    "reindex".into(),
                    "--lex-only".into(),
                    "--json".into(),
                ],
                session_id: "shared-session".into(),
                timeout: BACKGROUND_TIMEOUT,
                capture_stdout: false,
            }]
        );
    }

    #[test]
    fn stop_dispatches_the_checkpoint_and_the_pending_embed() {
        // Stop fans out to TWO children: the foreground checkpoint, whose
        // stdout becomes the hook's protocol response, and the background
        // pending-embed pass, whose output must stay suppressed. Deleting
        // either spawn must fail this test.
        let runner = FakeRunner::per_args([
            (
                &["checkpoint", "stop", "--json"][..],
                r#"{"decision":"block","reason":"15 since start"}"#,
            ),
            (
                &["search", "reindex", "--pending-only", "--json"][..],
                "background output must stay hidden",
            ),
        ]);
        let mut output = Vec::new();

        handle(
            HookEvent::Stop,
            &hook_payload("Stop", "shared-session"),
            &runner,
            &mut output,
        );

        // `calls()` sorts by argv — the two children race on separate scoped
        // threads, so only the SET of invocations is deterministic.
        assert_eq!(
            runner.calls(),
            vec![
                Call {
                    args: vec!["checkpoint".into(), "stop".into(), "--json".into()],
                    session_id: "shared-session".into(),
                    timeout: FOREGROUND_TIMEOUT,
                    capture_stdout: true,
                },
                Call {
                    args: vec![
                        "search".into(),
                        "reindex".into(),
                        "--pending-only".into(),
                        "--json".into(),
                    ],
                    session_id: "shared-session".into(),
                    timeout: BACKGROUND_TIMEOUT,
                    capture_stdout: false,
                },
            ]
        );
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "{\"decision\":\"block\",\"reason\":\"15 since start\"}\n"
        );
    }

    #[test]
    fn malformed_session_metadata_fails_open_with_json() {
        let runner = FakeRunner::successful("not-json");
        let mut output = Vec::new();

        handle(
            HookEvent::SessionStart,
            &hook_payload("SessionStart", "shared-session"),
            &runner,
            &mut output,
        );

        assert_eq!(output, b"{}\n");
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
