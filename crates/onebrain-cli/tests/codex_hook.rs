#![cfg(unix)]

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn fake_onebrain(root: &Path) -> PathBuf {
    let path = root.join("onebrain-child");
    fs::write(
        &path,
        r#"#!/usr/bin/env python3
import json
import os
import sys
import time

if os.environ.get("FAKE_PID_FILE"):
    with open(os.environ["FAKE_PID_FILE"], "w", encoding="utf-8") as handle:
        handle.write(str(os.getpid()))
if os.environ.get("FAKE_SLEEP_SECS"):
    time.sleep(float(os.environ["FAKE_SLEEP_SECS"]))
    if os.environ.get("FAKE_COMPLETED_FILE"):
        with open(os.environ["FAKE_COMPLETED_FILE"], "w", encoding="utf-8") as handle:
            handle.write("completed")

args = sys.argv[1:]
session_id = os.environ.get("CODEX_SESSION_ID", "")
if args == ["--version"]:
    if os.environ.get("FAKE_VERSION_PROBE_FILE"):
        with open(os.environ["FAKE_VERSION_PROBE_FILE"], "w", encoding="utf-8") as handle:
            handle.write("probed")
    if os.environ.get("FAKE_VERSION_SLEEP_SECS"):
        time.sleep(float(os.environ["FAKE_VERSION_SLEEP_SECS"]))
    print("onebrain " + os.environ.get("FAKE_VERSION", "3.4.25"))
elif args[:2] == ["session", "init"]:
    payload = {"session_token": session_id, "headless": False}
    if os.environ.get("FAKE_LARGE_OUTPUT"):
        payload["blob"] = "x" * 131072
    print(json.dumps(payload))
elif args[:2] == ["checkpoint", "stop"]:
    print(json.dumps({"decision": "block", "reason": "15 since start"}))
elif args[:2] == ["search", "reindex"]:
    print("background output must stay hidden")
else:
    sys.exit(64)
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn hook_command(fake: &Path, cache: &Path, mode: &str, session_id: &str) -> Command {
    let mut command = Command::cargo_bin("onebrain").unwrap();
    command
        .args(["codex-hook", mode])
        .env("ONEBRAIN_BIN", fake)
        .env("ONEBRAIN_CACHE_DIR", cache)
        .write_stdin(serde_json::json!({"session_id": session_id}).to_string());
    command
}

fn context(stdout: &[u8]) -> String {
    let response: Value = serde_json::from_slice(stdout).unwrap();
    response
        .pointer("/hookSpecificOutput/additionalContext")
        .and_then(Value::as_str)
        .unwrap()
        .to_string()
}

#[test]
fn session_start_drains_output_larger_than_a_pipe() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());

    let output = hook_command(&fake, temp.path(), "session-start", "large-output")
        .env("FAKE_LARGE_OUTPUT", "1")
        .output()
        .unwrap();

    assert!(output.status.success());
    let context = context(&output.stdout);
    assert!(context.contains("session_token: large-output"));
    assert!(context.contains(r#""blob":"xxx"#));
}

#[test]
fn bare_onebrain_override_is_injected_as_the_resolved_absolute_path() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());
    let mut paths = vec![temp.path().to_path_buf()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();
    let mut command = hook_command(
        Path::new("onebrain-child"),
        temp.path(),
        "session-start",
        "resolved-path",
    );

    let output = command.env("PATH", path).output().unwrap();

    assert!(output.status.success());
    let context = context(&output.stdout);
    assert!(
        context.contains(&fake.to_string_lossy().to_string()),
        "context did not pin resolved executable: {context}"
    );
    assert!(!context.contains("POSIX 'onebrain-child'"));
}

#[test]
fn relative_onebrain_override_is_injected_as_the_resolved_absolute_path() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());
    let mut command = hook_command(
        Path::new("./onebrain-child"),
        temp.path(),
        "session-start",
        "relative-path",
    );

    let output = command.current_dir(temp.path()).output().unwrap();

    assert!(output.status.success());
    let context = context(&output.stdout);
    assert!(context.contains(&fake.to_string_lossy().to_string()));
    assert!(!context.contains("/./onebrain-child"));
}

#[test]
fn stale_onebrain_override_fails_open_before_session_init() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());

    let output = hook_command(&fake, temp.path(), "session-start", "stale-override")
        .env("FAKE_VERSION", "3.4.24")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn compatible_override_version_probe_uses_the_foreground_budget() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());

    let output = hook_command(&fake, temp.path(), "session-start", "loaded-host")
        .env("FAKE_VERSION_SLEEP_SECS", "2.25")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!output.stdout.is_empty());
    assert!(context(&output.stdout).contains("session_token: loaded-host"));
}

#[test]
fn background_hook_does_not_probe_the_override_version() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());
    let probe_file = temp.path().join("version-probed");

    let output = hook_command(&fake, temp.path(), "lex", "background-fast")
        .env("FAKE_VERSION_PROBE_FILE", &probe_file)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        !probe_file.exists(),
        "background hook spawned an unnecessary version probe"
    );
}

#[test]
fn checkpoint_forwards_and_background_modes_suppress_real_child_output() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());

    let checkpoint = hook_command(&fake, temp.path(), "checkpoint", "protocol")
        .output()
        .unwrap();
    let lex = hook_command(&fake, temp.path(), "lex", "protocol")
        .output()
        .unwrap();
    let pending = hook_command(&fake, temp.path(), "pending", "protocol")
        .output()
        .unwrap();

    assert_eq!(
        String::from_utf8(checkpoint.stdout).unwrap(),
        "{\"decision\": \"block\", \"reason\": \"15 since start\"}\n"
    );
    assert!(lex.stdout.is_empty());
    assert!(pending.stdout.is_empty());
}

#[test]
fn timed_out_child_is_killed_reaped_and_fails_open() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());
    let pid_file = temp.path().join("child.pid");
    let completed_file = temp.path().join("child.completed");
    let started = Instant::now();

    let output = hook_command(&fake, temp.path(), "session-start", "timeout")
        .env("FAKE_SLEEP_SECS", "10")
        .env("FAKE_PID_FILE", &pid_file)
        .env("FAKE_COMPLETED_FILE", &completed_file)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "timeout exceeded fail-open bound: {elapsed:?}"
    );
    assert!(
        !completed_file.exists(),
        "timed-out child ran to completion"
    );
    let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "timed-out child survived"
    );
}

#[test]
fn outer_hook_dispatch_skips_search_cache_migration_noise() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());
    let mut command = Command::cargo_bin("onebrain").unwrap();
    command
        .args(["codex-hook", "session-start"])
        .env("ONEBRAIN_BIN", &fake)
        .env_remove("ONEBRAIN_CACHE_DIR")
        .write_stdin(serde_json::json!({"session_id": "quiet-prelude"}).to_string());

    #[cfg(target_os = "macos")]
    {
        let old = temp.path().join("Library/Caches/onebrain/search");
        let data_root = temp.path().join("Library/Application Support");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&data_root).unwrap();
        fs::write(data_root.join("onebrain"), "blocks directory creation").unwrap();
        command.env("HOME", temp.path());
    }

    #[cfg(target_os = "linux")]
    {
        let cache_root = temp.path().join("cache");
        let data_root = temp.path().join("data");
        fs::create_dir_all(cache_root.join("onebrain/search")).unwrap();
        fs::create_dir_all(&data_root).unwrap();
        fs::write(data_root.join("onebrain"), "blocks directory creation").unwrap();
        command
            .env("XDG_CACHE_HOME", cache_root)
            .env("XDG_DATA_HOME", data_root);
    }

    let output = command.output().unwrap();

    assert!(output.status.success());
    assert!(!output.stdout.is_empty());
    assert!(
        output.stderr.is_empty(),
        "hook prelude leaked stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
