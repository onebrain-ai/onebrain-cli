use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/qmd_reindex")
        .join(name)
}

#[test]
fn no_collection_emits_no_stdout() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .arg("qmd-reindex")
        .current_dir(fixture("no_collection"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn missing_vault_yml_emits_no_stdout() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .arg("qmd-reindex")
        .current_dir(fixture("no_vault"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}
