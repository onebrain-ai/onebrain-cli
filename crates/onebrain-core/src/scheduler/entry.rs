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

    if has_command && entry.harness.is_some() {
        return Err(SchedulerError::InvalidEntry {
            reason: "`harness` is valid only for skill-mode entries".into(),
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

    #[test]
    fn validate_rejects_harness_on_command_mode() {
        let mut e = cmd_cron();
        e.harness = Some(crate::Harness::Codex);
        let err = validate_entry(&e).unwrap_err();
        assert!(err.to_string().contains("harness"));
    }
}

/// Reject control characters in user-supplied entry values, naming the field.
///
/// Enforced by ALL THREE renderers, because no scheduler format can carry one
/// honestly: XML 1.0 forbids the C0 control characters
/// outright, and the numeric form does not rescue them — `&#1;` is itself
/// illegal. A raw one produces a document `plutil` / `launchctl bootstrap` /
/// `schtasks /Create /XML` reject, and the user sees an opaque backend error
/// naming nothing.
///
/// Carriage return is refused for a quieter reason: it IS legal, but XML
/// line-end normalization rewrites it to `\n` on parse, so the argument the
/// child receives differs from the one written in `onebrain.yml` — a silent
/// value change, which is worse than a loud rejection.
///
/// Newline is refused too: systemd cannot carry it at all, and the launchd
/// one-shot path interpolates args into a `/bin/sh -c` string where it is a
/// command separator. Refusing on every backend keeps one config portable.
///
/// TAB is allowed — legal XML, survives the round trip unchanged, and carries
/// no meaning in any of the three sinks.
pub fn reject_control_chars(field: &str, value: &str) -> Result<(), SchedulerError> {
    if let Some(bad) = value.chars().find(|c| c.is_control() && *c != '\t') {
        return Err(SchedulerError::InvalidEntry {
            reason: format!(
                "{field} contains the control character U+{:04X}, which no scheduler format can carry: {value:?}",
                bad as u32
            ),
        });
    }
    Ok(())
}

/// Apply [`reject_control_chars`] to every USER-SUPPLIED string on an entry.
///
/// Called by every renderer. A Linux gate-8 run caught the first version wiring
/// it into the two XML sinks only — systemd's own `sanitize_unit_value` refuses
/// newlines and nothing else, so `\u{1}` registered cleanly there while being
/// refused on macOS and Windows. One config, three platforms, one answer.
///
/// Only the fields a person types into `onebrain.yml` — command, skill, and
/// each arg (list element, or map key AND value). Context-derived paths are
/// machine-built and cannot carry a control character without the machine
/// already being broken.
///
/// Field names in the error are the YAML shape the user actually wrote
/// (`args[2]`, `args.topic`), so the message points at the line to edit rather
/// than at a rendered argv index they never see.
pub fn reject_control_chars_in_entry(entry: &ScheduleEntry) -> Result<(), SchedulerError> {
    if let Some(c) = entry.command.as_deref() {
        reject_control_chars("command", c)?;
    }
    if let Some(s) = entry.skill.as_deref() {
        reject_control_chars("skill", s)?;
    }
    match &entry.args {
        Some(Args::List(list)) => {
            for (i, a) in list.iter().enumerate() {
                reject_control_chars(&format!("args[{i}]"), a)?;
            }
        }
        Some(Args::Map(map)) => {
            for (k, v) in map {
                // The KEY matters as much as the value: it is interpolated into
                // the same `--arg key=value` token.
                reject_control_chars(&format!("args key {k:?}"), k)?;
                reject_control_chars(&format!("args.{k}"), v)?;
            }
        }
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod control_char_tests {
    use super::*;

    fn entry_with_arg(a: &str) -> ScheduleEntry {
        ScheduleEntry {
            cron: Some("0 9 * * *".into()),
            command: Some("/bin/echo".into()),
            args: Some(Args::List(vec![a.to_string()])),
            ..Default::default()
        }
    }

    /// XML 1.0 forbids C0 controls outright and `&#1;` is illegal too, so there
    /// is nothing to escape INTO — refusal is the only correct answer.
    #[test]
    fn a_control_character_is_refused_and_the_message_names_it() {
        let err = reject_control_chars("args[0]", "a\u{1}b").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("args[0]"), "msg: {msg}");
        assert!(msg.contains("U+0001"), "msg: {msg}");
    }

    /// CR is LEGAL XML — refused anyway, because line-end normalization
    /// rewrites it to `\n` on parse and the child then receives a different
    /// value than the config says. A silent value change beats no error only
    /// for whoever wrote the parser.
    #[test]
    fn carriage_return_is_refused_even_though_xml_permits_it() {
        assert!(reject_control_chars("args[0]", "a\rb").is_err());
        assert!(reject_control_chars("args[0]", "a\nb").is_err());
    }

    /// Tab survives the round trip unchanged and means nothing to any of the
    /// three sinks, so refusing it would only reject valid configs.
    #[test]
    fn tab_is_allowed() {
        assert!(reject_control_chars("args[0]", "a\tb").is_ok());
        assert!(reject_control_chars("args[0]", "ordinary text").is_ok());
    }

    /// The error must point at the YAML the user wrote, not at a rendered argv
    /// index they never see.
    #[test]
    fn entry_scan_names_the_field_the_user_typed() {
        let err = reject_control_chars_in_entry(&entry_with_arg("x\u{7}y")).unwrap_err();
        assert!(err.to_string().contains("args[0]"), "{err}");

        let mut e = entry_with_arg("fine");
        e.command = Some("/bin/ech\u{0}o".into());
        assert!(reject_control_chars_in_entry(&e)
            .unwrap_err()
            .to_string()
            .contains("command"));
    }

    /// A map KEY is interpolated into the same `--arg key=value` token as its
    /// value, so scanning only values would leave half the surface open.
    #[test]
    fn map_arg_keys_are_scanned_not_just_values() {
        let mut m = indexmap::IndexMap::new();
        m.insert("to\u{1}pic".to_string(), "fine".to_string());
        let mut e = entry_with_arg("x");
        e.command = None;
        e.skill = Some("/distill".into());
        e.args = Some(Args::Map(m));
        assert!(reject_control_chars_in_entry(&e).is_err());
    }
}
