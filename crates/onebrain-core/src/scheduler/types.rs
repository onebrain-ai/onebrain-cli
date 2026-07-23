//! Scheduler data types · mirrors Bun `src/lib/scheduler/types.ts`.
//!
//! `ScheduleEntry` is the unit of work the user writes in `vault.yml`:
//! exactly one of `cron`/`at` (recurring vs one-shot) and exactly one of
//! `skill`/`command` (OneBrain skill vs raw CLI binary).
//!
//! The `Args` enum encodes the asymmetry Bun's TS type union represents:
//! skill mode requires a string map (`--arg key=value` flags) while command
//! mode requires a string list (positional argv). Serde's `untagged` shape
//! lets a single YAML file drive both — `validate_entry` later catches
//! mismatches.

use crate::Harness;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Args interpretation depends on entry mode:
/// - skill mode → [`Args::Map`] (emitted as `--arg key=value` flags)
/// - command mode → [`Args::List`] (emitted as positional argv)
///
/// `IndexMap` preserves YAML insertion order so plist emission matches
/// Bun's `Object.entries` iteration byte-for-byte.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Args {
    /// Skill-mode args (key=value pairs).
    Map(IndexMap<String, String>),
    /// Command-mode args (positional argv).
    List(Vec<String>),
}

/// A single `vault.yml` `schedule:` list entry. Exactly one of `cron`/`at`
/// and exactly one of `skill`/`command` must be set; this is enforced by
/// [`crate::scheduler::entry::validate_entry`] (NOT at the type level — the
/// loader needs to surface a friendly error string rather than a serde
/// parse failure when users misconfigure).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ScheduleEntry {
    /// 5-field cron expression · exactly-one-of with `at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,

    /// One-shot timestamp `YYYY-MM-DD HH:MM` · exactly-one-of with `cron`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,

    /// OneBrain skill name (e.g. `/daily`) · exactly-one-of with `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,

    /// CLI binary name or path (e.g. `onebrain`, `/usr/bin/rsync`) ·
    /// exactly-one-of with `skill`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<Harness>,

    /// Args (map for skill mode · list for command mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Args>,
}

impl ScheduleEntry {
    pub fn effective_harness(&self) -> Harness {
        self.harness.unwrap_or(Harness::Claude)
    }
}

/// Top-level `vault.yml` shape (only the `schedule:` block is parsed here).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScheduleConfig {
    #[serde(default)]
    pub schedule: Vec<ScheduleEntry>,
}

/// `SKILL.md` frontmatter shape · drives schedulability validation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillFrontmatter {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub schedulable: Option<bool>,
    #[serde(default)]
    pub schedulable_with_args: Option<bool>,
    #[serde(default)]
    pub required_args: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_deserializes_map_from_yaml() {
        let y = "args:\n  topic: this-week\n";
        let e: ScheduleEntry = serde_yaml::from_str(y).unwrap();
        match e.args.unwrap() {
            Args::Map(m) => {
                assert_eq!(m.get("topic").map(String::as_str), Some("this-week"));
            }
            Args::List(_) => panic!("expected map"),
        }
    }

    #[test]
    fn args_deserializes_list_from_yaml() {
        let y = "args:\n  - hello\n  - world\n";
        let e: ScheduleEntry = serde_yaml::from_str(y).unwrap();
        match e.args.unwrap() {
            Args::List(v) => assert_eq!(v, vec!["hello".to_string(), "world".to_string()]),
            Args::Map(_) => panic!("expected list"),
        }
    }

    #[test]
    fn args_map_preserves_insertion_order() {
        // YAML insertion: z, a, m → IndexMap must keep that order so plist
        // emission iterates the same way Bun's `Object.entries` does. Values
        // are quoted strings — unquoted `1`/`2`/`3` would parse as ints and
        // fail the (String → String) map shape.
        let y = "args:\n  zeta: \"1\"\n  alpha: \"2\"\n  mu: \"3\"\n";
        let e: ScheduleEntry = serde_yaml::from_str(y).unwrap();
        if let Args::Map(m) = e.args.unwrap() {
            let keys: Vec<&str> = m.keys().map(String::as_str).collect();
            assert_eq!(keys, vec!["zeta", "alpha", "mu"]);
        } else {
            panic!("expected map");
        }
    }

    #[test]
    fn schedule_config_empty_when_no_schedule_key() {
        let cfg: ScheduleConfig = serde_yaml::from_str("# nothing here\n").unwrap();
        assert!(cfg.schedule.is_empty());
    }

    #[test]
    fn schedule_config_loads_minimal_entry() {
        let y = "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n";
        let cfg: ScheduleConfig = serde_yaml::from_str(y).unwrap();
        assert_eq!(cfg.schedule.len(), 1);
        assert_eq!(cfg.schedule[0].cron.as_deref(), Some("0 9 * * *"));
        assert_eq!(cfg.schedule[0].skill.as_deref(), Some("/daily"));
    }

    #[test]
    fn schedule_harness_round_trips_and_defaults_to_claude() {
        let yaml = "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n    harness: codex\n";
        let cfg: ScheduleConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.schedule[0].harness, Some(Harness::Codex));
        let legacy: ScheduleConfig =
            serde_yaml::from_str("schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n")
                .unwrap();
        assert_eq!(legacy.schedule[0].effective_harness(), Harness::Claude);
    }

    #[test]
    fn schedule_config_loads_command_entry_with_array_args() {
        let y =
            "schedule:\n  - cron: \"0 3 * * 0\"\n    command: onebrain\n    args:\n      - qmd-reindex\n";
        let cfg: ScheduleConfig = serde_yaml::from_str(y).unwrap();
        assert_eq!(cfg.schedule[0].command.as_deref(), Some("onebrain"));
        match cfg.schedule[0].args.as_ref().unwrap() {
            Args::List(v) => assert_eq!(v, &vec!["qmd-reindex".to_string()]),
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn skill_frontmatter_defaults_all_optional_fields_to_none() {
        let fm: SkillFrontmatter = serde_yaml::from_str("name: daily\n").unwrap();
        assert_eq!(fm.name.as_deref(), Some("daily"));
        assert!(fm.schedulable.is_none());
        assert!(fm.schedulable_with_args.is_none());
        assert!(fm.required_args.is_none());
    }
}
