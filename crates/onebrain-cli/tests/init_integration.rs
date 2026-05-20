//! Layer 2 integration tests for `onebrain init` — spawns the real binary
//! via `assert_cmd` with the cwd set to a fresh tempdir. Uses `--yes` so
//! the run is non-interactive and pulls the Essentials schedule preset by
//! default.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

#[test]
fn cli_yes_fresh_vault_writes_files_and_emits_header() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("OneBrain Init"))
        .stdout(predicate::str::contains("vault.yml: written"))
        .stdout(predicate::str::contains("folders:"))
        .stdout(predicate::str::contains("done"));

    // vault.yml created
    assert!(d.path().join("vault.yml").is_file());
    let content = fs::read_to_string(d.path().join("vault.yml")).unwrap();
    assert!(content.contains("update_channel: stable"));
    assert!(content.contains("schedule:"));

    // All 8 folders + imports
    for folder in [
        "00-inbox",
        "01-projects",
        "02-areas",
        "03-knowledge",
        "04-resources",
        "05-agent",
        "06-archive",
        "07-logs",
    ] {
        assert!(d.path().join(folder).is_dir(), "missing folder {folder}");
    }
    assert!(d.path().join("00-inbox").join("imports").is_dir());
}

#[test]
fn cli_yes_existing_vault_yml_returns_non_zero() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("vault.yml"), "old: value\n").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("vault.yml exists"))
        .stdout(predicate::str::contains("--force"));

    // Original content preserved
    let content = fs::read_to_string(d.path().join("vault.yml")).unwrap();
    assert_eq!(content, "old: value\n");
    // No folders created
    assert!(!d.path().join("00-inbox").exists());
}

#[test]
fn cli_yes_creates_schedule_block_with_essentials_entries() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success();

    let content = fs::read_to_string(d.path().join("vault.yml")).unwrap();
    assert!(content.contains("/daily"));
    assert!(content.contains("/weekly"));
    assert!(content.contains("/recap"));
}

#[test]
fn cli_yes_emits_essentials_preset_line() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("preset: Essentials"));
}

#[test]
fn cli_yes_run_twice_second_fails_without_force() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .failure();
}

/// Regression: `onebrain init --yes` in an empty directory must populate
/// `.claude/settings.json` with the Stop hook. Before the fix, init's
/// best-effort register-hooks call no-oped because `.claude/` did not exist
/// yet, so it printed "hooks: ok" without writing settings.json.
#[test]
fn cli_yes_populates_claude_settings_json_with_stop_hook() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("hooks: ok"));

    let settings_path = d.path().join(".claude").join("settings.json");
    assert!(
        settings_path.is_file(),
        ".claude/settings.json missing after init"
    );
    let text = fs::read_to_string(&settings_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("settings.json is valid JSON");
    let stop = v
        .get("hooks")
        .and_then(|h| h.get("Stop"))
        .and_then(|s| s.as_array())
        .expect("hooks.Stop array missing");
    assert!(!stop.is_empty(), "hooks.Stop is empty: {text}");
}

/// Bun v2.3.3-parity: `--force` overrides the existing-vault.yml guard so
/// the run succeeds and the file is rewritten without prompting.
#[test]
fn cli_yes_force_overwrites_existing_vault_yml() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("vault.yml"), "old: value\n").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--force", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("vault.yml: written"));
    let content = fs::read_to_string(d.path().join("vault.yml")).unwrap();
    assert!(
        content.contains("update_channel"),
        "vault.yml should have been overwritten with the fresh template",
    );
}

/// Bun v2.3.3-parity: `--vault-dir <path>` targets a directory other than
/// cwd. The binary should write the vault scaffold into that directory.
#[test]
fn cli_yes_vault_dir_targets_explicit_path() {
    let d = tempdir().unwrap();
    let target = d.path().join("nested-vault");
    fs::create_dir_all(&target).unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "init",
            "--yes",
            "--no-sync",
            "--vault-dir",
            target.to_str().unwrap(),
        ])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("done"));
    assert!(target.join("vault.yml").is_file());
    assert!(target.join("07-logs").is_dir());
}
