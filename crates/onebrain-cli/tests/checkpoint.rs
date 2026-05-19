use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/checkpoint")
        .join(name)
}

#[test]
fn reset_emits_no_stdout() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["checkpoint", "reset"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn unknown_mode_emits_stderr_warning() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["checkpoint", "xyzbadmode"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stderr(predicate::str::contains("unknown mode"));
}

#[test]
fn stop_with_fresh_state_emits_no_stdout() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["checkpoint", "stop"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}
