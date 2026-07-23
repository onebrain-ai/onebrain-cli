mod support;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;
use tempfile::tempdir;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/checkpoint")
        .join(name)
}

#[test]
fn reset_emits_no_stdout() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["checkpoint", "reset"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn unknown_mode_is_rejected_by_clap_in_v31() {
    // v3.0 used a positional `mode` argument and emitted an "unknown mode"
    // stderr warning at exit 0 for an unrecognized value. v3.1 promotes
    // `stop` / `reset` / `orphans` to proper clap subcommands, so an
    // unknown verb now fails at parse time with clap's standard exit 2.
    // This is the correct, tighter semantic for the v3.1 consistency
    // standard.
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["checkpoint", "xyzbadmode"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn stop_with_fresh_state_emits_no_stdout() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["checkpoint", "stop"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn codex_stop_state_is_isolated_per_chat_session_id() {
    let cache = tempdir().unwrap();
    let state = tempdir().unwrap();
    let vault = fixture("empty_vault");
    let token_for = |session_id: &str| {
        let output = Command::cargo_bin("onebrain")
            .unwrap()
            .env("ONEBRAIN_CACHE_DIR", cache.path())
            .env("TMPDIR", state.path())
            .env("TMP", state.path())
            .env("TEMP", state.path())
            .env("CODEX_SESSION_ID", session_id)
            .args(["session", "init", "--json"])
            .current_dir(&vault)
            .output()
            .unwrap();
        assert!(output.status.success());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        value["session_token"].as_str().unwrap().to_owned()
    };

    let first = token_for("same-prefix-chat-a");
    let second = token_for("same-prefix-chat-b");
    for session_id in ["same-prefix-chat-a", "same-prefix-chat-b"] {
        Command::cargo_bin("onebrain")
            .unwrap()
            .env("ONEBRAIN_CACHE_DIR", cache.path())
            .env("TMPDIR", state.path())
            .env("TMP", state.path())
            .env("TEMP", state.path())
            .env("CODEX_SESSION_ID", session_id)
            .args(["checkpoint", "stop"])
            .current_dir(&vault)
            .assert()
            .success();
    }

    assert_ne!(first, second);
    assert!(state
        .path()
        .join(format!("onebrain-{first}.state"))
        .is_file());
    assert!(state
        .path()
        .join(format!("onebrain-{second}.state"))
        .is_file());
}
