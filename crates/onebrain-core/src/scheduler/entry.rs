//! Schedule entry classifiers + shape validator. Mirrors Bun
//! `src/lib/scheduler/entry.ts`.

use crate::scheduler::error::SchedulerError;
use crate::scheduler::types::{Args, ScheduleEntry};

/// One-shot entry has `at:` set (cron is mutually exclusive).
pub fn is_one_shot(entry: &ScheduleEntry) -> bool {
    entry.at.is_some()
}

/// Skill-mode entry has `skill:` set (command is mutually exclusive).
pub fn is_skill_mode(entry: &ScheduleEntry) -> bool {
    entry.skill.is_some()
}

/// Command-mode entry has `command:` set (skill is mutually exclusive).
pub fn is_command_mode(entry: &ScheduleEntry) -> bool {
    entry.command.is_some()
}

/// Validate the shape of a schedule entry.
///
/// Enforces:
/// - exactly one of `cron`/`at`
/// - exactly one of `skill`/`command`
/// - non-empty skill / command (empty string is rejected)
/// - args type matches mode (map for skill · list for command)
///
/// Field-format validation (cron syntax, at format, schedulable frontmatter)
/// lives elsewhere — this fn is purely structural.
pub fn validate_entry(entry: &ScheduleEntry) -> Result<(), SchedulerError> {
    let has_cron = entry.cron.is_some();
    let has_at = entry.at.is_some();
    if has_cron == has_at {
        return Err(SchedulerError::InvalidEntry {
            reason: "entry must have exactly one of `cron` or `at`".into(),
        });
    }

    let has_skill = entry.skill.is_some();
    let has_command = entry.command.is_some();
    if has_skill == has_command {
        return Err(SchedulerError::InvalidEntry {
            reason: "entry must have exactly one of `skill` or `command`".into(),
        });
    }

    if has_skill && entry.skill.as_deref() == Some("") {
        return Err(SchedulerError::InvalidEntry {
            reason: "entry.skill must not be empty".into(),
        });
    }
    if has_command && entry.command.as_deref() == Some("") {
        return Err(SchedulerError::InvalidEntry {
            reason: "entry.command must not be empty".into(),
        });
    }

    if let Some(args) = &entry.args {
        match args {
            Args::List(_) if has_skill => {
                return Err(SchedulerError::InvalidEntry {
                    reason: "skill-mode entries require `args` as a map (Record<string, string>), not an array".into(),
                });
            }
            Args::Map(_) if has_command => {
                return Err(SchedulerError::InvalidEntry {
                    reason: "command-mode entries require `args` as a string array, not a map"
                        .into(),
                });
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn skill_cron() -> ScheduleEntry {
        ScheduleEntry {
            cron: Some("0 9 * * *".into()),
            skill: Some("/daily".into()),
            ..Default::default()
        }
    }
    fn cmd_cron() -> ScheduleEntry {
        ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("onebrain".into()),
            ..Default::default()
        }
    }

    #[test]
    fn is_one_shot_true_for_at_entry() {
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            skill: Some("/x".into()),
            ..Default::default()
        };
        assert!(is_one_shot(&e));
    }

    #[test]
    fn is_one_shot_false_for_cron_entry() {
        assert!(!is_one_shot(&skill_cron()));
    }

    #[test]
    fn is_skill_mode_true_for_skill_entry() {
        assert!(is_skill_mode(&skill_cron()));
    }

    #[test]
    fn is_skill_mode_false_for_command_entry() {
        assert!(!is_skill_mode(&cmd_cron()));
    }

    #[test]
    fn is_command_mode_true_for_command_entry() {
        assert!(is_command_mode(&cmd_cron()));
    }

    #[test]
    fn is_command_mode_false_for_skill_entry() {
        assert!(!is_command_mode(&skill_cron()));
    }

    #[test]
    fn validate_rejects_both_cron_and_at() {
        let e = ScheduleEntry {
            cron: Some("0 9 * * *".into()),
            at: Some("2026-05-13 14:30".into()),
            skill: Some("/x".into()),
            ..Default::default()
        };
        let err = validate_entry(&e).unwrap_err();
        assert!(err.to_string().contains("exactly one"), "msg was: {err}");
    }

    #[test]
    fn validate_rejects_neither_cron_nor_at() {
        let e = ScheduleEntry {
            skill: Some("/x".into()),
            ..Default::default()
        };
        let err = validate_entry(&e).unwrap_err();
        assert!(err.to_string().contains("exactly one"));
    }

    #[test]
    fn validate_rejects_empty_skill() {
        let e = ScheduleEntry {
            cron: Some("0 9 * * *".into()),
            skill: Some("".into()),
            ..Default::default()
        };
        let err = validate_entry(&e).unwrap_err();
        assert!(err.to_string().contains("skill"), "msg was: {err}");
    }

    #[test]
    fn validate_accepts_cron_only_skill_entry() {
        assert!(validate_entry(&skill_cron()).is_ok());
    }

    #[test]
    fn validate_accepts_at_only_skill_entry() {
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            skill: Some("/reminder".into()),
            ..Default::default()
        };
        assert!(validate_entry(&e).is_ok());
    }

    #[test]
    fn validate_accepts_command_with_array_args() {
        let mut e = cmd_cron();
        e.args = Some(Args::List(vec!["qmd-reindex".into()]));
        assert!(validate_entry(&e).is_ok());
    }

    #[test]
    fn validate_accepts_command_without_args() {
        assert!(validate_entry(&cmd_cron()).is_ok());
    }

    #[test]
    fn validate_rejects_both_skill_and_command() {
        let e = ScheduleEntry {
            cron: Some("0 9 * * *".into()),
            skill: Some("/daily".into()),
            command: Some("onebrain".into()),
            ..Default::default()
        };
        let err = validate_entry(&e).unwrap_err();
        assert!(err.to_string().contains("`skill` or `command`"));
    }

    #[test]
    fn validate_rejects_neither_skill_nor_command() {
        let e = ScheduleEntry {
            cron: Some("0 9 * * *".into()),
            ..Default::default()
        };
        let err = validate_entry(&e).unwrap_err();
        assert!(err.to_string().contains("`skill` or `command`"));
    }

    #[test]
    fn validate_rejects_skill_mode_with_array_args() {
        let mut e = skill_cron();
        e.args = Some(Args::List(vec!["x".into()]));
        let err = validate_entry(&e).unwrap_err();
        assert!(err.to_string().contains("skill-mode"));
    }

    #[test]
    fn validate_rejects_command_mode_with_map_args() {
        let mut e = cmd_cron();
        let mut map = IndexMap::new();
        map.insert("k".into(), "v".into());
        e.args = Some(Args::Map(map));
        let err = validate_entry(&e).unwrap_err();
        assert!(err.to_string().contains("command-mode"));
    }
}
