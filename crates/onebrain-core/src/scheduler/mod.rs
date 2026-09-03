//! OneBrain scheduler library · pure-Rust port of Bun
//! `src/lib/scheduler/`. Consumed by the `register-schedule` CLI command
//! and by the runtime that invokes scheduled skills.
//!
//! ## Layout
//!
//! Modules mirror the Bun layout one-to-one so future cross-implementation
//! changes can be traced by filename:
//!
//! - [`backend`] — the impure install/remove/query seam, cfg-selected per OS
//! - [`context`] — `SchedulerContext`, the inputs every backend renderer takes
//! - [`types`] — `ScheduleEntry`, `ScheduleConfig`, `SkillFrontmatter`, `Args`
//! - [`cron_parse`] — `validate_cron`, `validate_at`, expansion to calendar fields
//! - [`entry`] — `is_*` classifiers, `validate_entry` shape check, and the
//!   control-character refusal shared by all three renderers
//! - [`xml`] — XML escaping, shared by every renderer that emits XML
//! - [`log_dir`] — machine-local log destination + in-vault detection (#315)
//! - [`launchd`] — plist emitter (byte-for-byte parity with Bun's templating)
//! - [`systemd`] — Linux user-timer unit rendering (pure; corpus-backed)
//! - [`schtasks`] — Windows trigger translation (pure; corpus-backed)
//! - [`log_paths`] — runtime stdout/stderr log path builder
//! - [`reconcile`] — ownership types, reconcile planner, and per-backend
//!   artifact-ownership parsers (#410, #352)
//! - [`run_log`] — the CLI-owned job log, opened after exec (#372)
//! - [`run_record`] — pure rendering of a scheduled run into a vault record (#363)
//! - [`error`] — `SchedulerError` (matched error strings for parity tests)

pub mod backend;
pub mod context;
pub mod cron_parse;
pub mod entry;
pub mod error;
pub mod launchd;
pub mod log_dir;
pub mod log_paths;
pub mod reconcile;
pub mod run_log;
pub mod run_record;
pub mod schtasks;
pub mod systemd;
pub mod types;
pub mod xml;

#[cfg(test)]
pub mod test_support;

pub use backend::{
    artifact_key, describe, ensure_log_dir, install, is_installed, remove, InstallState,
};
pub use context::SchedulerContext;
pub use cron_parse::{
    at_fields, cron_fields, cron_fields_expanded, validate_at, validate_cron, AtFields,
    CronFieldSet, CronFields,
};
pub use entry::{is_command_mode, is_one_shot, is_skill_mode, validate_entry};
pub use error::SchedulerError;
pub use launchd::{generate_plist, label_for_entry, legacy_truncated_label, plist_path};
pub use log_dir::{default_log_dir, is_in_vault};
pub use log_paths::scheduler_log_path;
pub use reconcile::{normalize_path, plan_reconcile, InstalledArtifact, Ownership, ReconcilePlan};
pub use types::{Args, ScheduleConfig, ScheduleEntry, SkillFrontmatter};
pub use xml::escape as xml_escape;
pub use xml::unescape as xml_unescape;
