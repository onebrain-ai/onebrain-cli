mod support;

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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["checkpoint", "reset"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn unknown_mode_is_rejected_by_clap_in_v31() {
    // v3.0 used a positional `mode` argument and emitted an "unknown mode"
    // stderr warning at exit 0 for an unrecognized value. v3.1 promotes
    // `stop` / `reset` / `orphans` to proper clap subcommands, so an
    // unknown verb now fails at parse time with clap's standard exit 2.
    // This is the correct, tighter semantic for the v3.1 consistency
    // standard.
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["checkpoint", "xyzbadmode"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn stop_with_fresh_state_emits_no_stdout() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["checkpoint", "stop"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}
