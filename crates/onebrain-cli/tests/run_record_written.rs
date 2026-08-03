//! Every skill-mode scheduled run leaves a record in the vault — including
//! when it FAILS, and without depending on the skill choosing to write one.
//! Before v3.4.23 only 6 of 13 schedulable skills wrote anything at all.
//!
//! Drives the real binary against a mock harness, following the pattern in
//! `run_skill.rs` — `onebrain-cli` is binary-only (no lib target), so tests
//! cannot call `run()` directly.

mod support;

use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn write_minimal_vault(dir: &Path) {
    fs::write(dir.join("onebrain.yml"), "folders:\n  logs: 07-logs\n").unwrap();
}

/// Mock harness printing `msg` to stdout and exiting `code`.
fn write_mock(dir: &Path, msg: &str, code: i32) -> PathBuf {
    let path = dir.join("claude-mock.sh");
    fs::write(&path, format!("#!/bin/bash\necho '{msg}'\nexit {code}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(&path, p).unwrap();
    }
    path
}

/// First record file under `07-logs/log/**` whose name ends `-<entry>.md`.
fn find_record(vault: &Path, entry: &str) -> Option<String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(&vault.join("07-logs").join("log"), &mut files);
    files
        .into_iter()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&format!("-{entry}.md")))
        })
        .and_then(|p| fs::read_to_string(p).ok())
}

fn run_skill(vault: &Path, harness: &Path, scheduled: bool) -> std::process::Output {
    let mut cmd = Command::cargo_bin("onebrain").unwrap();
    cmd.env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CLAUDE_BIN", harness)
        .args([
            "skill",
            "run",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "daily",
        ]);
    if scheduled {
        cmd.env("ONEBRAIN_SCHEDULED", "1");
    } else {
        cmd.env_remove("ONEBRAIN_SCHEDULED");
    }
    cmd.output().unwrap()
}

#[test]
fn a_failing_run_still_leaves_a_record() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let mock = write_mock(d.path(), "boom", 3);

    let out = run_skill(d.path(), &mock, true);
    assert_eq!(out.status.code(), Some(3), "exit code propagates");

    let body = find_record(d.path(), "daily")
        .expect("a FAILING run must still leave a record — that is the point");
    assert!(body.contains("❌"), "failure is legible: {body}");
    assert!(body.contains("exit 3"), "exit code named: {body}");
}

#[test]
fn a_scheduled_run_is_tagged_scheduled_and_a_manual_one_is_not() {
    // doctor's staleness check counts ONLY scheduled runs. Without the marker
    // one manual run would make a week-dead cron job look alive.
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let mock = write_mock(d.path(), "ok", 0);

    run_skill(d.path(), &mock, true);
    let sched = find_record(d.path(), "daily").expect("record written");
    assert!(sched.contains("scheduled"), "tagged scheduled: {sched}");

    let d2 = tempdir().unwrap();
    write_minimal_vault(d2.path());
    let mock2 = write_mock(d2.path(), "ok", 0);
    run_skill(d2.path(), &mock2, false);
    let manual = find_record(d2.path(), "daily").expect("record written");
    assert!(manual.contains("manual"), "tagged manual: {manual}");
}

#[test]
fn a_record_failure_does_not_fail_the_run() {
    // Block the logs folder with a FILE so the record cannot be written.
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    fs::write(d.path().join("07-logs"), "not a directory").unwrap();
    let mock = write_mock(d.path(), "fine", 0);

    let out = run_skill(d.path(), &mock, true);

    assert_eq!(
        out.status.code(),
        Some(0),
        "logging must never kill the job it logs (#372's lesson); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_child_output_still_reaches_our_stdout() {
    // The write-through. v3.4.23 changed the non-interactive branch from
    // inherited stdio to piped-and-captured; without an explicit pass-through
    // the user would silently stop seeing output.
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let mock = write_mock(d.path(), "HELLO_FROM_CHILD", 0);

    let out = run_skill(d.path(), &mock, true);

    assert!(
        String::from_utf8_lossy(&out.stdout).contains("HELLO_FROM_CHILD"),
        "child output passed through: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_child_flooding_stderr_does_not_deadlock() {
    // The rev-1 plan drained one pipe to EOF before wait(), which deadlocks the
    // moment the child fills the OTHER pipe's ~64 KB buffer. This test fails by
    // HANGING rather than by asserting, which is exactly why it exists: #361 in
    // this repo is an unexplained pipe hang and we must not ship a second one.
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let path = d.path().join("flood.sh");
    fs::write(
        &path,
        "#!/bin/bash\n\
         for i in $(seq 1 4000); do\n\
         \x20 echo \"stderr line $i padding padding padding padding padding\" >&2\n\
         done\n\
         echo done-stdout\n\
         exit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(&path, p).unwrap();
    }

    let out = run_skill(d.path(), &path, true);

    assert_eq!(out.status.code(), Some(0), "completed without deadlocking");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("stderr line 4000"),
        "all stderr drained, not truncated at the pipe buffer"
    );
}
