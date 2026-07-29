//! Layer 2 integration tests for `onebrain register-schedule`. Exercises
//! the wired CLI end-to-end (assert_cmd spawns the real binary) against
//! synthetic vaults built in tempdirs.
//!
//! All tests that touch `~/Library/LaunchAgents` set `HOME=<tempdir>` so
//! the real LaunchAgents directory is untouched. `dirs::home_dir()` reads
//! `$HOME` on Unix, which is the standard isolation pattern.

mod support;

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
// macOS-only since Task 6: asserts launchd plist text; other platforms
// preview their own artifact (or an honest note) via render_preview [M5].
#[cfg(target_os = "macos")]
#[test]
fn dry_run_emits_plist_to_stdout() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("com.onebrain.daily"))
        .stdout(predicate::str::contains("StartCalendarInterval"));
}

/// `--dry-run` of a one-shot entry emits `Year/Month/Day/Hour/Minute` keys
/// plus the self-deleting shell wrapper.
// macOS-only since Task 6: asserts launchd plist text; other platforms
// preview their own artifact (or an honest note) via render_preview [M5].
#[cfg(target_os = "macos")]
#[test]
fn one_shot_dry_run_emits_year_field_and_self_delete_wrapper() {
    let v = write_skill_vault("schedule:\n  - at: \"2026-05-13 14:30\"\n    skill: /daily\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--status"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("Registered schedules: 1"))
        .stdout(predicate::str::contains("[cron]"));
}

/// `--remove` deletes the plist files for entries in vault.yml. Run
/// `--dry-run` is NOT used here — we actually write to a tempdir HOME and
/// verify the file disappears.
///
/// macOS/Unix-only: launchd is macOS-only, so the plist write path is
/// gated to Unix-style HOME layouts.
// macOS-only since Task 6: registers/renders launchd artifacts, which
// Linux now refuses (#313) and Windows renders differently.
#[cfg(target_os = "macos")]
#[test]
fn remove_deletes_plists_from_launch_agents() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n");
    let home = tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("Library/LaunchAgents")).unwrap();

    // First write the plist (no --dry-run).
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success();
    let plist = home
        .path()
        .join("Library/LaunchAgents/com.onebrain.daily.plist");
    assert!(plist.exists(), "expected plist at {}", plist.display());

    // Now remove it.
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--remove"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--resume", "/daily"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{2713} Resumed /daily"));
    assert!(!marker.exists(), "paused marker should be cleared");
}

/// Prior to #116 bug 2, a command-mode label was the binary basename alone,
/// so a skill `/echo` and a command `/bin/echo` on different schedules
/// collided on the same plist path (`com.onebrain.echo`) even though they
/// were entirely unrelated entries. Command-mode labels now always carry an
/// args- or cron-derived discriminator (every valid entry has a cron/at),
/// so this basename-only false-positive collision no longer happens — the
/// two entries register cleanly onto distinct plist paths.
///
/// Unix-only: relies on `/bin/echo` existing.
#[cfg(unix)]
#[test]
fn skill_and_command_with_same_basename_no_longer_collide() {
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("---  echo  ---"))
        .stdout(predicate::str::contains("---  echo-0-3-----0  ---"));
}

/// Two `command:` entries sharing a binary basename but with different args
/// must land on distinct plist paths (#116 bug 2 core regression test).
///
/// Unix-only: relies on `/bin/echo` existing.
#[cfg(unix)]
#[test]
fn command_mode_entries_same_binary_different_args_no_collision() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("vault.yml"),
        "schedule:\n  \
         - cron: \"0 9 * * *\"\n    command: /bin/echo\n    args:\n      - hello\n  \
         - cron: \"0 9 * * *\"\n    command: /bin/echo\n    args:\n      - world\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("---  echo-hello  ---"))
        .stdout(predicate::str::contains("---  echo-world  ---"));
}

/// Two `command:` entries with IDENTICAL command + args + cron are a
/// genuine duplicate and must still be rejected as a collision.
///
/// Unix-only: relies on `/bin/echo` existing.
#[cfg(unix)]
#[test]
fn command_mode_entries_fully_identical_still_collide() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("vault.yml"),
        "schedule:\n  \
         - cron: \"0 9 * * *\"\n    command: /bin/echo\n    args:\n      - hello\n  \
         - cron: \"0 9 * * *\"\n    command: /bin/echo\n    args:\n      - hello\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "normalize to the same schedule artifact",
        ));
}

/// Recurring command-mode entry for a GENERIC binary produces a hook-style
/// argv plist (no `--skill` / `run-skill`) and — #263 R2 — must NOT get a
/// `--vault` appended. `/bin/echo` stands in for any non-onebrain command
/// (rsync, etc.): appending `--vault` there would break the job.
///
/// Unix-only: relies on `/bin/echo` existing.
// macOS-only since Task 6: registers/renders launchd artifacts, which
// Linux now refuses (#313) and Windows renders differently.
#[cfg(target_os = "macos")]
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("<string>/bin/echo</string>"))
        .stdout(predicate::str::contains("<string>hello</string>"))
        .stdout(predicate::str::contains("<string>--skill</string>").not())
        .stdout(predicate::str::contains("<string>--vault</string>").not());
}

/// Recurring command-mode entry whose command IS the onebrain binary DOES
/// get `--vault <vault_path>` appended (#263 bug 1): launchd runs jobs with
/// cwd=/, so onebrain needs the vault path in its own argv. We point
/// `command:` at the built onebrain test binary by absolute path — the
/// register path resolves it (it exists) and its basename `onebrain` is
/// detected as the onebrain binary.
// macOS-only since Task 6: registers/renders launchd artifacts, which
// Linux now refuses (#313) and Windows renders differently.
#[cfg(target_os = "macos")]
#[test]
fn command_mode_onebrain_dry_run_embeds_vault() {
    let v = tempdir().unwrap();
    let onebrain_bin = assert_cmd::cargo::cargo_bin("onebrain");
    let onebrain_str = onebrain_bin.to_str().unwrap();
    std::fs::write(
        v.path().join("vault.yml"),
        format!(
            "schedule:\n  - cron: \"0 3 * * 0\"\n    command: {onebrain_str}\n    \
             args:\n      - search\n      - reindex\n"
        ),
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("<string>search</string>"))
        .stdout(predicate::str::contains("<string>reindex</string>"))
        .stdout(predicate::str::contains("<string>--vault</string>"));
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires user input"));
}

/// #116 bug 2 fix (this release): a command-mode entry registered under the
/// OLD basename-only label scheme leaves a stale plist on disk once the
/// entry starts carrying an args/cron discriminator. Registering again must
/// remove the stale legacy-labeled plist file so the user doesn't end up
/// with both the old and new jobs loaded.
///
/// Unix-only: relies on `/bin/echo` existing and writes to a tempdir HOME.
// macOS-only since Task 6: registers/renders launchd artifacts, which
// Linux now refuses (#313) and Windows renders differently.
#[cfg(target_os = "macos")]
#[test]
fn stale_legacy_plist_removed_on_reregister() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("vault.yml"),
        "schedule:\n  - cron: \"0 3 * * 0\"\n    command: /bin/echo\n    args:\n      - hello\n",
    )
    .unwrap();
    let home = tempdir().unwrap();
    let agents_dir = home.path().join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    // Simulate a pre-#116 registration: a plist at the OLD basename-only
    // label (no discriminator) — this is what `onebrain register-schedule`
    // would have written before the args discriminator existed.
    let legacy_plist = agents_dir.join("com.onebrain.echo.plist");
    std::fs::write(&legacy_plist, "<!-- stale pre-#116 plist -->").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed stale legacy plist"));

    assert!(
        !legacy_plist.exists(),
        "stale legacy plist must be removed on re-register"
    );
    // The new discriminator-bearing plist must exist alongside the cleanup.
    let new_plist = agents_dir.join("com.onebrain.echo-hello.plist");
    assert!(
        new_plist.exists(),
        "expected new plist at {}",
        new_plist.display()
    );
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains("shell-special"));
}

/// `--refresh` prints a notice line before writing plists.
// macOS-only since Task 6: asserts launchd plist text; other platforms
// preview their own artifact (or an honest note) via render_preview [M5].
#[cfg(target_os = "macos")]
#[test]
fn refresh_emits_notice_line() {
    // Use onebrain.yml (canonical v3.1+ format).
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n",
    )
    .unwrap();
    write_skill(v.path(), "daily", "name: daily\nschedulable: true");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--refresh", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("--refresh: re-emitting"));
}

/// `--status` reports `[once]` tag for one-shot entries.
#[test]
fn status_shows_once_tag_for_at_entries() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - at: \"2026-05-13 14:30\"\n    skill: /daily\n",
    )
    .unwrap();
    write_skill(v.path(), "daily", "name: daily\nschedulable: true");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--status"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("[once]"));
}

/// `--status` shows skill args in parentheses when present.
#[test]
fn status_shows_skill_args_in_parens() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /distill\n    args:\n      topic: this-week\n",
    )
    .unwrap();
    write_skill(v.path(), "distill", "name: distill\nschedulable: true");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--status"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        // Args formatted as "(topic=this-week)"
        .stdout(predicate::str::contains("topic=this-week"));
}

/// `--status` shows `cmd:` label for command-mode entries (no args).
///
/// Unix-only: relies on `/bin/echo` existing.
#[cfg(unix)]
#[test]
fn status_shows_cmd_label_for_command_mode_entry() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 3 * * 0\"\n    command: /bin/echo\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--status"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("cmd: /bin/echo"));
}

/// `--status` shows `cmd:` with args for command-mode entries.
///
/// Unix-only: relies on `/bin/echo` existing.
#[cfg(unix)]
#[test]
fn status_shows_cmd_with_args() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 3 * * 0\"\n    command: /bin/echo\n    args:\n      - hello\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--status"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("cmd: /bin/echo hello"));
}

/// `--resume` when no paused marker exists prints "not paused".
#[test]
fn resume_when_not_paused_prints_not_paused() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n",
    )
    .unwrap();
    // No marker file created — skill is not paused.
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--resume", "/daily"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("is not paused"));
}

/// Empty schedule in onebrain.yml → prints a notice, exits 0.
#[test]
fn empty_schedule_block_exits_cleanly() {
    let v = tempdir().unwrap();
    std::fs::write(v.path().join("onebrain.yml"), "# no schedule key\n").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("Nothing to register"));
}

/// Invalid cron expression produces a friendly error message.
#[test]
fn invalid_cron_produces_error() {
    let v = tempdir().unwrap();
    write_skill(v.path(), "daily", "name: daily\nschedulable: true");
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * *\"\n    skill: /daily\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid cron").or(predicate::str::contains("cron")));
}

/// Skill with schedulable_with_args: true but missing required arg is rejected.
#[test]
fn schedulable_with_args_missing_required_arg_fails() {
    let v = tempdir().unwrap();
    let skill_dir = v.path().join(".claude/plugins/onebrain/skills/distill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nschedulable_with_args: true\nrequired_args:\n  - topic\n---\n",
    )
    .unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /distill\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires args").or(predicate::str::contains("topic")));
}

/// Skill with no schedulable key in frontmatter is rejected.
#[test]
fn skill_no_schedulable_key_fails() {
    let v = tempdir().unwrap();
    let skill_dir = v.path().join(".claude/plugins/onebrain/skills/nodecl");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "---\nname: nodecl\n---\n").unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /nodecl\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not declare schedulable"));
}

/// skill-mode args with shell-special chars are rejected at register time.
#[test]
fn skill_mode_args_with_shell_special_rejected() {
    let v = tempdir().unwrap();
    write_skill(v.path(), "distill", "name: distill\nschedulable: true");
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /distill\n    args:\n      topic: \"$(evil)\"\n",
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains("shell-special"));
}

/// `--test` with a skill that isn't in the schedule fails with a helpful error.
#[test]
fn test_run_missing_skill_fails_with_message() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n",
    )
    .unwrap();
    write_skill(v.path(), "daily", "name: daily\nschedulable: true");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--test", "/nonexistent"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no `schedule:` entry matching skill",
        ));
}

/// `--remove` when no plists exist exits cleanly (nothing to delete).
#[cfg(unix)]
#[test]
fn remove_when_no_plists_exits_cleanly() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n",
    )
    .unwrap();
    write_skill(v.path(), "daily", "name: daily\nschedulable: true");
    // No plist was ever written → remove should be a no-op exit 0.
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--remove"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success();
}

/// Actual registration (no --dry-run) writes plist to LaunchAgents.
///
/// Unix-only: LaunchAgents is macOS/Linux HOME convention.
// macOS-only since Task 6: registers/renders launchd artifacts, which
// Linux now refuses (#313) and Windows renders differently.
#[cfg(target_os = "macos")]
#[test]
fn registration_writes_plist_file() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n",
    )
    .unwrap();
    write_skill(v.path(), "daily", "name: daily\nschedulable: true");
    let home = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{2713} Wrote"))
        .stdout(predicate::str::contains(
            "Registered and activated 1 schedule entries",
        ));

    let plist = home
        .path()
        .join("Library/LaunchAgents/com.onebrain.daily.plist");
    assert!(plist.exists(), "plist must exist at {}", plist.display());
}

/// `--status` reports the artifact's real state (#312): absent → ✗, and —
/// because this harness disables activation — present-but-not-loaded → ⚠.
/// The old test asserted ✓ here, which was precisely the lie #312 fixes:
/// a file on disk is not a scheduled job.
// Unix-only: macOS and Linux share the artifact-on-disk model this asserts;
// Windows persists no file, so ⚠ can never arise there (decision 4).
#[cfg(unix)]
#[test]
fn status_marks_installed_and_uninstalled() {
    let v = tempdir().unwrap();
    std::fs::write(
        v.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n",
    )
    .unwrap();
    write_skill(v.path(), "daily", "name: daily\nschedulable: true");
    let home = tempdir().unwrap();

    // Before registration → uninstalled (✗)
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--status"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{2717}"));

    // Register it
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success();

    // After registration, with activation disabled by the harness: artifact
    // on disk but the OS scheduler (launchd / systemd) not running it →
    // Inactive (⚠). Asserting ✓ here would re-assert the pre-#312
    // file-existence lie on either platform.
    let expected = "\u{26a0}";
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--status"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains(expected));
}

// ── #116 bug 1: cron step/list/range end-to-end ────────────────────────────

/// A `*/N` step cron expression is accepted end-to-end and emits an
/// array-form `StartCalendarInterval` in the dry-run plist output.
// macOS-only since Task 6: asserts launchd plist text; other platforms
// preview their own artifact (or an honest note) via render_preview [M5].
#[cfg(target_os = "macos")]
#[test]
fn step_cron_dry_run_emits_array_form_calendar_interval() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 */6 * * *\"\n    skill: /daily\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("<key>StartCalendarInterval</key>"))
        .stdout(predicate::str::contains("<array>"))
        .stdout(predicate::str::contains("<integer>0</integer>"))
        .stdout(predicate::str::contains("<integer>6</integer>"))
        .stdout(predicate::str::contains("<integer>12</integer>"))
        .stdout(predicate::str::contains("<integer>18</integer>"));
}

/// A list (`1,3,5`) cron expression is accepted end-to-end.
// macOS-only since Task 6: asserts launchd plist text; other platforms
// preview their own artifact (or an honest note) via render_preview [M5].
#[cfg(target_os = "macos")]
#[test]
fn list_cron_dry_run_succeeds() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * 1,3,5\"\n    skill: /daily\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("<array>"));
}

/// A range (`1-5`) cron expression is accepted end-to-end.
// macOS-only since Task 6: asserts launchd plist text; other platforms
// preview their own artifact (or an honest note) via render_preview [M5].
#[cfg(target_os = "macos")]
#[test]
fn range_cron_dry_run_succeeds() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * 1-5\"\n    skill: /daily\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("<array>"));
}

/// A single-value cron expression (the common case) still emits the plain
/// single-`<dict>` calendar interval form, not an array — byte-parity
/// regression guard at the CLI-integration layer.
// macOS-only since Task 6: asserts launchd plist text; other platforms
// preview their own artifact (or an honest note) via render_preview [M5].
#[cfg(target_os = "macos")]
#[test]
fn plain_cron_dry_run_still_emits_single_dict_form() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "<key>StartCalendarInterval</key>\n    <dict>",
        ))
        .stdout(predicate::str::contains("<array>").count(1)); // only ProgramArguments' array
}

/// A pathologically-expanding cron (two simultaneously multi-valued fields)
/// is rejected with a clear error rather than silently emitting a huge
/// plist.
#[test]
fn pathological_cron_expansion_rejected() {
    let v = write_skill_vault("schedule:\n  - cron: \"*/1 */1 * * *\"\n    skill: /daily\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", v.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains("exceeding the cap"));
}

/// Linux (#314): `register` writes both systemd user units. Drives the full
/// resolve→collision→install path through the systemd arm on the one runner
/// that measures (MJ-9, option 1); the activation legs (`daemon-reload`,
/// `enable`, `restart`) are covered by the VM verify, since the harness
/// disables activation.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn linux_register_writes_systemd_user_units() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n");
    let home = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("systemd user timers"));
    let unit_dir = home.path().join(".config/systemd/user");
    assert!(unit_dir.join("onebrain-daily.timer").exists());
    assert!(unit_dir.join("onebrain-daily.service").exists());
    assert!(
        !home.path().join("Library").exists(),
        "must not create a macOS-only ~/Library on Linux"
    );
}

/// The #330 lesson pinned end-to-end: systemd refuses a relative ExecStart
/// binary (measured — that exact case went red on main), so the unit
/// actually written for a bare command name must carry the absolute path
/// `resolve_command_binary` found at register time.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn linux_written_service_unit_has_an_absolute_execstart() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 3 * * 0\"\n    command: sh\n");
    let home = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success();
    // Find the written unit rather than hard-coding the label: command-mode
    // labels carry a discriminator (args/cron/at), so the exact filename is
    // `command_discriminator`'s business, not this test's.
    let unit_dir = home.path().join(".config/systemd/user");
    let service = std::fs::read_dir(&unit_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "service"))
        .expect("register must write a .service unit");
    let text = std::fs::read_to_string(&service).unwrap();
    let exec = text
        .lines()
        .find(|l| l.starts_with("ExecStart="))
        .expect("service must have ExecStart");
    assert!(
        exec.starts_with("ExecStart=/"),
        "systemd rejects relative binaries: {exec}"
    );
}

/// Linux `--dry-run` prints the real artifacts — both units, labelled per
/// file — and writes nothing [M5].
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn linux_dry_run_prints_both_units() {
    let v = write_skill_vault("schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n");
    let home = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args(["register-schedule", "--dry-run"])
        .current_dir(v.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("# onebrain-daily.service"))
        .stdout(predicate::str::contains("# onebrain-daily.timer"))
        .stdout(predicate::str::contains("OnCalendar=*-*-* 09:00:00"));
    assert!(
        !home.path().join(".config/systemd/user").exists(),
        "a dry run must write nothing"
    );
}
