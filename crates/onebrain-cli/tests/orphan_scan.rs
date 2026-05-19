use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/orphan_scan")
        .join(name)
}

#[test]
fn empty_logs_emits_zero() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["orphan-scan", ".", "abc12345"])
        .current_dir(fixture("empty_logs"))
        .assert()
        .success()
        .stdout(predicate::str::is_match(r#"^\{"orphan_count":0\}\n?$"#).unwrap());
}

#[test]
fn current_session_token_filters_self() {
    // When `session_token == currtok`, the lone checkpoint must be skipped.
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["orphan-scan", ".", "currtok"])
        .current_dir(fixture("current_token_only"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"orphan_count\":0"));
}

#[test]
fn manual_session_log_skips_date() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["orphan-scan", ".", "abc12345"])
        .current_dir(fixture("manual_log_skip"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"orphan_count\":0"));
}
