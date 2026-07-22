mod support;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/harness")
        .join(name)
}

#[test]
fn empty_dir_emits_direct_json() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["harness", "detect", "--json"])
        .current_dir(fixture("empty"))
        .env_remove("ONEBRAIN_HARNESS")
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""harnesses":["direct"]"#));
}

#[test]
fn claude_dir_emits_claude_json() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["harness", "detect", "--json"])
        .current_dir(fixture("with_claude"))
        .env_remove("ONEBRAIN_HARNESS")
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""harnesses":["claude"]"#));
}

#[test]
fn both_dirs_emit_claude_first_json() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["harness", "detect", "--json"])
        .current_dir(fixture("with_both"))
        .env_remove("ONEBRAIN_HARNESS")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            r#""harnesses":["claude","gemini"]"#,
        ));
}

/// v3.1 contract: `onebrain harness` with no flag must emit text, not JSON.
/// Previously it emitted JSON unconditionally; now machine consumers must
/// pass `--json` (or `--yaml` / `--output <fmt>`) explicitly.
#[test]
fn default_mode_is_text_not_json() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["harness", "detect"])
        .current_dir(fixture("empty"))
        .env_remove("ONEBRAIN_HARNESS")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("Detected harnesses:"))
        .stdout(predicate::str::contains("direct"))
        .stdout(predicate::str::starts_with("{").not());
}

#[test]
fn yaml_mode_emits_yaml_not_json() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["harness", "detect", "--yaml"])
        .current_dir(fixture("with_claude"))
        .env_remove("ONEBRAIN_HARNESS")
        .assert()
        .success()
        .stdout(predicate::str::contains("harnesses:"))
        .stdout(predicate::str::starts_with("{").not());
}

#[test]
fn pretty_json_is_indented_multiline() {
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["harness", "detect", "--json", "--pretty"])
        .current_dir(fixture("with_claude"))
        .env_remove("ONEBRAIN_HARNESS")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains('\n'),
        "pretty JSON must be multi-line: {stdout}"
    );
    assert!(
        stdout.contains("  "),
        "pretty JSON must be indented: {stdout}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert!(parsed.get("harnesses").is_some());
}
