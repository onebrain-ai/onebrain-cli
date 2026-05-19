//! Layer 2 integration tests for `onebrain register-schedule`. Exercises
//! the wired CLI end-to-end (assert_cmd spawns the real binary) against
//! synthetic vaults built in tempdirs.
//!
//! All tests that touch `~/Library/LaunchAgents` set `HOME=<tempdir>` so
//! the real LaunchAgents directory is untouched. `dirs::home_dir()` reads
//! `$HOME` on Unix, which is the standard isolation pattern.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::tempdir;

fn write_skill(vault: &Path, name: &str, frontmatter: &str) {
    let dir = vault.join(format!(".claude/plugins/onebrain/skills/{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), format!("---\n{frontmatter}\n---\n")).unwrap();
}

fn write_skill_vault(yaml: &str) -> tempfile::TempDir {
    let v = tempdir().unwrap();
    std::fs::write(v.path().join("vault.yml"), yaml).unwrap();
    write_skill(v.path(), "daily", "name: daily\nschedulable: true");
    v
}

/// `--dry-run` prints the plist for a recurring skill entry without writing
/// to disk.
#[test]
fn dry_run_emits_plist_to_stdout() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("com.onebrain.daily"))
        .stdout(predicate::str::contains("StartCalendarInterval"));
}

/// `--dry-run` of a one-shot entry emits `Year/Month/Day/Hour/Minute` keys
/// plus the self-deleting shell wrapper.
#[test]
fn one_shot_dry_run_emits_year_field_and_self_delete_wrapper() {
    let v = write_skill_vault("schedule:\n  - at: \"2026-05-13 14:30\"\n    skill: /daily\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("<key>Year</key>"))
        .stdout(predicate::str::contains("<key>Day</key>"))
        .stdout(predicate::str::contains("launchctl bootout"))
        .stdout(predicate::str::contains("rm -f"));
}

/// `--status` reports each entry with the `[cron]` tag.
#[test]
fn status_lists_entries_with_cron_tag() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-schedule", "--status"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Registered schedules: 1"))
        .stdout(predicate::str::contains("[cron]"));
}

/// `--remove` deletes the plist files for entries in vault.yml. Run
/// `--dry-run` is NOT used here — we actually write to a tempdir HOME and
/// verify the file disappears.
#[test]
fn remove_deletes_plists_from_launch_agents() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n");
    let home = tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("Library/LaunchAgents")).unwrap();

    // First write the plist (no --dry-run).
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-schedule"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .assert()
        .success();
    let plist = home
        .path()
        .join("Library/LaunchAgents/com.onebrain.daily.plist");
    assert!(plist.exists(), "expected plist at {}", plist.display());

    // Now remove it.
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-schedule", "--remove"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{2713} Removed"));
    assert!(!plist.exists(), "plist should be removed");
}

/// `--resume <skill>` clears the `.paused/<skill>.txt` marker if present.
#[test]
fn resume_clears_paused_marker_file() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n");
    let paused = v.path().join("07-logs/scheduler/.paused");
    std::fs::create_dir_all(&paused).unwrap();
    let marker = paused.join("daily.txt");
    std::fs::write(&marker, "paused at 2026-05-19\n").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-schedule", "--resume", "/daily"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{2713} Resumed /daily"));
    assert!(!marker.exists(), "paused marker should be cleared");
}

/// Skill+command collision (same basename) is rejected with the verbatim
/// Bun error string.
#[test]
fn collision_skill_and_command_with_same_basename_fails() {
    let v = tempdir().unwrap();
    write_skill(v.path(), "echo", "name: echo\nschedulable: true");
    std::fs::write(
        v.path().join("vault.yml"),
        "schedule:\n  \
         - cron: \"0 9 * * *\"\n    skill: /echo\n  \
         - cron: \"0 3 * * 0\"\n    command: /bin/echo\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("normalize to the same plist path"));
}

/// Recurring command-mode entry produces a hook-style argv plist (no
/// `--skill` / `--vault` / `run-skill`).
#[test]
fn command_mode_dry_run_produces_hook_style_argv() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("vault.yml"),
        "schedule:\n  - cron: \"0 3 * * 0\"\n    command: /bin/echo\n    args:\n      - hello\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("<string>/bin/echo</string>"))
        .stdout(predicate::str::contains("<string>hello</string>"))
        .stdout(predicate::str::contains("<string>--skill</string>").not());
}

/// Skill marked `schedulable: false` is rejected at register time.
#[test]
fn unschedulable_skill_rejected() {
    let v = tempdir().unwrap();
    write_skill(v.path(), "daily", "name: daily\nschedulable: false");
    std::fs::write(
        v.path().join("vault.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires user input"));
}

/// One-shot args containing shell-special chars are rejected.
#[test]
fn one_shot_command_rejects_shell_special_chars() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("vault.yml"),
        "schedule:\n  - at: \"2026-05-13 14:30\"\n    command: /bin/echo\n    args:\n      - \"$EVIL\"\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("shell-special"));
}
