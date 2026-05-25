use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/orphan_scan")
        .join(name)
}

// v3.1: default output is text. JSON shape now requires `--json`. These
// tests pass `--json` explicitly to assert the machine-consumer contract.
// Default-text behaviour has its own regression guard at the bottom.

#[test]
fn empty_logs_emits_zero_with_json_flag() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["orphan-scan", ".", "abc12345", "--json"])
        .current_dir(fixture("empty_logs"))
        .assert()
        .success()
        .stdout(predicate::str::is_match(r#"^\{"orphan_count":0\}\n?$"#).unwrap());
}

#[test]
fn current_session_token_filters_self_with_json_flag() {
    // When `session_token == currtok`, the lone checkpoint must be skipped.
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["orphan-scan", ".", "currtok", "--json"])
        .current_dir(fixture("current_token_only"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"orphan_count\":0"));
}

#[test]
fn manual_session_log_skips_date_with_json_flag() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["orphan-scan", ".", "abc12345", "--json"])
        .current_dir(fixture("manual_log_skip"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"orphan_count\":0"));
}

/// v3.1 contract: default mode (no flag) emits text — explicit guard
/// against regression to JSON-by-default for the hook-protocol commands.
#[test]
fn default_emits_text_not_json() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["orphan-scan", ".", "abc12345"])
        .current_dir(fixture("empty_logs"))
        .assert()
        .success()
        .stdout(predicate::str::starts_with("{").not())
        .stdout(predicate::str::contains("orphan checkpoint"));
}
