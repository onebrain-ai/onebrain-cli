//! Layer 2 integration tests for `onebrain migrate` — exercises the wired
//! CLI end-to-end against synthetic vaults in tempdirs. Mirrors the
//! corresponding `migrate.test.ts` (Bun) scenarios.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write_minimal_vault(dir: &Path) {
    fs::write(
        dir.join("vault.yml"),
        "folders:\n  \
           inbox: 00-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("07-logs")).unwrap();
}

#[test]
fn migrate_empty_logs_outputs_zero_summary() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .args(["migrate", "backfill-recapped"])
        .assert()
        .success()
        .stdout("backfilled: 0 files, skipped: 0\n");
}

/// Bun v2.3.3 positional `<cutoff_date>` argument is accepted and used as the
/// inclusive cutoff (mirrors the Rust-form `--cutoff <date>` flag).
#[test]
fn migrate_accepts_positional_cutoff_date() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .args(["migrate", "backfill-recapped", "2026-01-01"])
        .assert()
        .success()
        .stdout(predicate::str::contains("backfilled:"));
}

/// clap `conflicts_with = "cutoff_date"` rejects the user supplying both
/// positional `<cutoff_date>` AND `--cutoff <date>` so neither value is
/// silently dropped.
#[test]
fn migrate_rejects_both_positional_and_flag_cutoff() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .args([
            "migrate",
            "backfill-recapped",
            "2026-01-01",
            "--cutoff",
            "2026-06-01",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap_or_default();
    assert!(
        stderr.contains("cannot be used")
            || stderr.contains("conflicts")
            || stderr.contains("the argument"),
        "stderr must explain the conflict · got: {stderr}"
    );
}

#[test]
fn migrate_backfills_session_log_missing_recapped() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let month = d.path().join("07-logs/session/2026/04");
    fs::create_dir_all(&month).unwrap();
    let log = month.join("2026-04-20-session-01.md");
    fs::write(
        &log,
        "---\ntags: session-log\ndate: '2026-04-20'\n---\n\n## Session\n\nbody\n",
    )
    .unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .args(["migrate", "backfill-recapped"])
        .assert()
        .success()
        .stdout("backfilled: 1 files, skipped: 0\n");

    let after = fs::read_to_string(&log).unwrap();
    assert!(after.contains("recapped:"));
}

#[test]
fn migrate_unknown_name_warns_exits_0() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .args(["migrate", "not-a-real-migration"])
        .assert()
        .success()
        .stderr(predicate::str::contains("unknown migration"))
        .stdout(predicate::str::is_empty());
}

#[test]
fn migrate_missing_vault_yml_warns_exits_0() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .args(["migrate", "backfill-recapped"])
        .assert()
        .success()
        .stderr(predicate::str::contains("not inside a vault"))
        .stdout(predicate::str::is_empty());
}

#[test]
fn migrate_cutoff_flag_respected() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let month = d.path().join("07-logs/session/2026/04");
    fs::create_dir_all(&month).unwrap();
    fs::write(
        month.join("2026-04-20-session-01.md"),
        "---\ntags: session-log\n---\n\nbody\n",
    )
    .unwrap();
    fs::write(
        month.join("2026-04-25-session-01.md"),
        "---\ntags: session-log\n---\n\nbody\n",
    )
    .unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .args(["migrate", "backfill-recapped", "--cutoff", "2026-04-22"])
        .assert()
        .success()
        .stdout("backfilled: 1 files, skipped: 0\n");

    let older = fs::read_to_string(month.join("2026-04-20-session-01.md")).unwrap();
    let newer = fs::read_to_string(month.join("2026-04-25-session-01.md")).unwrap();
    assert!(older.contains("recapped:"));
    assert!(!newer.contains("recapped:"));
}

#[test]
fn migrate_vault_flag_overrides_cwd() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let month = d.path().join("07-logs/session/2026/04");
    fs::create_dir_all(&month).unwrap();
    fs::write(
        month.join("2026-04-20-session-01.md"),
        "---\ntags: session-log\n---\n\nbody\n",
    )
    .unwrap();
    let elsewhere = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(elsewhere.path())
        .args([
            "migrate",
            "backfill-recapped",
            "--vault",
            d.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout("backfilled: 1 files, skipped: 0\n");
}
