//! The one place that talks to an OS scheduler.
//!
//! Rendering lives in the per-OS modules and is pure, so its snapshot tests run
//! on any host. Only the functions here perform installation, removal, or
//! queries — and only through the platform's own mechanism.
//!
//! **Every platform has an arm from day one.** Leaving a platform without one
//! makes `cargo test --workspace` fail there for every commit until its real
//! backend lands — the round-4 BL-A failure, which is the same class as B1
//! (gating `uid`), reintroduced one task after the plan congratulated itself
//! for avoiding it. All three arms are real backends now: launchd (macOS),
//! Task Scheduler (Windows), systemd user timers (Linux).

use crate::scheduler::context::SchedulerContext;
use crate::scheduler::error::SchedulerError;
use crate::scheduler::launchd::plist_path;
use crate::scheduler::types::ScheduleEntry;
use std::path::PathBuf;

/// What `is_installed` can report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    /// The OS scheduler will run this.
    Active,
    /// An artifact exists but the OS is not running it.
    Inactive,
    /// Nothing installed.
    Absent,
}

/// Install (or refresh) the artifact for `entry`. Returns the artifact path.
pub fn install(entry: &ScheduleEntry, ctx: &SchedulerContext) -> Result<PathBuf, SchedulerError> {
    imp::install(entry, ctx)
}

/// Remove the artifact for `label_safe`. Returns whether anything was removed.
pub fn remove(label_safe: &str, ctx: &SchedulerContext) -> Result<bool, SchedulerError> {
    imp::remove(label_safe, ctx)
}

/// Report the artifact's real state.
pub fn is_installed(
    label_safe: &str,
    ctx: &SchedulerContext,
) -> Result<InstallState, SchedulerError> {
    imp::is_installed(label_safe, ctx)
}

/// Human-readable name of the active backend, for help text and diagnostics.
pub fn describe() -> &'static str {
    imp::describe()
}

/// What `--dry-run` prints for this entry on this platform, or `None` when
/// the platform has no backend (a dry run that errors teaches nothing, and
/// printing another OS's format teaches the wrong thing) [M5].
pub fn render_preview(
    entry: &ScheduleEntry,
    ctx: &SchedulerContext,
) -> Result<Option<String>, SchedulerError> {
    #[cfg(target_os = "macos")]
    {
        Ok(Some(crate::scheduler::launchd::generate_plist(entry, ctx)?))
    }
    #[cfg(windows)]
    {
        Ok(Some(crate::scheduler::schtasks::generate_task_xml(
            entry, ctx,
        )?))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Both units, labelled: a dry run must show everything install would
        // write, and which file each block lands in.
        let label = crate::scheduler::launchd::label_for_entry(entry);
        let base = crate::scheduler::systemd::unit_base_name(&label);
        Ok(Some(format!(
            "# {base}.service\n{}\n# {base}.timer\n{}",
            crate::scheduler::systemd::generate_service_unit(entry, ctx)?,
            crate::scheduler::systemd::generate_timer_unit(entry, ctx)
        )))
    }
}

/// The identity `detect_collisions` keys uniqueness on. Two entries whose
/// artifact keys are equal would overwrite each other at install time.
///
/// On every current backend this is the artifact path launchd would use — a
/// stable, platform-independent function of the label — which keeps today's
/// collision semantics exactly. The Windows backend (Task 7c) maps the same
/// label space onto task names, so equal keys still mean colliding tasks.
pub fn artifact_key(entry: &ScheduleEntry, ctx: &SchedulerContext) -> PathBuf {
    let label = crate::scheduler::launchd::label_for_entry(entry);
    plist_path(&label, &ctx.homedir)
}

/// Test-isolation kill-switch shared by every backend arm AND by the CLI's
/// legacy-plist cleanup path — NOT a user feature.
///
/// Labels/task names/unit names are process- or user-global namespaces: an
/// integration test running the real binary with `HOME=tempdir` still
/// targets the REAL scheduler entries on activate/deactivate. The first
/// suite run after macOS activation landed hijacked the operator's real
/// `/daily` (bootstrapped it from a since-deleted tempdir) and deleted its
/// plist. The integration harness sets this for every spawned binary;
/// nothing else should. The 37th call site that spawned a scheduler command
/// WITHOUT checking this (the legacy-plist bootout) is why it is `pub` and
/// shared rather than copied per arm.
pub fn activation_disabled() -> bool {
    std::env::var_os("ONEBRAIN_SCHEDULER_NO_ACTIVATE").is_some()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::process::Command;

    fn service_target(label_safe: &str, ctx: &SchedulerContext) -> String {
        format!("gui/{}/com.onebrain.{label_safe}", ctx.uid)
    }

    /// Write the plist, then make launchd actually run it (#312).
    ///
    /// `bootout` first (failure ignored — the job may simply not be loaded),
    /// then `bootstrap` (failure is a real error). The pair makes
    /// re-registration idempotent and picks up a regenerated plist
    /// immediately — the file-only model only converged at next login, which
    /// is how a regenerated plist sat unloaded for months.
    pub fn install(
        entry: &ScheduleEntry,
        ctx: &SchedulerContext,
    ) -> Result<PathBuf, SchedulerError> {
        let target = write_plist(entry, ctx)?;
        if activation_disabled() {
            return Ok(target);
        }
        let label = crate::scheduler::launchd::label_for_entry(entry);

        let _ = Command::new("launchctl")
            .args(["bootout", &service_target(&label, ctx)])
            .output();

        let bootstrap = Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{}", ctx.uid)])
            .arg(&target)
            .output();
        match bootstrap {
            Ok(out) if out.status.success() => Ok(target),
            Ok(out) => Err(SchedulerError::BackendCommand {
                command: format!("launchctl bootstrap gui/{} {}", ctx.uid, target.display()),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
            // launchctl itself unavailable (sandboxed CI, containers): the
            // artifact is written and launchd will pick it up at login — the
            // pre-#312 behaviour. Degraded but not silent: `is_installed`
            // will report Inactive, not Active.
            Err(_) => Ok(target),
        }
    }

    /// `bootout` BEFORE deleting — a loaded job keeps firing after its plist
    /// is gone, which is how `--remove` used to lie.
    pub fn remove(label_safe: &str, ctx: &SchedulerContext) -> Result<bool, SchedulerError> {
        if !activation_disabled() {
            let _ = Command::new("launchctl")
                .args(["bootout", &service_target(label_safe, ctx)])
                .output();
        }
        let target = plist_path(label_safe, &ctx.homedir);
        if target.exists() {
            std::fs::remove_file(&target)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Ask launchd, not the filesystem (#312). Exit 0 from `launchctl print`
    /// → Active. Otherwise fall back to file existence: present → Inactive
    /// (artifact on disk, OS not running it — the state the old `list`
    /// could not see), absent → Absent. A missing `launchctl` binary lands
    /// in the same fallback rather than erroring.
    pub fn is_installed(
        label_safe: &str,
        ctx: &SchedulerContext,
    ) -> Result<InstallState, SchedulerError> {
        let loaded = !activation_disabled()
            && Command::new("launchctl")
                .args(["print", &service_target(label_safe, ctx)])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        if loaded {
            return Ok(InstallState::Active);
        }
        let target = plist_path(label_safe, &ctx.homedir);
        Ok(if target.exists() {
            InstallState::Inactive
        } else {
            InstallState::Absent
        })
    }

    pub fn describe() -> &'static str {
        "launchd"
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;
    use crate::scheduler::systemd::{
        generate_service_unit, generate_timer_unit, unit_base_name, unit_dir,
    };
    use std::process::Command;

    fn systemctl(args: &[&str]) -> Result<std::process::Output, SchedulerError> {
        Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .map_err(|e| SchedulerError::BackendCommand {
                command: format!("systemctl --user {}", args.join(" ")),
                status: "spawn failed".to_string(),
                stderr: e.to_string(),
            })
    }

    fn run_systemctl(args: &[&str]) -> Result<(), SchedulerError> {
        let out = systemctl(args)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(SchedulerError::BackendCommand {
                command: format!("systemctl --user {}", args.join(" ")),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }

    /// Write both units, then make systemd actually run the timer:
    /// `daemon-reload` → `enable` (the timers.target symlink, so it starts
    /// at login) → `restart` (starts it NOW and — unlike `enable --now`,
    /// which is a no-op on an already-running timer — picks up a regenerated
    /// OnCalendar immediately, the same re-registration semantics as the
    /// macOS arm's bootout→bootstrap pair).
    ///
    /// Failures surface verbatim. A Linux box where `systemctl --user` does
    /// not work (no systemd, no user session) gets a loud error, not
    /// write-and-report-success — failing loudly beats succeeding falsely,
    /// the release's whole thesis (#313).
    ///
    /// No `ensure_log_dir` here: output capture is the journal's job by
    /// design (BL-1) — no unit line references a log path, so there is no
    /// directory whose absence could re-create the exit-78 class of bug.
    pub fn install(
        entry: &ScheduleEntry,
        ctx: &SchedulerContext,
    ) -> Result<PathBuf, SchedulerError> {
        let label = crate::scheduler::launchd::label_for_entry(entry);
        let base = unit_base_name(&label);
        let dir = unit_dir(&ctx.homedir);
        std::fs::create_dir_all(&dir)?;
        let timer_path = dir.join(format!("{base}.timer"));
        std::fs::write(
            dir.join(format!("{base}.service")),
            generate_service_unit(entry, ctx)?,
        )?;
        std::fs::write(&timer_path, generate_timer_unit(entry, ctx))?;
        if activation_disabled() {
            return Ok(timer_path);
        }
        run_systemctl(&["daemon-reload"])?;
        let timer_unit = format!("{base}.timer");
        run_systemctl(&["enable", &timer_unit])?;
        run_systemctl(&["restart", &timer_unit])?;
        Ok(timer_path)
    }

    /// `disable --now` BEFORE deleting (a live timer with its units gone
    /// keeps firing until reload — the Linux spelling of the `--remove` lie),
    /// then remove both units, then `daemon-reload` so the removal is
    /// observed. Missing units → Ok(false).
    pub fn remove(label_safe: &str, ctx: &SchedulerContext) -> Result<bool, SchedulerError> {
        let base = unit_base_name(label_safe);
        if !activation_disabled() {
            // Failure ignored — the timer may simply not be loaded.
            let _ = systemctl(&["disable", "--now", &format!("{base}.timer")]);
        }
        let dir = unit_dir(&ctx.homedir);
        let mut removed = false;
        for name in [format!("{base}.timer"), format!("{base}.service")] {
            let path = dir.join(name);
            if path.exists() {
                std::fs::remove_file(&path)?;
                removed = true;
            }
        }
        if removed && !activation_disabled() {
            let _ = systemctl(&["daemon-reload"]);
        }
        Ok(removed)
    }

    /// Ask systemd, not the filesystem (#312): `is-active <base>.timer` —
    /// NOT `is-enabled`, which reports yes for a stopped-but-symlinked timer
    /// (MJ-7's Linux sibling). Fall back to file existence: present →
    /// Inactive, absent → Absent. A missing `systemctl` binary lands in the
    /// same fallback rather than erroring.
    pub fn is_installed(
        label_safe: &str,
        ctx: &SchedulerContext,
    ) -> Result<InstallState, SchedulerError> {
        let base = unit_base_name(label_safe);
        let active = !activation_disabled()
            && systemctl(&["is-active", &format!("{base}.timer")])
                .map(|o| o.status.success())
                .unwrap_or(false);
        if active {
            return Ok(InstallState::Active);
        }
        Ok(
            if unit_dir(&ctx.homedir)
                .join(format!("{base}.timer"))
                .exists()
            {
                InstallState::Inactive
            } else {
                InstallState::Absent
            },
        )
    }

    pub fn describe() -> &'static str {
        "systemd user timers"
    }
}

/// Create the scheduler log directory before any artifact references it.
///
/// launchd opens `StandardOutPath`/`StandardErrorPath` **before exec** and does
/// not create parent directories, so a missing directory is fatal for any entry
/// that still carries a redirect.
///
/// **v3.4.23 narrowed which entries those are.** Skill-mode plists no longer
/// emit a redirect at all — the CLI opens its own log after exec and recreates
/// the directory itself (`scheduler::run_log`). **Command-mode entries still
/// depend on this**: launchd execs their binary directly, so no OneBrain
/// process exists to own a log, and for them skipping this still ships
/// EX_CONFIG-78-with-no-output — the #315 headline bug.
pub fn ensure_log_dir(ctx: &SchedulerContext) -> std::io::Result<()> {
    std::fs::create_dir_all(&ctx.log_base_path)
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::scheduler::schtasks::{generate_task_xml, task_name};
    use std::process::Command;

    /// Render → temp file → `schtasks /Create /XML <tmp> /TN <name> /F` →
    /// delete the temp in every path.
    ///
    /// `/Create /XML` takes a path, not stdin, so a temp file is unavoidable;
    /// it is removed even on failure because a leftover copy is exactly the
    /// artifact-vs-reality drift this design rejects (decision 4: Task
    /// Scheduler owns the definition, `/Query /XML` returns it on demand).
    /// `/F` makes re-registration idempotent with no pre-check.
    ///
    /// `__USER__` in the rendered document (see `generate_task_xml`) is
    /// substituted with the real account here — the renderer stays pure.
    pub fn install(
        entry: &ScheduleEntry,
        ctx: &SchedulerContext,
    ) -> Result<PathBuf, SchedulerError> {
        ensure_log_dir(ctx)?;
        let label = crate::scheduler::launchd::label_for_entry(entry);
        let name = task_name(&label);
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "SYSTEM".into());
        let xml = generate_task_xml(entry, ctx)?.replace("__USER__", &user);

        // Report the task name as the "artifact": Windows persists nothing on
        // disk by design, and the CLI's "Wrote {}" line becomes the task name.
        let artifact = PathBuf::from(&name);
        if activation_disabled() {
            return Ok(artifact);
        }

        let tmp = std::env::temp_dir().join(format!(
            "onebrain-task-{}-{}.xml",
            std::process::id(),
            label
        ));
        // Task Scheduler expects UTF-16 for /XML documents.
        let utf16: Vec<u8> = std::iter::once(0xFEFFu16)
            .chain(xml.encode_utf16())
            .flat_map(|u| u.to_le_bytes())
            .collect();
        std::fs::write(&tmp, utf16)?;

        let out = Command::new("schtasks")
            .args(["/Create", "/XML"])
            .arg(&tmp)
            .args(["/TN", &name, "/F"])
            .output();
        let _ = std::fs::remove_file(&tmp);

        match out {
            Ok(o) if o.status.success() => Ok(artifact),
            Ok(o) => Err(SchedulerError::BackendCommand {
                command: format!("schtasks /Create /XML <tmp> /TN {name} /F"),
                status: o.status.to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            }),
            Err(e) => Err(SchedulerError::BackendCommand {
                command: "schtasks /Create".to_string(),
                status: "spawn failed".to_string(),
                stderr: e.to_string(),
            }),
        }
    }

    /// `/Delete /TN <name> /F`; a missing task is Ok(false), any other
    /// failure surfaces verbatim.
    pub fn remove(label_safe: &str, ctx: &SchedulerContext) -> Result<bool, SchedulerError> {
        let _ = ctx;
        if activation_disabled() {
            return Ok(false);
        }
        let name = task_name(label_safe);
        let out = Command::new("schtasks")
            .args(["/Delete", "/TN", &name, "/F"])
            .output();
        match out {
            Ok(o) if o.status.success() => Ok(true),
            // ERROR: The system cannot find the file specified — not a task.
            Ok(_) => Ok(false),
            Err(e) => Err(SchedulerError::BackendCommand {
                command: format!("schtasks /Delete /TN {name} /F"),
                status: "spawn failed".to_string(),
                stderr: e.to_string(),
            }),
        }
    }

    /// `/Query /FO LIST /V` and parse `Status:` — exit code alone reports 0
    /// for a DISABLED task, which is "present but inert", the exact state
    /// this release exists to stop mis-reporting (round 3, MJ-7).
    pub fn is_installed(
        label_safe: &str,
        ctx: &SchedulerContext,
    ) -> Result<InstallState, SchedulerError> {
        let _ = ctx;
        if activation_disabled() {
            return Ok(InstallState::Absent);
        }
        let name = task_name(label_safe);
        let out = Command::new("schtasks")
            .args(["/Query", "/TN", &name, "/FO", "LIST", "/V"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                let disabled = text
                    .lines()
                    .any(|l| l.trim_start().starts_with("Status:") && l.contains("Disabled"));
                Ok(if disabled {
                    InstallState::Inactive
                } else {
                    InstallState::Active
                })
            }
            Ok(_) => Ok(InstallState::Absent),
            Err(_) => Ok(InstallState::Absent),
        }
    }

    pub fn describe() -> &'static str {
        "Task Scheduler"
    }
}

/// The plist writer — used only by the macOS arm now: Windows renders
/// task XML and Linux refuses (#313). Gated so the Linux build, which is
/// the one `-D warnings` leg that measures, sees no dead code.
#[cfg(target_os = "macos")]
fn write_plist(entry: &ScheduleEntry, ctx: &SchedulerContext) -> Result<PathBuf, SchedulerError> {
    use crate::scheduler::launchd::generate_plist;
    ensure_log_dir(ctx)?;
    let label = crate::scheduler::launchd::label_for_entry(entry);
    let target = plist_path(&label, &ctx.homedir);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, generate_plist(entry, ctx)?)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::test_support;

    /// The whole lifecycle with activation disabled — filesystem-only, so it
    /// runs in the default suite without touching the user's real systemd
    /// manager. The systemctl legs are covered by the VM verify (#314) and
    /// the explicit `--ignored` round-trip above.
    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn linux_units_round_trip_on_disk_without_activation() {
        // No guard/lock needed: this is the only in-process test that
        // mutates this var on Linux, and it never unsets it — the kill-switch
        // is exactly what every other env-reading path here wants anyway.
        std::env::set_var("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1");
        let home = tempfile::tempdir().unwrap();
        let ctx = test_support::ctx_in(home.path());
        let entry = test_support::daily_entry();

        let artifact = install(&entry, &ctx).unwrap();
        let dir = crate::scheduler::systemd::unit_dir(&ctx.homedir);
        assert_eq!(artifact, dir.join("onebrain-daily.timer"));
        assert!(artifact.exists(), "timer unit must be written");
        assert!(
            dir.join("onebrain-daily.service").exists(),
            "service unit must be written"
        );
        assert!(
            !home.path().join("Library").exists(),
            "must not create a macOS-only ~/Library on Linux"
        );
        assert_eq!(
            is_installed("daily", &ctx).unwrap(),
            InstallState::Inactive,
            "on disk but not enabled — never \u{2713} from file existence alone"
        );

        let preview = render_preview(&entry, &ctx)
            .unwrap()
            .expect("linux has a backend, so a preview");
        assert!(preview.contains("[Timer]"), "{preview}");
        assert!(preview.contains("OnCalendar="), "{preview}");

        assert!(remove("daily", &ctx).unwrap());
        assert!(!artifact.exists() && !dir.join("onebrain-daily.service").exists());
        assert_eq!(is_installed("daily", &ctx).unwrap(), InstallState::Absent);
        assert!(!remove("daily", &ctx).unwrap(), "second remove is a no-op");
    }

    #[test]
    fn describe_names_the_active_backend() {
        assert!(!describe().is_empty());
        #[cfg(target_os = "macos")]
        assert_eq!(describe(), "launchd");
    }

    #[test]
    fn is_installed_reports_absent_for_an_unknown_label() {
        // READ-ONLY exemption from [B2]: on macOS this now performs a
        // `launchctl print` on a label that cannot exist, mutating nothing.
        // It stays in the default suite deliberately — it is also the proof
        // that a missing/failing launchctl degrades to the file fallback
        // instead of erroring (sandboxed CI).
        let home = tempfile::tempdir().unwrap();
        let ctx = test_support::ctx_in(home.path());
        assert_eq!(
            is_installed("onebrain-test-no-such-label", &ctx).unwrap(),
            InstallState::Absent
        );
    }

    /// Run-unique label per [B2]: `gui/<uid>/<label>` is a process-global OS
    /// namespace that `HOME=tempdir` does not sandbox. A fixed label here
    /// would boot out a developer's real job of the same name.
    fn unique_label(tag: &str) -> String {
        format!(
            "onebrain-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    #[ignore = "touches the real OS scheduler domain — run explicitly (Task 8's CI job and the 9b VM audit do), never in the default suite"]
    fn install_then_remove_round_trips_through_the_backend() {
        // macOS/Windows isolate under a tempdir home because their install
        // mechanisms take explicit paths/documents (`launchctl bootstrap
        // <plist>`, `schtasks /XML <file>`). Linux cannot be fooled that way:
        // `systemctl --user enable <name>` resolves against the REAL user
        // manager's search path, which never sees a tempdir $HOME — measured
        // on the 9b VM audit, the tempdir variant dies with "Unit not
        // found". So the systemd leg runs against the real unit directory,
        // leaning on the run-unique label + teardown that were already
        // mandatory here (B2).
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let home = tempfile::tempdir().unwrap();
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let ctx = test_support::ctx_in(home.path());
        #[cfg(all(unix, not(target_os = "macos")))]
        let real_home = std::env::var_os("HOME").expect("HOME must be set");
        #[cfg(all(unix, not(target_os = "macos")))]
        let ctx = test_support::ctx_in(std::path::Path::new(&real_home));

        let label = unique_label("roundtrip");
        let entry = test_support::entry_labelled(&label);

        // Teardown must run even if an assertion panics: a leaked bootstrapped
        // job pointing at a deleted tempdir outlives the test process.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let artifact = install(&entry, &ctx).unwrap();
            // Decision 4: Windows persists NO artifact on disk — install
            // reports the task NAME. Everywhere else the artifact is a file.
            #[cfg(windows)]
            assert!(
                artifact.to_string_lossy().starts_with("\\OneBrain\\"),
                "windows artifact is the task name: {artifact:?}"
            );
            #[cfg(not(windows))]
            assert!(artifact.exists(), "install must write the artifact");
            // Not on Linux: output capture is the journal's job (BL-1), so
            // the systemd arm deliberately creates no log directory.
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            assert!(
                ctx.log_base_path.is_dir(),
                "install must create the log dir — launchd opens the log paths before exec"
            );
            assert_ne!(
                is_installed(&label, &ctx).unwrap(),
                InstallState::Absent,
                "installed entry must not read as absent"
            );
        }));
        let removed = remove(&label, &ctx).unwrap();
        assert_eq!(is_installed(&label, &ctx).unwrap(), InstallState::Absent);
        result.unwrap();
        assert!(removed, "teardown should have found the artifact");
    }

    #[test]
    #[cfg_attr(
        any(target_os = "macos", windows),
        ignore = "queries the real OS scheduler domain — run explicitly, never in the default suite"
    )]
    fn an_artifact_the_os_does_not_know_about_reports_inactive() {
        // The regression guard for the actual #312 bug: a file on disk must
        // not read as scheduled.
        let home = tempfile::tempdir().unwrap();
        let ctx = test_support::ctx_in(home.path());
        let label = unique_label("inactive");
        let entry = test_support::entry_labelled(&label);

        // Write the artifact WITHOUT going through install() — no bootstrap.
        let target = plist_path(&label, &ctx.homedir);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(
            &target,
            crate::scheduler::launchd::generate_plist(&entry, &ctx).unwrap(),
        )
        .unwrap();

        #[cfg(target_os = "macos")]
        assert_eq!(is_installed(&label, &ctx).unwrap(), InstallState::Inactive);

        let _ = std::fs::remove_file(&target);
    }

    #[test]
    fn the_log_directory_is_created_before_the_artifact_is_written() {
        let home = tempfile::tempdir().unwrap();
        let ctx = test_support::ctx_in(home.path());
        assert!(!ctx.log_base_path.exists(), "fixture must start without it");

        ensure_log_dir(&ctx).unwrap();

        assert!(
            ctx.log_base_path.is_dir(),
            "launchd opens the log paths before exec — a missing dir is exit 78 with no output"
        );
    }

    // NOTE: the former `install_creates_the_log_directory_as_part_of_writing`
    // test was folded into the round-trip test above. Its "safe everywhere"
    // premise died with the placeholder arms: a real install() bootstraps
    // launchd on macOS, registers a schtasks task on Windows, and refuses on
    // Linux until Task 9b — none of which belongs in the default suite.

    #[test]
    fn artifact_key_collides_exactly_when_labels_collide() {
        let home = tempfile::tempdir().unwrap();
        let ctx = test_support::ctx_in(home.path());
        let a = test_support::daily_entry();
        let b = test_support::daily_command_entry();
        assert_eq!(artifact_key(&a, &ctx), artifact_key(&a, &ctx));
        assert_ne!(artifact_key(&a, &ctx), artifact_key(&b, &ctx));
    }
}
