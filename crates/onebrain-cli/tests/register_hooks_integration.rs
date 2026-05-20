//! Layer 2 integration tests for `onebrain register-hooks` — spawns the real
//! binary via `assert_cmd` against synthetic vaults in tempdirs. Verifies
//! exit codes, stdout messages, and the on-disk `.claude/settings.json`
//! shape after the command runs.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write_vault(dir: &Path, claude_dir: bool, qmd_collection: Option<&str>) {
    if claude_dir {
        fs::create_dir_all(dir.join(".claude")).unwrap();
    }
    let yml = match qmd_collection {
        Some(c) => format!("qmd_collection: {c}\n"),
        None => "method: onebrain\n".to_string(),
    };
    fs::write(dir.join("vault.yml"), yml).unwrap();
}

fn read_settings(dir: &Path) -> Value {
    let text = fs::read_to_string(dir.join(".claude").join("settings.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn cli_fresh_vault_writes_settings() {
    let d = tempdir().unwrap();
    write_vault(d.path(), true, None);
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-hooks", "--vault", d.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stop added"))
        .stdout(predicate::str::contains("permissions added = 14"))
        .stdout(predicate::str::contains("done"));
    let s = read_settings(d.path());
    assert!(s["hooks"]["Stop"].is_array());
    let perms = s["permissions"]["allow"].as_array().unwrap();
    assert_eq!(perms.len(), 14);
}

#[test]
fn cli_existing_canonical_idempotent() {
    let d = tempdir().unwrap();
    write_vault(d.path(), true, None);
    // First run installs.
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-hooks", "--vault", d.path().to_str().unwrap()])
        .assert()
        .success();
    // Second run is idempotent.
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-hooks", "--vault", d.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stop ok"))
        .stdout(predicate::str::contains("permissions added = 0"));
}

#[test]
fn cli_foreign_hooks_preserved() {
    let d = tempdir().unwrap();
    write_vault(d.path(), true, None);
    // Pre-seed with a user UserPromptSubmit hook that should survive.
    fs::write(
        d.path().join(".claude").join("settings.json"),
        serde_json::to_string(&serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "matcher": "",
                    "hooks": [
                        {"type": "command", "command": "onebrain checkpoint user-prompt-submit"},
                        {"type": "command", "command": "my-user-script.sh"},
                    ]
                }]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-hooks", "--vault", d.path().to_str().unwrap()])
        .assert()
        .success();
    let s = read_settings(d.path());
    let ups = &s["hooks"]["UserPromptSubmit"];
    assert!(ups.is_array(), "UserPromptSubmit preserved");
    let entries = ups[0]["hooks"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["command"], "my-user-script.sh");
}

#[test]
fn cli_dry_run_does_not_write() {
    let d = tempdir().unwrap();
    write_vault(d.path(), true, None);
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "register-hooks",
            "--vault",
            d.path().to_str().unwrap(),
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stop added"))
        .stdout(predicate::str::contains("dry-run"));
    // No settings.json on disk.
    assert!(!d.path().join(".claude").join("settings.json").exists());
}

#[test]
fn cli_remove_strips_state() {
    let d = tempdir().unwrap();
    write_vault(d.path(), true, None);
    // Install first.
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-hooks", "--vault", d.path().to_str().unwrap()])
        .assert()
        .success();
    // Now strip.
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "register-hooks",
            "--vault",
            d.path().to_str().unwrap(),
            "--remove",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed"))
        .stdout(predicate::str::contains("permissions removed = 14"));
    let s = read_settings(d.path());
    assert!(
        s.get("hooks").is_none() || s["hooks"].get("Stop").is_none(),
        "Stop event should be gone after --remove"
    );
    let allow = s["permissions"]["allow"].as_array().unwrap();
    assert!(!allow.iter().any(|v| v.as_str() == Some("Bash(onebrain *)")));
}

#[test]
fn cli_no_claude_dir_is_noop() {
    let d = tempdir().unwrap();
    write_vault(d.path(), false, None);
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-hooks", "--vault", d.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("no claude harness"));
    assert!(!d.path().join(".claude").join("settings.json").exists());
}

#[test]
fn cli_qmd_collection_set_adds_post_tool_use() {
    let d = tempdir().unwrap();
    write_vault(d.path(), true, Some("ob-1-test"));
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-hooks", "--vault", d.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("PostToolUse added"));
    let s = read_settings(d.path());
    assert!(s["hooks"]["PostToolUse"].is_array());
    assert_eq!(s["hooks"]["PostToolUse"][0]["matcher"], "Write|Edit");
}

/// `--vault-dir` is a Bun v2.3.3-parity alias for `--vault` — same behavior.
#[test]
fn cli_vault_dir_alias_resolves_to_vault() {
    let d = tempdir().unwrap();
    write_vault(d.path(), true, None);
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "register-hooks",
            "--vault-dir",
            d.path().to_str().unwrap(),
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stop"))
        .stdout(predicate::str::contains("dry-run"));
}
