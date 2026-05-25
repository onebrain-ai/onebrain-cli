use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn session_init_emits_required_fields_in_minimal_vault() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .arg("session-init")
        .current_dir(fixture("minimal_vault"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"datetime\":"))
        .stdout(predicate::str::contains("\"session_token\":"))
        .stdout(predicate::str::contains("\"qmd_unembedded\":"))
        .stdout(predicate::str::contains("\"decision\":").not());
}

#[test]
fn session_init_emits_block_outside_vault() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .arg("session-init")
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"decision\":\"block\""))
        .stdout(predicate::str::contains(
            "\"reason\":\"onebrain-init-required\"",
        ));
}

/// R1 C2: malformed vault.yml now emits the distinct
/// `onebrain-vault-malformed` reason (was previously collapsed to
/// `onebrain-init-required`). Still exits 0 with a block JSON so
/// SessionStart can surface "fix your vault.yml" instead of "/onboarding".
#[test]
fn session_init_emits_block_on_malformed_vault_yml() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .arg("session-init")
        .current_dir(fixture("malformed_vault"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"decision\":\"block\""))
        .stdout(predicate::str::contains(
            "\"reason\":\"onebrain-vault-malformed\"",
        ))
        .stdout(predicate::str::contains("\"error_detail\":"));
}
