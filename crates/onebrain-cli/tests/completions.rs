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

#[test]
fn init_yes_does_not_print_completions_hint() {
    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Shell completions").not());
}

fn completions_for(shell: &str) -> String {
    String::from_utf8(
        Command::cargo_bin("onebrain")
            .unwrap()
            .args(["completions", shell])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
}

#[test]
fn completions_exclude_hidden_top_level_commands() {
    // These hidden command names never appear anywhere in the visible tree
    // (not as candidates, not inside any help/description text), so a plain
    // substring check is a sound and strict guard for them. `migrate` is NOT
    // in this set: `plugin migrate` is a *visible* verb, so its name legitimately
    // appears — the hidden top-level `migrate` alias is covered structurally by
    // `completions_hidden_aliases_absent_from_top_level` below.
    let out = completions_for("zsh");
    for hidden in [
        "avatar",
        "daemon",
        "bundle",
        "serve",
        "session-init",
        "orphan-scan",
    ] {
        assert!(
            !out.contains(hidden),
            "hidden command `{hidden}` leaked into zsh completions"
        );
    }
}

#[test]
fn completions_keep_visible_commands() {
    let out = completions_for("zsh");
    for visible in ["init", "update", "doctor", "qmd", "schedule"] {
        assert!(
            out.contains(visible),
            "visible command `{visible}` missing from zsh completions"
        );
    }
}

/// Hardened replacement for the plan's weak `completions_hide_the_completions_command_itself`.
///
/// The original asserted `!out.contains("completions")`, which is bogus: the word
/// "completions" appears throughout the generated script's own function names. The
/// robust check is structural — extract the bash top-level candidate list (the
/// `opts="..."` line in the root `onebrain)` case arm, which lists exactly the
/// registered top-level subcommands) and assert that every hidden top-level name
/// (including `completions` itself and the legacy aliases like `migrate`) is absent
/// from it, while the visible ones are present. This pins the exact mechanism the
/// fix targets: hidden subcommands must not be registered as completion candidates.
#[test]
fn completions_hidden_aliases_absent_from_top_level() {
    let out = completions_for("bash");
    // The root command's candidate list is the `opts="..."` line that enumerates
    // the top-level subcommands. Several leaf contexts also emit `opts=`, so select
    // the one that holds the known visible top-level commands rather than the first.
    let opts_line = out
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("opts=\""))
        .find(|l| l.contains(" init ") && l.contains(" doctor "))
        .expect("bash script must contain a root opts= candidate list");

    // Hidden top-level names — including the `migrate`/`session-init`/... legacy
    // aliases and the hidden `completions` command — must NOT be candidates.
    for hidden in [
        "completions",
        "avatar",
        "daemon",
        "bundle",
        "serve",
        "session-init",
        "orphan-scan",
        "migrate",
        "vault-sync",
        "run-skill",
        "register-hooks",
        "register-schedule",
        "qmd-reindex",
    ] {
        // Match as a whole word in the space-separated candidate list.
        let leaked = opts_line
            .split_whitespace()
            .any(|tok| tok.trim_matches('"') == hidden);
        assert!(
            !leaked,
            "hidden top-level command `{hidden}` is a completion candidate: {opts_line}"
        );
    }

    // Sanity: the visible top-level commands ARE candidates.
    for visible in ["init", "update", "doctor", "qmd", "schedule", "vault", "session"] {
        let present = opts_line
            .split_whitespace()
            .any(|tok| tok.trim_matches('"') == visible);
        assert!(
            present,
            "visible top-level command `{visible}` missing from candidate list: {opts_line}"
        );
    }
}
