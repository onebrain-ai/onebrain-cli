//! Windows-only fire-proof: a task registered through the real backend
//! actually RUNS, and a one-shot actually deletes itself.
//!
//! This is the regression guard for the failure class that motivated the
//! entire v3.4.20 release: an artifact that installs cleanly and never fires.
//! Snapshot tests cannot catch "well-formed and inert" — only a real
//! scheduler observing a real fire can.
//!
//! `#[ignore]` because it sleeps for minutes; the `windows-schedule-fires`
//! CI job runs it with `--ignored`. Activation is deliberately NOT disabled
//! here — a hosted runner is throwaway, the one place real registration is
//! safe (the same reasoning as the schema workflow's launchd job).
//!
//! NOTE: this is a separate crate from `onebrain-core`, so
//! `scheduler::test_support` (`#![cfg(test)]` at module level) is not
//! reachable — fixtures are built locally, per the plan's [M6].
#![cfg(windows)]

use onebrain_core::scheduler::{backend, InstallState, ScheduleEntry, SchedulerContext};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn ctx(home: &std::path::Path) -> SchedulerContext {
    SchedulerContext {
        vault_path: home.join("vault"),
        skill_cli_path: "C:\\Windows\\System32\\cmd.exe".to_string(),
        log_base_path: home.join("logs"),
        homedir: home.to_path_buf(),
        uid: 501, // meaningless on Windows; the field is deliberately ungated (B1)
    }
}

#[test]
#[ignore = "slow: waits for Task Scheduler to fire; run via the windows-schedule-fires CI job"]
fn a_registered_one_shot_actually_runs_and_then_deletes_itself() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("fired.txt");
    // Uniqueness comes from the tempdir path inside the args discriminator:
    // the backend derives the label from command basename + args, and the
    // tempdir name is random per run — two parallel jobs cannot collide.

    // `at:` is minute-granular (`YYYY-MM-DD HH:MM`, literal space — MJ-5), so
    // "+60 s" is not expressible: round up to the minute, then add one more.
    // The real delay lands anywhere in 60–120 s [BL-5]; every ceiling below
    // budgets for the far edge.
    let now = chrono::Local::now();
    let fire_at = (now + chrono::Duration::seconds(119))
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    use chrono::Timelike;

    // Command-mode entry whose action writes the sentinel. cmd.exe handles
    // the redirect itself — no shell wrapper is emitted by the renderer, the
    // COMMAND simply is a shell, which is the test's business, not the
    // scheduler's.
    let entry = ScheduleEntry {
        at: Some(fire_at.format("%Y-%m-%d %H:%M").to_string()),
        command: Some("C:\\Windows\\System32\\cmd.exe".to_string()),
        args: Some(onebrain_core::scheduler::Args::List(vec![
            "/c".to_string(),
            format!("echo fired > \"{}\"", sentinel.display()),
        ])),
        ..Default::default()
    };
    let ctx = ctx(dir.path());

    // Best-effort teardown ALWAYS runs at the end — but never before the
    // self-delete assertion, which it would make vacuous [M1].
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let artifact = backend::install(&entry, &ctx).expect("install must register the task");
        assert_eq!(
            artifact,
            PathBuf::from(format!("\\OneBrain\\{}", backend_label(&entry))),
            "windows install reports the task name as its artifact (decision 4)"
        );
        assert_ne!(
            backend::is_installed(&backend_label(&entry), &ctx).unwrap(),
            InstallState::Absent,
            "registered task must be queryable"
        );

        // ── assert 1: it FIRES ──────────────────────────────────────────
        // Fire lands 60–120 s out; poll to fire+180 s for scheduler start
        // latency on a loaded runner (a tight bound turns a pass into a
        // flake).
        let fire_deadline = Instant::now() + Duration::from_secs(120 + 180);
        let mut fired = false;
        while Instant::now() < fire_deadline {
            if sentinel.exists() {
                fired = true;
                break;
            }
            std::thread::sleep(Duration::from_secs(5));
        }
        assert!(fired, "task registered cleanly but never fired — the exact bug class this release exists to catch");
        println!("fired ✓");

        // ── assert 2: it DELETES ITSELF ─────────────────────────────────
        // Measured chain (spike E): EndBoundary = start+60 s, delete clock
        // runs from the EndBoundary, observed disappearance ~180 s from
        // registration. From the fire, budget expiry(60) + delete(60) +
        // generous slack(180) — and do NOT delete it ourselves first [M1].
        let gone_deadline = Instant::now() + Duration::from_secs(300);
        let mut gone = false;
        while Instant::now() < gone_deadline {
            if backend::is_installed(&backend_label(&entry), &ctx).unwrap() == InstallState::Absent
            {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_secs(10));
        }
        assert!(
            gone,
            "one-shot fired but never deleted itself — EndBoundary/DeleteExpiredTaskAfter regression [M1]"
        );
        println!("self-deleted ✓");
    }));

    // Teardown: harmless if the task already self-deleted.
    let _ = backend::remove(&backend_label(&entry), &ctx);
    result.unwrap();
}

/// The label the backend derives for this entry (command basename + args
/// discriminator) — computed the same way install() computes it, so the
/// query targets what was actually registered.
fn backend_label(entry: &ScheduleEntry) -> String {
    onebrain_core::scheduler::label_for_entry(entry)
}
