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
//! for avoiding it. The non-macOS arm below is a deliberately
//! behaviour-preserving placeholder, replaced by Task 6 (Linux refuses) and
//! Task 7c (Windows gets schtasks).

use crate::scheduler::context::SchedulerContext;
use crate::scheduler::error::SchedulerError;
use crate::scheduler::launchd::{generate_plist, plist_path};
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

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::process::Command;

    fn service_target(label_safe: &str, ctx: &SchedulerContext) -> String {
        format!("gui/{}/com.onebrain.{label_safe}", ctx.uid)
    }

    /// Test-isolation kill-switch, NOT a user feature.
    ///
    /// Labels are a process-global launchd namespace: an integration test
    /// running the real binary with `HOME=tempdir` still targets the REAL
    /// `gui/<uid>/com.onebrain.daily` on bootout/bootstrap. The first suite
    /// run after activation landed hijacked the operator's real `/daily`
    /// (bootstrapped it from a since-deleted tempdir) and deleted its plist.
    /// The integration harness sets this for every spawned binary; nothing
    /// else should.
    fn activation_disabled() -> bool {
        std::env::var_os("ONEBRAIN_SCHEDULER_NO_ACTIVATE").is_some()
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

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    /// PLACEHOLDER, and deliberately behaviour-preserving: it does exactly
    /// what the code does today on these platforms — writes a launchd plist
    /// that nothing will ever read. That is wrong, and it is the *documented
    /// current bug* (#313): Task 6 is where Linux starts refusing, and Task 7c
    /// is where Windows gets a real backend.
    ///
    /// Preserving the wrong behaviour here is what makes Task 3 a pure seam
    /// with no behaviour change anywhere — and it is what Task 6's failing
    /// test asserts against ("install succeeds and ~/Library exists"). If
    /// this arm refused instead, that test would pass before Task 6 wrote a
    /// line, and Task 6 would prove nothing.
    pub fn install(
        entry: &ScheduleEntry,
        ctx: &SchedulerContext,
    ) -> Result<PathBuf, SchedulerError> {
        write_plist(entry, ctx)
    }

    pub fn remove(label_safe: &str, ctx: &SchedulerContext) -> Result<bool, SchedulerError> {
        let target = plist_path(label_safe, &ctx.homedir);
        if target.exists() {
            std::fs::remove_file(&target)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn is_installed(
        label_safe: &str,
        ctx: &SchedulerContext,
    ) -> Result<InstallState, SchedulerError> {
        let target = plist_path(label_safe, &ctx.homedir);
        Ok(if target.exists() {
            InstallState::Active
        } else {
            InstallState::Absent
        })
    }

    pub fn describe() -> &'static str {
        "launchd (placeholder — not this platform's scheduler)"
    }
}

/// Create the scheduler log directory before any artifact references it.
///
/// launchd opens `StandardOutPath`/`StandardErrorPath` **before exec** and
/// does not create parent directories. The machine-local default
/// (`~/Library/Logs/onebrain`) does not exist on a fresh install, so skipping
/// this ships EX_CONFIG-78-with-no-output — a byte-for-byte re-creation of
/// the #315 headline bug (round 4, BL-2: "the cheapest way to sink the
/// release").
pub fn ensure_log_dir(ctx: &SchedulerContext) -> std::io::Result<()> {
    std::fs::create_dir_all(&ctx.log_base_path)
}

/// Shared by both arms while the placeholder exists; Task 6/7c specialise.
fn write_plist(entry: &ScheduleEntry, ctx: &SchedulerContext) -> Result<PathBuf, SchedulerError> {
    ensure_log_dir(ctx)?;
    let label = crate::scheduler::launchd::label_for_entry(entry);
    let target = plist_path(&label, &ctx.homedir);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, generate_plist(entry, ctx))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::test_support;

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
    #[cfg_attr(
        target_os = "macos",
        ignore = "touches the real launchd domain — run explicitly, never in the default suite"
    )]
    fn install_then_remove_round_trips_through_the_backend() {
        let home = tempfile::tempdir().unwrap();
        let ctx = test_support::ctx_in(home.path());
        let label = unique_label("roundtrip");
        let entry = test_support::entry_labelled(&label);

        // Teardown must run even if an assertion panics: a leaked bootstrapped
        // job pointing at a deleted tempdir outlives the test process.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let path = install(&entry, &ctx).unwrap();
            assert!(path.exists(), "install must write the artifact");
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
        target_os = "macos",
        ignore = "touches the real launchd domain — run explicitly, never in the default suite"
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
        std::fs::write(&target, generate_plist(&entry, &ctx)).unwrap();

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

    #[test]
    fn install_creates_the_log_directory_as_part_of_writing() {
        // Filesystem-only in Task 3's placeholder arms — safe everywhere.
        // The OS-touching round-trip lives in Task 4's #[ignore]d tests.
        let home = tempfile::tempdir().unwrap();
        let ctx = test_support::ctx_in(home.path());
        install(&test_support::entry_labelled("logdir-probe"), &ctx).unwrap();
        assert!(ctx.log_base_path.is_dir());
        let _ = remove("logdir-probe", &ctx);
    }

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
