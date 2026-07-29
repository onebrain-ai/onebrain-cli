//! Windows Task Scheduler — cron → trigger translation (pure).
//!
//! Every claim in this module is backed by a measured corpus case in
//! `tests/scheduler-corpus/windows/` that CI runs against a real Task
//! Scheduler on every PR. Where a doc comment cites a filename, that file is
//! the evidence.
//!
//! Two review rounds put blockers into this design by *reading* the schema;
//! the corpus exists so that stops being possible. The rendering itself is
//! Task 7b; this module only decides **which** trigger shape a cron maps to
//! and **how** its times of day are expressed.

use crate::scheduler::cron_parse::cron_fields_expanded;
use crate::scheduler::error::SchedulerError;
use crate::scheduler::types::ScheduleEntry;

/// Which `CalendarTrigger` subtree a cron maps to. The schema offers four
/// mutually-exclusive shapes — this is a choice, not a field mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerShape {
    /// `ScheduleByDay`, `DaysInterval=1`.
    Daily,
    /// `ScheduleByWeek` + `DaysOfWeek`.
    ///
    /// CANNOT carry `<Months>` — measured: *"The task XML contains an
    /// unexpected node. (11,11):Months:"* (`reject-weekly-with-months.xml`).
    Weekly { days: Vec<u32> },
    /// `ScheduleByMonth` + `DaysOfMonth` (+ `Months` when restricted).
    ///
    /// `months` empty = every month. The RENDERER may then omit `<Months>`
    /// (omission measurably means "Every month" — `accept-monthly-with-months`
    /// vs its omitted sibling) — but when `months` is non-empty it MUST be
    /// emitted, or `0 9 1 3 *` silently fires 12×/year.
    Monthly { days: Vec<u32>, months: Vec<u32> },
    /// `ScheduleByMonthDayOfWeek` — the only shape carrying weekday AND month
    /// (`accept-monthly-dayofweek.xml`). `weeks` must list every week: cron
    /// says "every Friday in March", not "the second Friday". Emitting
    /// `<Week>1</Week>` alone is a silent 5× under-fire that no other layer
    /// catches, which is why the field exists at all.
    MonthlyByWeekday {
        weeks: Vec<Week>,
        days_of_week: Vec<u32>,
        months: Vec<u32>,
    },
}

/// Task Scheduler's `<Weeks>` takes 1–4 plus a distinct `Last` token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Week {
    N(u8),
    Last,
}

/// Every week of the month — how cron's "every Friday" is said in a shape
/// whose native unit is "the Nth Friday".
pub fn all_weeks() -> Vec<Week> {
    vec![Week::N(1), Week::N(2), Week::N(3), Week::N(4), Week::Last]
}

/// How the times of day are expressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerTiming {
    /// One `CalendarTrigger` per `(hour, minute)`, sorted and deduped.
    Explicit(Vec<(u32, u32)>),
    /// One `CalendarTrigger` per start, each carrying its own `<Repetition>`.
    ///
    /// Measured (`accept-multi-repetition.xml`): repeating triggers coexist
    /// in one task. An earlier draft had a single `start` here, which forced
    /// `0,5,10 * * * *` to 72 explicit triggers — over the limit — and an
    /// exception to a locked design decision was built on that number. The
    /// 72 was a property of THIS TYPE, not of Windows; as a Vec it is three
    /// triggers.
    Repeating {
        starts: Vec<(u32, u32)>,
        interval_minutes: u32,
        duration_minutes: u32,
    },
}

/// MEASURED, and pinned from both sides: 48 triggers are accepted and 49 is
/// rejected with *"The task XML contains too many nodes of the same type."*
/// (`accept-triggers-048.xml` · `reject-triggers-049.xml`).
///
/// This is a **backstop in this backend, not validation in `validate_cron`**:
/// with `<Repetition>` collapsing regular intervals and multi-start covering
/// irregular minute lists, no plausible cron reaches it — and a uniform cap
/// would have hard-errored crons macOS runs today (`0,5,10 * * * *`).
pub const MAX_TRIGGERS: usize = 48;

/// Which `CalendarTrigger` subtree this entry's cron needs.
///
/// Assumes a cron that already passed `validate_cron` (like every other
/// consumer of the expander). The DOM+DOW arm is defensive: validation
/// rejects that combination first, so reaching it here means a caller
/// bypassed validation.
pub fn trigger_shape(entry: &ScheduleEntry) -> Result<TriggerShape, SchedulerError> {
    let cron = entry.cron.as_deref().unwrap_or_default();
    let set = cron_fields_expanded(cron);
    let day_restricted = !set.day.is_empty();
    let weekday_restricted = !set.weekday.is_empty();

    match (day_restricted, weekday_restricted) {
        (true, true) => Err(SchedulerError::InvalidCron {
            cron: cron.to_string(),
            reason: "restricting both day-of-month and day-of-week is not supported; \
                     use separate `schedule:` entries (one day-restricted, one \
                     weekday-restricted)"
                .to_string(),
        }),
        (true, false) => Ok(TriggerShape::Monthly {
            days: set.day,
            months: set.month,
        }),
        (false, true) if !set.month.is_empty() => Ok(TriggerShape::MonthlyByWeekday {
            weeks: all_weeks(),
            days_of_week: set.weekday,
            months: set.month,
        }),
        (false, true) => Ok(TriggerShape::Weekly { days: set.weekday }),
        (false, false) => Ok(TriggerShape::Daily),
    }
}

/// How this entry's cron expresses its times of day.
pub fn trigger_timing(entry: &ScheduleEntry) -> Result<TriggerTiming, SchedulerError> {
    let cron = entry.cron.as_deref().unwrap_or_default();
    let set = cron_fields_expanded(cron);
    let timing = plan_timing(&set.minute, &set.hour);

    let trigger_count = match &timing {
        TriggerTiming::Explicit(times) => times.len(),
        TriggerTiming::Repeating { starts, .. } => starts.len(),
    };
    if trigger_count > MAX_TRIGGERS {
        return Err(SchedulerError::TooManyTriggers {
            cron: cron.to_string(),
            needed: trigger_count,
        });
    }
    Ok(timing)
}

/// `Some(step)` when `values` is a regular cycle covering its whole period:
/// equal gaps, and `gap × len == period`. `{0,15,30,45}` over 60 → `Some(15)`;
/// `{0,5,10}` → `None` (the wrap-around gap is 50 — this is exactly why
/// `0,5,10 * * * *` is multi-start, not single-`<Repetition>`).
fn regular_step(values: &[u32], period: u32) -> Option<u32> {
    match values {
        [] | [_] => None,
        [first, rest @ ..] => {
            let gap = rest[0] - first;
            if gap == 0 || !period.is_multiple_of(gap) || (period / gap) as usize != values.len() {
                return None;
            }
            let mut prev = *first;
            for &v in rest {
                if v - prev != gap {
                    return None;
                }
                prev = v;
            }
            Some(gap)
        }
    }
}

fn plan_timing(minutes: &[u32], hours: &[u32]) -> TriggerTiming {
    let hour_wild = hours.is_empty();

    // Wildcard minute → per-minute repetition; the only expressible form,
    // since no ScheduleBy* subtree has a minute wildcard
    // (`accept-repetition-every-minute.xml`).
    if minutes.is_empty() {
        return if hour_wild {
            TriggerTiming::Repeating {
                starts: vec![(0, 0)],
                interval_minutes: 1,
                duration_minutes: 24 * 60,
            }
        } else {
            TriggerTiming::Repeating {
                starts: hours.iter().map(|&h| (h, 0)).collect(),
                interval_minutes: 1,
                duration_minutes: 60,
            }
        };
    }

    // Regular minute cycle (`*/5`, `0,30`) → one start per hour-context,
    // repeating through its window. An all-day `P1D` duration is measured
    // acceptable (`accept-multi-repetition.xml` carries it).
    if let Some(step) = regular_step(minutes, 60) {
        let first = minutes[0];
        return if hour_wild {
            TriggerTiming::Repeating {
                starts: vec![(0, first)],
                interval_minutes: step,
                duration_minutes: 24 * 60,
            }
        } else {
            TriggerTiming::Repeating {
                starts: hours.iter().map(|&h| (h, first)).collect(),
                interval_minutes: step,
                duration_minutes: 60,
            }
        };
    }

    // Irregular minutes + wildcard hour → one hourly-repeating trigger per
    // minute (`0,5,10 * * * *` = THREE triggers, `accept-multi-repetition`).
    if hour_wild {
        return TriggerTiming::Repeating {
            starts: minutes.iter().map(|&m| (0, m)).collect(),
            interval_minutes: 60,
            duration_minutes: 24 * 60,
        };
    }

    // Single minute value across restricted hours: a regular hour cycle
    // collapses (`0 */2 * * *` → one trigger repeating every 120 min);
    // irregular hours enumerate (`0 9,17` → two).
    if minutes.len() == 1 {
        if let Some(hstep) = regular_step(hours, 24) {
            return TriggerTiming::Repeating {
                starts: vec![(hours[0], minutes[0])],
                interval_minutes: hstep * 60,
                duration_minutes: 24 * 60,
            };
        }
    }

    // Fully enumerated cross-product, sorted and deduped.
    let mut times: Vec<(u32, u32)> = hours
        .iter()
        .flat_map(|&h| minutes.iter().map(move |&m| (h, m)))
        .collect();
    times.sort_unstable();
    times.dedup();
    TriggerTiming::Explicit(times)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::test_support::entry_cron;

    // ── shape ────────────────────────────────────────────────────────────

    #[test]
    fn daily_cron_maps_to_scheduleby_day() {
        assert_eq!(
            trigger_shape(&entry_cron("0 9 * * *")).unwrap(),
            TriggerShape::Daily
        );
    }

    #[test]
    fn weekday_restricted_maps_to_scheduleby_week() {
        assert_eq!(
            trigger_shape(&entry_cron("0 17 * * 5")).unwrap(),
            TriggerShape::Weekly { days: vec![5] }
        );
    }

    #[test]
    fn month_restriction_is_carried_never_dropped() {
        // Omitting <Months> is ACCEPTED by Windows and silently means "every
        // month" — `0 9 1 3 *` would fire 12x/year. Corpus:
        // accept-monthly-with-months.expect pins the resolved date to March.
        assert_eq!(
            trigger_shape(&entry_cron("0 9 1 3 *")).unwrap(),
            TriggerShape::Monthly {
                days: vec![1],
                months: vec![3],
            }
        );
    }

    #[test]
    fn month_wildcard_monthly_carries_an_empty_month_set() {
        // `0 9 1 * *` — 1st of EVERY month. Here omission and enumeration
        // mean the same thing; empty is the wildcard marker.
        assert_eq!(
            trigger_shape(&entry_cron("0 9 1 * *")).unwrap(),
            TriggerShape::Monthly {
                days: vec![1],
                months: vec![],
            }
        );
    }

    #[test]
    fn month_plus_weekday_uses_the_fourth_subtree_and_is_not_rejected() {
        // ScheduleByWeek refuses <Months> (measured), which made this look
        // unrepresentable — both early review rounds concluded a rejection
        // rule was needed. ScheduleByMonthDayOfWeek handles it
        // (accept-monthly-dayofweek.xml); rejecting would have removed
        // capability launchd users have today.
        assert_eq!(
            trigger_shape(&entry_cron("0 17 * 3 5")).unwrap(),
            TriggerShape::MonthlyByWeekday {
                weeks: all_weeks(),
                days_of_week: vec![5],
                months: vec![3],
            }
        );
    }

    #[test]
    fn day_of_month_plus_weekday_is_still_rejected_defensively() {
        // validate_cron rejects this before an entry normally exists; the
        // fixture builds the entry WITHOUT validating, so this asserts the
        // defensive arm a bypassing caller would hit.
        assert!(trigger_shape(&entry_cron("0 9 1,15 * 1,5")).is_err());
    }

    // ── timing ───────────────────────────────────────────────────────────

    #[test]
    fn a_regular_hour_interval_collapses_to_one_repeating_trigger() {
        assert_eq!(
            trigger_timing(&entry_cron("0 */2 * * *")).unwrap(),
            TriggerTiming::Repeating {
                starts: vec![(0, 0)],
                interval_minutes: 120,
                duration_minutes: 1440,
            }
        );
    }

    #[test]
    fn a_wildcard_minute_repeats_rather_than_expanding_to_sixty() {
        // `* 9 * * *` has no ScheduleBy* form at all
        // (accept-repetition-every-minute.xml).
        assert_eq!(
            trigger_timing(&entry_cron("* 9 * * *")).unwrap(),
            TriggerTiming::Repeating {
                starts: vec![(9, 0)],
                interval_minutes: 1,
                duration_minutes: 60,
            }
        );
    }

    #[test]
    fn a_stepped_minute_over_a_wildcard_hour_is_one_all_day_trigger() {
        // `*/5 * * * *` as one-per-time would be 288 triggers — over the
        // measured limit. As a repetition it is ONE (P1D duration measured
        // acceptable in accept-multi-repetition.xml).
        assert_eq!(
            trigger_timing(&entry_cron("*/5 * * * *")).unwrap(),
            TriggerTiming::Repeating {
                starts: vec![(0, 0)],
                interval_minutes: 5,
                duration_minutes: 1440,
            }
        );
    }

    #[test]
    fn an_irregular_minute_list_becomes_one_repeating_trigger_per_minute() {
        // THE case that dissolved the uniform-cap exception: gaps 5,5,50 are
        // not a regular interval, but three hourly-repeating triggers coexist
        // (accept-multi-repetition.xml) — 3 triggers, not 72.
        assert_eq!(
            trigger_timing(&entry_cron("0,5,10 * * * *")).unwrap(),
            TriggerTiming::Repeating {
                starts: vec![(0, 0), (0, 5), (0, 10)],
                interval_minutes: 60,
                duration_minutes: 1440,
            }
        );
    }

    #[test]
    fn an_irregular_hour_list_becomes_one_trigger_each() {
        assert_eq!(
            trigger_timing(&entry_cron("0 9,17 * * *")).unwrap(),
            TriggerTiming::Explicit(vec![(9, 0), (17, 0)])
        );
    }

    #[test]
    fn an_explicit_product_over_the_limit_is_refused_with_a_useful_message() {
        // 7 irregular minutes × 8 irregular hours = 56 explicit triggers.
        // Both lists are gap-irregular so no repetition path applies.
        let cron = "1,2,4,8,16,32,59 0,1,3,7,9,13,19,23 * * *";
        let err = trigger_timing(&entry_cron(cron)).unwrap_err().to_string();
        assert!(err.contains("48"), "must name the MEASURED limit: {err}");
        assert!(
            err.contains("schedule:"),
            "must suggest splitting entries: {err}"
        );
    }

    #[test]
    fn the_cap_counts_repeating_starts_too() {
        // 49 irregular minutes under a wildcard hour → 49 repeating triggers,
        // one over the measured limit. The cap is on TRIGGERS, not on which
        // enum variant produced them.
        let minutes: Vec<String> = (0..49)
            .map(|i| if i < 48 { i * 5 / 4 } else { 59 }) // irregular by construction
            .scan(std::collections::BTreeSet::new(), |seen, m| {
                Some(if seen.insert(m) { Some(m) } else { None })
            })
            .flatten()
            .map(|m| m.to_string())
            .collect();
        // Fall back to a plain irregular 49-list if dedup shrank it.
        let list = if minutes.len() == 49 {
            minutes.join(",")
        } else {
            let mut v: Vec<u32> = (0..48).collect();
            v.push(59); // gap 11 at the end → irregular
            v.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
        };
        let cron = format!("{list} * * * *");
        let entry = entry_cron(&cron);
        assert!(
            trigger_timing(&entry).is_err(),
            "49 starts must exceed the cap"
        );
    }
}
