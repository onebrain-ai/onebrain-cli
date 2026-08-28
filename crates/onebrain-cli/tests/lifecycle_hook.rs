//! Cross-harness lifecycle hook integration tests.

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
session_id = os.environ.get("ONEBRAIN_HOOK_SESSION_ID", "")
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
    if not os.environ.get("FAKE_CHECKPOINT_SILENT"):
        print(json.dumps({"decision": "block", "reason": "15 since start"}))
elif args[:2] == ["search", "reindex"]:
    if os.environ.get("FAKE_REINDEX_FILE"):
        with open(os.environ["FAKE_REINDEX_FILE"], "w", encoding="utf-8") as handle:
            handle.write(" ".join(args))
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

fn hook_command(fake: &Path, cache: &Path, event: &str, session_id: &str) -> Command {
    let mut command = Command::cargo_bin("onebrain").unwrap();
    command
        .arg("hook")
        .env("ONEBRAIN_BIN", fake)
        .env("ONEBRAIN_CACHE_DIR", cache)
        .write_stdin(
            serde_json::json!({
                "session_id": session_id,
                "transcript_path": "/tmp/session.jsonl",
                "cwd": "/tmp/vault",
                "hook_event_name": event,
                "timestamp": "2026-08-26T10:00:00Z",
                "model": "gpt-5.6-sol",
                "permission_mode": "default"
            })
            .to_string(),
        );
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

    let output = hook_command(&fake, temp.path(), "SessionStart", "large-output")
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
        "SessionStart",
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
        "SessionStart",
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

    let output = hook_command(&fake, temp.path(), "SessionStart", "stale-override")
        .env("FAKE_VERSION", "3.4.24")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn compatible_override_version_probe_uses_the_foreground_budget() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());

    let output = hook_command(&fake, temp.path(), "SessionStart", "loaded-host")
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

    let output = hook_command(&fake, temp.path(), "PostToolUse", "background-fast")
        .env("FAKE_VERSION_PROBE_FILE", &probe_file)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
    assert!(
        !probe_file.exists(),
        "background hook spawned an unnecessary version probe"
    );
}

#[test]
fn stop_forwards_checkpoint_and_suppresses_pending_output() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());

    let checkpoint = hook_command(&fake, temp.path(), "Stop", "protocol")
        .output()
        .unwrap();
    let lex = hook_command(&fake, temp.path(), "AfterTool", "protocol")
        .output()
        .unwrap();

    assert_eq!(
        String::from_utf8(checkpoint.stdout).unwrap(),
        "{\"decision\": \"block\", \"reason\": \"15 since start\"}\n"
    );
    assert_eq!(lex.stdout, b"{}\n");
}

/// End of session: the Stop hook must ALSO dispatch the deferred embedding
/// pass, not just the checkpoint. The fake child records its own argv, so a
/// dropped `search reindex --pending-only` spawn leaves no marker behind.
#[test]
fn stop_dispatches_the_pending_embed_child() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());
    let reindex_file = temp.path().join("pending-embed.args");

    let output = hook_command(&fake, temp.path(), "Stop", "pending-embed")
        .env("FAKE_REINDEX_FILE", &reindex_file)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(&reindex_file)
            .expect("Stop must dispatch the pending-embed child")
            .trim(),
        "search reindex --pending-only --json"
    );
    // The background child's stdout still never reaches the harness — only
    // the foreground checkpoint's protocol JSON does.
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "{\"decision\": \"block\", \"reason\": \"15 since start\"}\n"
    );
}

#[test]
fn silent_stop_emits_an_empty_json_object() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());

    let output = hook_command(&fake, temp.path(), "AfterAgent", "quiet-stop")
        .env("FAKE_CHECKPOINT_SILENT", "1")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
}

#[test]
fn timed_out_child_is_killed_reaped_and_fails_open() {
    let temp = TempDir::new().unwrap();
    let fake = fake_onebrain(temp.path());
    let pid_file = temp.path().join("child.pid");
    let completed_file = temp.path().join("child.completed");
    let started = Instant::now();

    let output = hook_command(&fake, temp.path(), "SessionStart", "timeout")
        .env("FAKE_SLEEP_SECS", "10")
        .env("FAKE_PID_FILE", &pid_file)
        .env("FAKE_COMPLETED_FILE", &completed_file)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
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
        .arg("hook")
        .env("ONEBRAIN_BIN", &fake)
        .env_remove("ONEBRAIN_CACHE_DIR")
        .write_stdin(
            serde_json::json!({
                "session_id": "quiet-prelude",
                "hook_event_name": "SessionStart"
            })
            .to_string(),
        );

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
