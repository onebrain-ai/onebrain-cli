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

    /// Write the plist. Exactly the sequence `register_schedule.rs` performed
    /// before the seam existed — moved, not changed. `launchctl bootstrap`
    /// comes in Task 4.
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

    /// File existence, for now. Task 4 replaces this with a launchctl query —
    /// a file on disk must not read as scheduled (#312).
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

/// Shared by both arms while the placeholder exists; Task 6/7c specialise.
fn write_plist(entry: &ScheduleEntry, ctx: &SchedulerContext) -> Result<PathBuf, SchedulerError> {
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
        // File-existence only in Task 3 — safe everywhere. When Task 4 makes
        // this a live launchctl query, THIS test must gain #[ignore] per [B2];
        // the label is already non-colliding by construction.
        let home = tempfile::tempdir().unwrap();
        let ctx = test_support::ctx_in(home.path());
        assert_eq!(
            is_installed("onebrain-test-no-such-label", &ctx).unwrap(),
            InstallState::Absent
        );
    }

    #[test]
    fn install_then_remove_round_trips_on_the_filesystem() {
        let home = tempfile::tempdir().unwrap();
        let ctx = test_support::ctx_in(home.path());
        let entry = test_support::entry_labelled("seam-roundtrip");

        let path = install(&entry, &ctx).unwrap();
        assert!(path.exists(), "install must write the artifact");
        assert_eq!(
            is_installed("seam-roundtrip", &ctx).unwrap(),
            InstallState::Active
        );

        assert!(remove("seam-roundtrip", &ctx).unwrap());
        assert_eq!(
            is_installed("seam-roundtrip", &ctx).unwrap(),
            InstallState::Absent
        );
        assert!(
            !remove("seam-roundtrip", &ctx).unwrap(),
            "second remove is a no-op"
        );
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
