mod support;

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/checkpoint")
        .join(name)
}

/// A CLI invocation pinned to isolated cache + state (`$TMPDIR`) directories,
/// so a test's checkpoint state files can never collide with the developer's.
fn pinned(cache: &Path, state: &Path) -> Command {
    let mut command = Command::cargo_bin("onebrain").unwrap();
    command
        .env("ONEBRAIN_CACHE_DIR", cache)
        .env("TMPDIR", state)
        .env("TMP", state)
        .env("TEMP", state)
        .current_dir(fixture("empty_vault"));
    command
}

/// Reproduce what `onebrain hook` does on a Stop event: run the checkpoint
/// child with `ONEBRAIN_HOOK_SESSION_ID` set, so cadence state accrues under
/// the hash-16 token derived from the harness session id. Returns that
/// resolved token (read back from the CLI itself) and its state file path.
fn seed_hook_cadence(cache: &Path, state: &Path, session_id: &str) -> (String, PathBuf) {
    let output = pinned(cache, state)
        .env("ONEBRAIN_HOOK_SESSION_ID", session_id)
        .args(["session", "init", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let token = value["session_token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 16, "hook session ids resolve as sha256[..16]");

    pinned(cache, state)
        .env("ONEBRAIN_HOOK_SESSION_ID", session_id)
        .args(["checkpoint", "stop"])
        .assert()
        .success();

    let path = state_file(state, &token);
    let seeded = fs::read_to_string(&path).unwrap();
    assert!(
        seeded.starts_with("1:"),
        "expected one counted stop under the hook token, got {seeded}"
    );
    (token, path)
}

/// `$TMPDIR/onebrain-{token}.state` — the cadence state file for `token`.
fn state_file(state: &Path, token: &str) -> PathBuf {
    state.join(format!("onebrain-{token}.state"))
}

/// The agent shell that runs `/wrapup` never sees `ONEBRAIN_HOOK_SESSION_ID` —
/// only the hook runner injects it — so an env-resolved reset lands on a
/// different token. `--session-token` carries the already-resolved token
/// across that gap and zeroes the state file the cadence actually lives in.
#[test]
fn reset_with_session_token_clears_the_hook_resolved_state_file() {
    let cache = tempdir().unwrap();
    let state = tempdir().unwrap();
    let (hook_token, hook_state) =
        seed_hook_cadence(cache.path(), state.path(), "harness-chat-wrapup-a");

    pinned(cache.path(), state.path())
        .env_remove("ONEBRAIN_HOOK_SESSION_ID")
        .env_remove("CODEX_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env(
            "CLAUDE_CODE_SESSION_ID",
            "b947d3eb-3b17-4cd0-ba00-26ccd8d409cd",
        )
        .args(["checkpoint", "reset", "--session-token", &hook_token])
        .assert()
        .success();

    let after = fs::read_to_string(&hook_state).unwrap();
    assert!(
        after.starts_with("0:") && after.ends_with(":00"),
        "reset must zero the hook-resolved state file, got {after}"
    );
}

/// The divergence the flag exists to fix: without it, reset re-derives a token
/// from the agent shell's environment (here `CLAUDE_CODE_SESSION_ID`, an
/// 8-char truncation) and zeroes THAT state file, leaving the hash-16 counter
/// the hook has been incrementing untouched — so it survives every wrapup.
#[test]
fn reset_without_session_token_misses_the_hook_resolved_state_file() {
    let cache = tempdir().unwrap();
    let state = tempdir().unwrap();
    let (hook_token, hook_state) =
        seed_hook_cadence(cache.path(), state.path(), "harness-chat-wrapup-b");

    pinned(cache.path(), state.path())
        .env_remove("ONEBRAIN_HOOK_SESSION_ID")
        .env_remove("CODEX_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env(
            "CLAUDE_CODE_SESSION_ID",
            "b947d3eb-3b17-4cd0-ba00-26ccd8d409cd",
        )
        .args(["checkpoint", "reset"])
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(&hook_state).unwrap().split(':').next(),
        Some("1"),
        "an env-resolved reset must not have touched the hook token's counter"
    );
    assert!(
        state_file(state.path(), "b947d3eb").is_file(),
        "the env-resolved reset landed on the truncate-8 token instead"
    );
    assert_ne!(hook_token, "b947d3eb");
}

#[test]
fn reset_rejects_a_session_token_with_no_alphanumeric_characters() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["checkpoint", "reset", "--session-token", "..."])
        .current_dir(fixture("empty_vault"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("session token override"));
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
