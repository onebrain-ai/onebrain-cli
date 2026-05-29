use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn completions_zsh_emits_compdef_marker() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("#compdef onebrain"));
}
