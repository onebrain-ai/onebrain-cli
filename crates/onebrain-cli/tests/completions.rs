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

#[test]
fn completions_bash_emits_function_marker() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("_onebrain()"));
}

#[test]
fn completions_fish_emits_complete_marker() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["completions", "fish"])
        .assert()
        .success()
        .stdout(predicate::str::contains("complete -c onebrain"));
}

#[test]
fn completions_rejects_unknown_shell() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["completions", "tcsh"])
        .assert()
        .failure();
}
