use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/harness")
        .join(name)
}

#[test]
fn empty_dir_emits_direct() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .arg("harness")
        .current_dir(fixture("empty"))
        .env_remove("ONEBRAIN_HARNESS")
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""harnesses":["direct"]"#));
}

#[test]
fn claude_dir_emits_claude() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .arg("harness")
        .current_dir(fixture("with_claude"))
        .env_remove("ONEBRAIN_HARNESS")
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""harnesses":["claude"]"#));
}

#[test]
fn both_dirs_emit_claude_first() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .arg("harness")
        .current_dir(fixture("with_both"))
        .env_remove("ONEBRAIN_HARNESS")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            r#""harnesses":["claude","gemini"]"#,
        ));
}
