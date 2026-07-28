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
//! - [`entry`] — `is_*` classifiers + `validate_entry` shape check
//! - [`xml`] — XML escaping, shared by every renderer that emits XML
//! - [`log_dir`] — machine-local log destination + in-vault detection (#315)
//! - [`launchd`] — plist emitter (byte-for-byte parity with Bun's templating)
//! - [`log_paths`] — runtime stdout/stderr log path builder
//! - [`error`] — `SchedulerError` (matched error strings for parity tests)

pub mod backend;
pub mod context;
pub mod cron_parse;
pub mod entry;
pub mod error;
pub mod launchd;
pub mod log_dir;
pub mod log_paths;
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
pub use launchd::{generate_plist, label_for_entry, plist_path};
pub use log_dir::{default_log_dir, is_in_vault};
pub use log_paths::scheduler_log_path;
pub use types::{Args, ScheduleConfig, ScheduleEntry, SkillFrontmatter};
pub use xml::escape as xml_escape;
