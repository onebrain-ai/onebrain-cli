//! Cron + at-string validation and conversion to launchd
//! `StartCalendarInterval` fields. Mirrors Bun
//! `src/lib/scheduler/cron-parse.ts` line-for-line.
//!
//! Only single integers and `*` are supported per field — launchd's
//! `StartCalendarInterval` accepts one integer per field, so step (`*/5`),
//! range (`1-5`), and list (`1,3,5`) syntax are explicitly rejected.

use crate::scheduler::error::SchedulerError;
use regex::Regex;
use std::sync::OnceLock;

fn cron_field_re() -> &'static Regex {
    // Single wildcard `*` OR a single integer. No step/range/list.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\*|\d+)$").unwrap())
}

fn at_re() -> &'static Regex {
    // YYYY-MM-DD HH:MM (literal space, two digits each except year).
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2})$").unwrap())
}

/// Validate a 5-field cron string. Returns `Err(SchedulerError::InvalidCron)`
/// with the same reason strings Bun emits ("expected 5 fields, got N",
/// "invalid field syntax: \"foo\"").
pub fn validate_cron(cron: &str) -> Result<(), SchedulerError> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(SchedulerError::InvalidCron {
            cron: cron.to_string(),
            reason: format!("expected 5 fields, got {}", fields.len()),
        });
    }
    for f in &fields {
        if !cron_field_re().is_match(f) {
            return Err(SchedulerError::InvalidCron {
                cron: cron.to_string(),
                reason: format!("invalid field syntax: \"{f}\""),
            });
        }
    }
    Ok(())
}

/// Launchd `StartCalendarInterval` fields for a recurring (cron) entry.
/// `None` for a field means launchd matches every value (wildcard).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CronFields {
    pub minute: Option<u32>,
    pub hour: Option<u32>,
    pub day: Option<u32>,
    pub month: Option<u32>,
    pub weekday: Option<u32>,
}

/// Convert a validated 5-field cron string to launchd `CronFields`.
///
/// Panics if `cron` did not pass [`validate_cron`] — the caller is
/// responsible for ordering. Bun's contract is identical (docstring
/// "Assumes `cron` already passed `validateCron`.").
pub fn cron_fields_to_launchd(cron: &str) -> CronFields {
    let f: Vec<&str> = cron.split_whitespace().collect();
    assert_eq!(
        f.len(),
        5,
        "cron_fields_to_launchd called on unvalidated input"
    );
    let parse = |s: &str| {
        if s == "*" {
            None
        } else {
            Some(s.parse::<u32>().expect("validate_cron failed to gate"))
        }
    };
    CronFields {
        minute: parse(f[0]),
        hour: parse(f[1]),
        day: parse(f[2]),
        month: parse(f[3]),
        weekday: parse(f[4]),
    }
}

/// Validate a one-shot timestamp in `YYYY-MM-DD HH:MM` form.
pub fn validate_at(at: &str) -> Result<(), SchedulerError> {
    let caps = at_re()
        .captures(at)
        .ok_or_else(|| SchedulerError::InvalidAt {
            at: at.to_string(),
            reason: format!("expected 'YYYY-MM-DD HH:MM', got \"{at}\""),
        })?;
    // caps[1] = year (currently unused — accepted as-is)
    let month: u32 = caps[2].parse().unwrap_or(0);
    let day: u32 = caps[3].parse().unwrap_or(0);
    let hour: u32 = caps[4].parse().unwrap_or(99);
    let minute: u32 = caps[5].parse().unwrap_or(99);
    if !(1..=12).contains(&month) {
        return Err(SchedulerError::InvalidAt {
            at: at.to_string(),
            reason: format!("month out of range: {month}"),
        });
    }
    if !(1..=31).contains(&day) {
        return Err(SchedulerError::InvalidAt {
            at: at.to_string(),
            reason: format!("day out of range: {day}"),
        });
    }
    if hour > 23 {
        return Err(SchedulerError::InvalidAt {
            at: at.to_string(),
            reason: format!("hour out of range: {hour}"),
        });
    }
    if minute > 59 {
        return Err(SchedulerError::InvalidAt {
            at: at.to_string(),
            reason: format!("minute out of range: {minute}"),
        });
    }
    Ok(())
}

/// Launchd one-shot `StartCalendarInterval` fields. All five required —
/// launchd treats omitted fields as wildcards which would cause repeated
/// firing on a one-shot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtFields {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

/// Convert a validated one-shot timestamp to [`AtFields`].
///
/// Panics if `at` did not pass [`validate_at`].
pub fn at_to_launchd(at: &str) -> AtFields {
    let caps = at_re()
        .captures(at)
        .unwrap_or_else(|| panic!("at_to_launchd called with unvalidated input: {at}"));
    AtFields {
        year: caps[1].parse().unwrap(),
        month: caps[2].parse().unwrap(),
        day: caps[3].parse().unwrap(),
        hour: caps[4].parse().unwrap(),
        minute: caps[5].parse().unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_cron_accepts_daily_9am() {
        assert!(validate_cron("0 9 * * *").is_ok());
    }

    #[test]
    fn validate_cron_rejects_step_syntax() {
        let e = validate_cron("*/5 * * * *").unwrap_err();
        assert!(matches!(e, SchedulerError::InvalidCron { .. }));
    }

    #[test]
    fn validate_cron_rejects_range_syntax() {
        assert!(validate_cron("0 9 * * 1-5").is_err());
    }

    #[test]
    fn validate_cron_rejects_list_syntax() {
        assert!(validate_cron("0 9 * * 1,3,5").is_err());
    }

    #[test]
    fn validate_cron_rejects_wrong_field_count() {
        let e = validate_cron("0 9 * *").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("expected 5 fields"), "msg was: {msg}");
    }

    #[test]
    fn validate_cron_rejects_invalid_characters() {
        let e = validate_cron("0 9 * * abc").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("invalid field syntax"), "msg was: {msg}");
    }

    #[test]
    fn cron_fields_to_launchd_daily_9am() {
        let f = cron_fields_to_launchd("0 9 * * *");
        assert_eq!(f.minute, Some(0));
        assert_eq!(f.hour, Some(9));
        assert!(f.day.is_none());
        assert!(f.month.is_none());
        assert!(f.weekday.is_none());
    }

    #[test]
    fn cron_fields_to_launchd_sunday_noon() {
        let f = cron_fields_to_launchd("0 12 * * 0");
        assert_eq!(f.minute, Some(0));
        assert_eq!(f.hour, Some(12));
        assert_eq!(f.weekday, Some(0));
    }

    #[test]
    fn validate_at_accepts_valid_timestamp() {
        assert!(validate_at("2026-05-13 14:30").is_ok());
    }

    #[test]
    fn validate_at_rejects_bad_format() {
        let e = validate_at("2026/05/13 14:30").unwrap_err();
        assert!(e.to_string().contains("expected"));
    }

    #[test]
    fn validate_at_rejects_month_out_of_range() {
        let e = validate_at("2026-13-01 09:00").unwrap_err();
        assert!(e.to_string().contains("month out of range"));
    }

    #[test]
    fn validate_at_rejects_day_zero() {
        assert!(validate_at("2026-05-00 09:00").is_err());
    }

    #[test]
    fn validate_at_rejects_day_32() {
        assert!(validate_at("2026-05-32 09:00").is_err());
    }

    #[test]
    fn validate_at_rejects_hour_24() {
        let e = validate_at("2026-05-13 24:00").unwrap_err();
        assert!(e.to_string().contains("hour out of range"));
    }

    #[test]
    fn validate_at_rejects_minute_60() {
        assert!(validate_at("2026-05-13 09:60").is_err());
    }

    #[test]
    fn at_to_launchd_2026_05_13_1430() {
        let f = at_to_launchd("2026-05-13 14:30");
        assert_eq!(
            f,
            AtFields {
                year: 2026,
                month: 5,
                day: 13,
                hour: 14,
                minute: 30
            }
        );
    }
}
