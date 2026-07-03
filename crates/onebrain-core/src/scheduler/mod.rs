//! OneBrain scheduler library · pure-Rust port of Bun
//! `src/lib/scheduler/`. Consumed by the `register-schedule` CLI command
//! and by the runtime that invokes scheduled skills.
//!
//! ## Layout
//!
//! Modules mirror the Bun layout one-to-one so future cross-implementation
//! changes can be traced by filename:
//!
//! - [`types`] — `ScheduleEntry`, `ScheduleConfig`, `SkillFrontmatter`, `Args`
//! - [`cron_parse`] — `validate_cron`, `validate_at`, conversion to launchd fields
//! - [`entry`] — `is_*` classifiers + `validate_entry` shape check
//! - [`launchd`] — plist emitter (byte-for-byte parity with Bun's templating)
//! - [`log_paths`] — runtime stdout/stderr log path builder
//! - [`error`] — `SchedulerError` (matched error strings for parity tests)

pub mod cron_parse;
pub mod entry;
pub mod error;
pub mod launchd;
pub mod log_paths;
pub mod types;

pub use cron_parse::{
    at_to_launchd, cron_fields_to_launchd, cron_fields_to_launchd_expanded, validate_at,
    validate_cron, AtFields, CronFieldSet, CronFields,
};
pub use entry::{is_command_mode, is_one_shot, is_skill_mode, validate_entry};
pub use error::SchedulerError;
pub use launchd::{generate_plist, label_for_entry, plist_path, xml_escape, LaunchdContext};
pub use log_paths::scheduler_log_path;
pub use types::{Args, ScheduleConfig, ScheduleEntry, SkillFrontmatter};
