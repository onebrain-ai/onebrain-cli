//! Schedule preset definitions.
//!
//! Mirrors the four-tier table from
//! `.claude/plugins/onebrain/skills/_shared/schedule-presets.md`. The names
//! and entry layout are canonical — keep them in lockstep with the shared
//! doc when either changes.

use indexmap::IndexMap;
use serde::Serialize;
use std::fmt;

/// User's preset choice during init.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchedulePreset {
    /// One daily entry (`/daily` at 09:00).
    Minimal,
    /// Three entries: `/daily`, `/weekly` Friday, `/recap` Sunday.
    #[default]
    Essentials,
    /// Essentials plus `/doctor` monthly, `/tasks` daily, `qmd-reindex`
    /// Sunday (command-mode entry).
    MaintenancePlus,
    /// No entries — leaves `schedule:` out of vault.yml entirely.
    Skip,
}

impl SchedulePreset {
    /// Stable label used in confirmation messages + CLI output. Matches
    /// the wording from `_shared/schedule-presets.md`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Minimal => "Minimal",
            Self::Essentials => "Essentials (Recommended)",
            Self::MaintenancePlus => "Maintenance Plus",
            Self::Skip => "Skip",
        }
    }

    /// Entries to write under `schedule:` in vault.yml. `Skip` returns an
    /// empty vec — the caller is responsible for omitting the key entirely
    /// in that case.
    pub fn entries(&self) -> Vec<ScheduleEntry> {
        match self {
            Self::Minimal => vec![ScheduleEntry::skill("0 9 * * *", "/daily")],
            Self::Essentials => vec![
                ScheduleEntry::skill("0 9 * * *", "/daily"),
                ScheduleEntry::skill("0 17 * * 5", "/weekly"),
                ScheduleEntry::skill("0 12 * * 0", "/recap"),
            ],
            Self::MaintenancePlus => vec![
                ScheduleEntry::skill("0 9 * * *", "/daily"),
                ScheduleEntry::skill("0 17 * * 5", "/weekly"),
                ScheduleEntry::skill("0 12 * * 0", "/recap"),
                ScheduleEntry::skill("0 9 1 * *", "/doctor"),
                ScheduleEntry::skill("0 6 * * *", "/tasks"),
                ScheduleEntry::command("0 3 * * 0", "onebrain", vec!["qmd-reindex".to_string()]),
            ],
            Self::Skip => vec![],
        }
    }

    /// All four variants, in the order they're presented to the user.
    pub fn all() -> &'static [Self] {
        &[
            Self::Minimal,
            Self::Essentials,
            Self::MaintenancePlus,
            Self::Skip,
        ]
    }
}

impl fmt::Display for SchedulePreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One entry under `schedule:` — either a skill-mode invocation or a raw
/// command-mode invocation. Shape matches the vault.yml schema documented
/// in `INSTRUCTIONS.md` (`schedule:` block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ScheduleEntry {
    Skill {
        cron: String,
        skill: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<IndexMap<String, String>>,
    },
    Command {
        cron: String,
        command: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
}

impl ScheduleEntry {
    pub fn skill(cron: impl Into<String>, skill: impl Into<String>) -> Self {
        Self::Skill {
            cron: cron.into(),
            skill: skill.into(),
            args: None,
        }
    }

    pub fn command(cron: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self::Command {
            cron: cron.into(),
            command: command.into(),
            args,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_has_one_daily_entry() {
        let entries = SchedulePreset::Minimal.entries();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            ScheduleEntry::Skill { cron, skill, .. } => {
                assert_eq!(cron, "0 9 * * *");
                assert_eq!(skill, "/daily");
            }
            _ => panic!("expected skill entry"),
        }
    }

    #[test]
    fn essentials_has_three_entries_daily_weekly_recap() {
        let entries = SchedulePreset::Essentials.entries();
        assert_eq!(entries.len(), 3);
        let skills: Vec<&str> = entries
            .iter()
            .filter_map(|e| match e {
                ScheduleEntry::Skill { skill, .. } => Some(skill.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(skills, vec!["/daily", "/weekly", "/recap"]);
    }

    #[test]
    fn maintenance_plus_has_six_entries_with_one_command_mode() {
        let entries = SchedulePreset::MaintenancePlus.entries();
        assert_eq!(entries.len(), 6);
        let cmd_count = entries
            .iter()
            .filter(|e| matches!(e, ScheduleEntry::Command { .. }))
            .count();
        assert_eq!(cmd_count, 1);
    }

    #[test]
    fn skip_returns_zero_entries() {
        assert!(SchedulePreset::Skip.entries().is_empty());
    }

    #[test]
    fn label_is_stable_for_all_variants() {
        assert_eq!(SchedulePreset::Minimal.label(), "Minimal");
        assert_eq!(
            SchedulePreset::Essentials.label(),
            "Essentials (Recommended)"
        );
        assert_eq!(SchedulePreset::MaintenancePlus.label(), "Maintenance Plus");
        assert_eq!(SchedulePreset::Skip.label(), "Skip");
    }

    #[test]
    fn display_uses_label() {
        assert_eq!(
            format!("{}", SchedulePreset::Essentials),
            "Essentials (Recommended)"
        );
    }

    #[test]
    fn all_returns_four_variants_in_order() {
        let all = SchedulePreset::all();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], SchedulePreset::Minimal);
        assert_eq!(all[1], SchedulePreset::Essentials);
        assert_eq!(all[2], SchedulePreset::MaintenancePlus);
        assert_eq!(all[3], SchedulePreset::Skip);
    }

    #[test]
    fn default_is_essentials() {
        assert_eq!(SchedulePreset::default(), SchedulePreset::Essentials);
    }

    #[test]
    fn skill_entry_serializes_without_null_args() {
        let e = ScheduleEntry::skill("0 9 * * *", "/daily");
        let yaml = serde_yaml::to_string(&e).unwrap();
        assert!(
            !yaml.contains("args"),
            "Skill with no args should omit args: key, got {yaml}"
        );
        assert!(yaml.contains("cron: 0 9 * * *"));
        assert!(yaml.contains("skill: /daily"));
    }

    #[test]
    fn command_entry_serializes_with_args_list() {
        let e = ScheduleEntry::command("0 3 * * 0", "onebrain", vec!["qmd-reindex".into()]);
        let yaml = serde_yaml::to_string(&e).unwrap();
        assert!(yaml.contains("command: onebrain"));
        assert!(yaml.contains("qmd-reindex"));
    }
}
