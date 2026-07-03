//! Cron + at-string validation and conversion to launchd
//! `StartCalendarInterval` fields. Mirrors Bun
//! `src/lib/scheduler/cron-parse.ts` line-for-line for the single-value
//! case; step/list/range support (#116 bug 1) is a Rust-port extension —
//! Bun rejected them outright, but launchd's `StartCalendarInterval`
//! ORs across an *array* of dicts, so we expand each field to its
//! concrete value set and let [`crate::scheduler::launchd`] emit one dict
//! per combination.
//!
//! Supported per-field syntax: `*` (wildcard), a single integer, a `*/N`
//! step, an `a-b` inclusive range, and an `a,b,c` list. These compose —
//! e.g. `1-5/2` is accepted (nice-to-have combination) — but the common
//! cases named in #116 are steps and lists.
//!
//! Weekday also accepts the standard cron `0`-`7` range (both `0` and `7`
//! mean Sunday); `7` is normalized to `0` during expansion so launchd's
//! `Weekday` key never receives an out-of-range value and `0,7` dedupes
//! to a single combination.
//!
//! Day-of-month and day-of-week are mutually exclusive: `validate_cron`
//! rejects a cron string that restricts BOTH (neither is `*`), because
//! standard cron ORs the two fields but our launchd `<dict>` emitter ANDs
//! same-dict keys — see `validate_cron`'s day/weekday check for the full
//! rationale.

use crate::scheduler::error::SchedulerError;
use regex::Regex;
use std::sync::OnceLock;

/// One cron field's valid value range (inclusive), used both for `*`
/// expansion and for `*/N` step expansion.
#[derive(Debug, Clone, Copy)]
struct FieldRange {
    min: u32,
    max: u32,
}

const MINUTE_RANGE: FieldRange = FieldRange { min: 0, max: 59 };
const HOUR_RANGE: FieldRange = FieldRange { min: 0, max: 23 };
const DAY_RANGE: FieldRange = FieldRange { min: 1, max: 31 };
const MONTH_RANGE: FieldRange = FieldRange { min: 1, max: 12 };
// Standard/Vixie cron accepts weekday 0-7, where BOTH 0 and 7 mean Sunday
// (launchd's own `Weekday` key only understands 0-6, so a bare 7 is
// normalized to 0 during expansion — see `expand_item`'s post-processing
// in `expand_field`/`expand_field_or_wildcard`).
const WEEKDAY_RANGE: FieldRange = FieldRange { min: 0, max: 7 };

fn cron_field_re() -> &'static Regex {
    // A single cron field: one comma-separated list of items, where each
    // item is `*`, `*/N`, an integer, `a-b`, or `a-b/N`. This is a
    // syntax-shape check only — range bounds and step-nonzero are
    // validated separately so we can emit a specific reason string.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(\*(/\d+)?|\d+(-\d+(/\d+)?)?)(,(\*(/\d+)?|\d+(-\d+(/\d+)?)?))*$").unwrap()
    })
}

fn at_re() -> &'static Regex {
    // YYYY-MM-DD HH:MM (literal space, two digits each except year).
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2})$").unwrap())
}

/// Validate a 5-field cron string. Returns `Err(SchedulerError::InvalidCron)`
/// with a reason string ("expected 5 fields, got N", "invalid field syntax:
/// \"foo\"", a semantic reason like "step must be nonzero" / "range start
/// must not exceed end", or the Cartesian-product cap message from
/// [`CronFieldSet::combinations`]).
///
/// Checking the combination cap here (rather than only at plist-emission
/// time) means `cron_fields_to_launchd_expanded` + `.combinations()` can
/// never panic/error downstream on an already-validated cron string —
/// [`crate::scheduler::launchd::calendar_block`] relies on this.
pub fn validate_cron(cron: &str) -> Result<(), SchedulerError> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(SchedulerError::InvalidCron {
            cron: cron.to_string(),
            reason: format!("expected 5 fields, got {}", fields.len()),
        });
    }
    let ranges = [
        MINUTE_RANGE,
        HOUR_RANGE,
        DAY_RANGE,
        MONTH_RANGE,
        WEEKDAY_RANGE,
    ];
    let mut set = CronFieldSet::default();
    for (i, (f, range)) in fields.iter().zip(ranges.iter()).enumerate() {
        if !cron_field_re().is_match(f) {
            return Err(SchedulerError::InvalidCron {
                cron: cron.to_string(),
                reason: format!("invalid field syntax: \"{f}\""),
            });
        }
        // Syntax matched — now check semantic validity (step nonzero,
        // range order, values in bounds) via the same expansion path the
        // launchd emitter uses, surfacing its error as the reason string.
        let mut expanded =
            expand_field_or_wildcard(f, *range).map_err(|reason| SchedulerError::InvalidCron {
                cron: cron.to_string(),
                reason,
            })?;
        if i == 4 {
            normalize_weekday_seven(&mut expanded);
        }
        match i {
            0 => set.minute = expanded,
            1 => set.hour = expanded,
            2 => set.day = expanded,
            3 => set.month = expanded,
            _ => set.weekday = expanded,
        }
    }
    // Standard cron ORs day-of-month and day-of-week when BOTH are
    // restricted (neither is `*`) — e.g. `0 9 1,15 * 1,5` fires on the
    // 1st/15th of the month OR every Mon/Fri. Our launchd emitter instead
    // ANDs the two keys within one `<dict>` (launchd has no native OR-across-
    // keys construct), so a both-restricted cron would silently fire far
    // less often than the user's cron string implies. Reject rather than
    // silently emit the wrong (AND) semantics — the #116 collision fix in
    // this same release makes splitting into two separate `schedule:`
    // entries (one day-restricted, one weekday-restricted) a viable
    // workaround, since same-binary/skill entries on different schedules no
    // longer collide on one plist path.
    if fields[2] != "*" && fields[4] != "*" {
        return Err(SchedulerError::InvalidCron {
            cron: cron.to_string(),
            reason: "restricting both day-of-month and day-of-week is not supported by the \
                     launchd backend; use separate `schedule:` entries (which no longer \
                     collide as of this release)"
                .to_string(),
        });
    }
    if let Err(reason) = set.combinations() {
        return Err(SchedulerError::InvalidCron {
            cron: cron.to_string(),
            reason,
        });
    }
    Ok(())
}

/// Expand a single validated field-syntax string to its concrete sorted,
/// deduped value set within `range`. A bare `*` enumerates the FULL range
/// (e.g. minute → `0..=59`) — callers that want the cheaper "no
/// constraint" wildcard sentinel (empty vec) instead of a fully-enumerated
/// range should use [`expand_field_or_wildcard`].
fn expand_field(field: &str, range: FieldRange) -> Result<Vec<u32>, String> {
    let mut values: Vec<u32> = Vec::new();
    for item in field.split(',') {
        values.extend(expand_item(item, range)?);
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

/// Standard/Vixie cron accepts weekday `0`-`7`, where both `0` and `7` mean
/// Sunday — but launchd's `Weekday` key only understands `0`-`6`. Map any
/// `7` in the expanded value set down to `0`, then re-sort + dedup so
/// `0,7` (both meaning Sunday) collapses to a single `[0]` rather than
/// emitting two combinations (or, worse, an out-of-range `Weekday` key).
fn normalize_weekday_seven(values: &mut Vec<u32>) {
    for v in values.iter_mut() {
        if *v == 7 {
            *v = 0;
        }
    }
    values.sort_unstable();
    values.dedup();
}

/// Same as [`expand_field`], but a bare `*` collapses to the empty-vec
/// wildcard sentinel ("no constraint", matching launchd's own "field
/// absent = every value" semantics) instead of the fully-enumerated
/// range. This keeps the common no-op case cheap and matches
/// [`CronFieldSet`]'s convention that empty = wildcard, not "all values".
///
/// Used by both [`validate_cron`] (to surface semantic errors AND build
/// the `CronFieldSet` needed to check the combination cap) and
/// [`cron_fields_to_launchd_expanded`] (same expansion, post-validation).
fn expand_field_or_wildcard(field: &str, range: FieldRange) -> Result<Vec<u32>, String> {
    if field == "*" {
        return Ok(Vec::new());
    }
    expand_field(field, range)
}

/// Expand one comma-item (`*`, `*/N`, `a`, `a-b`, or `a-b/N`) to its
/// concrete values within `range`.
fn expand_item(item: &str, range: FieldRange) -> Result<Vec<u32>, String> {
    // Split off an optional `/step` suffix.
    let (base, step) = match item.split_once('/') {
        Some((b, s)) => {
            let step: u32 = s
                .parse()
                .map_err(|_| format!("invalid step syntax: \"{item}\""))?;
            if step == 0 {
                return Err(format!("step must be nonzero: \"{item}\""));
            }
            (b, Some(step))
        }
        None => (item, None),
    };

    let (lo, hi) = if base == "*" {
        (range.min, range.max)
    } else if let Some((a, b)) = base.split_once('-') {
        let a: u32 = a
            .parse()
            .map_err(|_| format!("invalid range syntax: \"{item}\""))?;
        let b: u32 = b
            .parse()
            .map_err(|_| format!("invalid range syntax: \"{item}\""))?;
        if a > b {
            return Err(format!("range start must not exceed end: \"{item}\""));
        }
        (a, b)
    } else {
        let v: u32 = base
            .parse()
            .map_err(|_| format!("invalid field syntax: \"{item}\""))?;
        (v, v)
    };

    if lo < range.min || hi > range.max {
        return Err(format!(
            "value out of range ({}-{}): \"{item}\"",
            range.min, range.max
        ));
    }

    let step = step.unwrap_or(1);
    Ok((lo..=hi).step_by(step as usize).collect())
}

/// Launchd `StartCalendarInterval` fields for a recurring (cron) entry —
/// single-value form. `None` for a field means launchd matches every
/// value (wildcard). Retained for the common single-combination case
/// (used directly by [`crate::scheduler::launchd`] when a cron string
/// expands to exactly one value per field, so the existing single-`<dict>`
/// plist shape and its byte-parity snapshot are untouched).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CronFields {
    pub minute: Option<u32>,
    pub hour: Option<u32>,
    pub day: Option<u32>,
    pub month: Option<u32>,
    pub weekday: Option<u32>,
}

/// Expanded launchd `StartCalendarInterval` field set — one concrete
/// value set per field (empty = wildcard, matches every value). This is
/// the general form; [`cron_fields_to_launchd`] collapses it to
/// [`CronFields`] when every field has 0 or 1 values (the common case),
/// and [`cron_fields_to_launchd_expanded`] exposes the full expansion for
/// the array-form plist emitter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CronFieldSet {
    pub minute: Vec<u32>,
    pub hour: Vec<u32>,
    pub day: Vec<u32>,
    pub month: Vec<u32>,
    pub weekday: Vec<u32>,
}

impl CronFieldSet {
    /// Cartesian product cap — a plain `* * * * *` would otherwise expand
    /// to 60×24×31×12×7 combinations (wildcards aren't enumerated, so this
    /// never actually happens, but the cap must still admit legitimate
    /// multi-field expressions). Any single multi-valued field is fine
    /// (e.g. `*/2` on hour → 12 combinations); the product only grows large
    /// when MULTIPLE fields are simultaneously multi-valued.
    ///
    /// 1000 accepts the benign "every day of every month" idiom
    /// `0 0 1-31 1-12 *` (31×12 = 372 combinations) while still rejecting
    /// the pathological `*/1 */1 * * *` (1440 combinations) — a clear
    /// error rather than silently writing a huge plist.
    const MAX_COMBINATIONS: usize = 1000;

    /// True when every field has at most one concrete value — the plist
    /// emitter can use the existing single-`<dict>` form.
    fn is_single_combination(&self) -> bool {
        [
            &self.minute,
            &self.hour,
            &self.day,
            &self.month,
            &self.weekday,
        ]
        .iter()
        .all(|v| v.len() <= 1)
    }

    /// Collapse to the single-value [`CronFields`] form. Panics if any
    /// field has more than one value — callers must check
    /// [`Self::is_single_combination`] first.
    fn to_single(&self) -> CronFields {
        let one = |v: &[u32]| v.first().copied();
        CronFields {
            minute: one(&self.minute),
            hour: one(&self.hour),
            day: one(&self.day),
            month: one(&self.month),
            weekday: one(&self.weekday),
        }
    }

    /// Expand to the full Cartesian product of per-field combinations,
    /// each represented as a [`CronFields`] (one dict per combination in
    /// the array-form plist). Returns an error string when the product
    /// would exceed [`Self::MAX_COMBINATIONS`].
    pub fn combinations(&self) -> Result<Vec<CronFields>, String> {
        // `None` sentinel per field = wildcard (matches launchd's own
        // "field absent = every value" semantics — we do NOT enumerate
        // wildcards, only genuine multi-value fields).
        let opts = |v: &[u32]| -> Vec<Option<u32>> {
            if v.is_empty() {
                vec![None]
            } else {
                v.iter().copied().map(Some).collect()
            }
        };
        let minutes = opts(&self.minute);
        let hours = opts(&self.hour);
        let days = opts(&self.day);
        let months = opts(&self.month);
        let weekdays = opts(&self.weekday);

        let total = minutes
            .len()
            .saturating_mul(hours.len())
            .saturating_mul(days.len())
            .saturating_mul(months.len())
            .saturating_mul(weekdays.len());
        if total > Self::MAX_COMBINATIONS {
            return Err(format!(
                "cron expression expands to {total} StartCalendarInterval combinations, \
                 exceeding the cap of {}. Narrow the step/list/range so fewer fields are \
                 simultaneously multi-valued.",
                Self::MAX_COMBINATIONS
            ));
        }

        let mut out = Vec::with_capacity(total);
        for &mo in &minutes {
            for &h in &hours {
                for &d in &days {
                    for &m in &months {
                        for &w in &weekdays {
                            out.push(CronFields {
                                minute: mo,
                                hour: h,
                                day: d,
                                month: m,
                                weekday: w,
                            });
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Convert a validated 5-field cron string to launchd `CronFields`
/// (single-combination form).
///
/// Panics if `cron` did not pass [`validate_cron`], OR if the expression
/// expands to more than one combination (multi-value fields) — callers
/// that might receive step/list/range cron strings should use
/// [`cron_fields_to_launchd_expanded`] instead and check
/// [`CronFieldSet::is_single_combination`] (or just always call the
/// expanded form and branch on combination count, which is what
/// [`crate::scheduler::launchd::calendar_block`] does).
pub fn cron_fields_to_launchd(cron: &str) -> CronFields {
    let set = cron_fields_to_launchd_expanded(cron);
    assert!(
        set.is_single_combination(),
        "cron_fields_to_launchd called on a multi-combination cron string \
         (use cron_fields_to_launchd_expanded instead): {cron}"
    );
    set.to_single()
}

/// Convert a validated 5-field cron string to the full expanded
/// [`CronFieldSet`] (one concrete value list per field; empty = wildcard).
///
/// Panics if `cron` did not pass [`validate_cron`] — the caller is
/// responsible for ordering. Bun's contract is identical (docstring
/// "Assumes `cron` already passed `validateCron`.").
pub fn cron_fields_to_launchd_expanded(cron: &str) -> CronFieldSet {
    let f: Vec<&str> = cron.split_whitespace().collect();
    assert_eq!(
        f.len(),
        5,
        "cron_fields_to_launchd_expanded called on unvalidated input"
    );
    let expand = |s: &str, range: FieldRange| -> Vec<u32> {
        expand_field_or_wildcard(s, range).expect("validate_cron failed to gate")
    };
    let mut weekday = expand(f[4], WEEKDAY_RANGE);
    normalize_weekday_seven(&mut weekday);
    CronFieldSet {
        minute: expand(f[0], MINUTE_RANGE),
        hour: expand(f[1], HOUR_RANGE),
        day: expand(f[2], DAY_RANGE),
        month: expand(f[3], MONTH_RANGE),
        weekday,
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

    // ── #116 bug 1: step / list / range now ACCEPTED ──────────────────────────

    #[test]
    fn validate_cron_accepts_step_syntax() {
        assert!(validate_cron("*/5 * * * *").is_ok());
    }

    #[test]
    fn validate_cron_accepts_range_syntax() {
        assert!(validate_cron("0 9 * * 1-5").is_ok());
    }

    #[test]
    fn validate_cron_accepts_list_syntax() {
        assert!(validate_cron("0 9 * * 1,3,5").is_ok());
    }

    #[test]
    fn validate_cron_accepts_range_with_step() {
        // Nice-to-have combination named in #116: `1-5/2`.
        assert!(validate_cron("0 9 1-5/2 * *").is_ok());
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
    fn validate_cron_rejects_empty_list_item() {
        // "1,,3" — an empty item between commas.
        let e = validate_cron("0 9 * * 1,,3").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("invalid field syntax"), "msg was: {msg}");
    }

    #[test]
    fn validate_cron_rejects_zero_step() {
        let e = validate_cron("*/0 * * * *").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("step must be nonzero"), "msg was: {msg}");
    }

    #[test]
    fn validate_cron_rejects_reversed_range() {
        let e = validate_cron("0 9 * * 5-2").unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("range start must not exceed end"),
            "msg was: {msg}"
        );
    }

    #[test]
    fn validate_cron_rejects_out_of_range_value() {
        // Hour field max is 23.
        let e = validate_cron("0 24 * * *").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("out of range"), "msg was: {msg}");
    }

    #[test]
    fn validate_cron_rejects_non_numeric_field() {
        let e = validate_cron("0 9 * * a").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("invalid field syntax"), "msg was: {msg}");
    }

    #[test]
    fn validate_cron_rejects_pathological_combination_explosion() {
        // Two simultaneously multi-valued fields (60 × 24 = 1440) exceed
        // the combination cap — validate_cron must reject it up front
        // rather than letting the plist emitter choke on it later.
        let e = validate_cron("*/1 */1 * * *").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("exceeding the cap"), "msg was: {msg}");
    }

    // ── weekday=7 regression (standard/Vixie cron: 0 AND 7 both mean Sunday) ──

    #[test]
    fn validate_cron_accepts_weekday_seven_as_sunday() {
        assert!(validate_cron("0 9 * * 7").is_ok());
    }

    #[test]
    fn expanded_weekday_seven_normalizes_to_zero() {
        let set = cron_fields_to_launchd_expanded("0 9 * * 7");
        assert_eq!(set.weekday, vec![0]);
    }

    #[test]
    fn expanded_weekday_zero_and_seven_dedupe_to_single_zero() {
        let set = cron_fields_to_launchd_expanded("0 9 * * 0,7");
        assert_eq!(set.weekday, vec![0]);
    }

    #[test]
    fn validate_cron_rejects_weekday_eight() {
        let e = validate_cron("0 9 * * 8").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("out of range"), "msg was: {msg}");
    }

    #[test]
    fn cron_fields_to_launchd_weekday_seven_collapses_to_single_dict_zero() {
        // 0,7 dedupes to exactly one value → single-combination form.
        let f = cron_fields_to_launchd("0 9 * * 0,7");
        assert_eq!(f.weekday, Some(0));
    }

    // ── DOM/DOW both-restricted: rejected (launchd ANDs, cron ORs) ────────────

    #[test]
    fn validate_cron_rejects_both_day_and_weekday_restricted() {
        let e = validate_cron("0 9 1,15 * 1,5").unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("restricting both day-of-month and day-of-week"),
            "msg was: {msg}"
        );
    }

    #[test]
    fn validate_cron_rejects_both_restricted_single_values() {
        // Even single (non-list/range) values on both fields must be
        // rejected — the restriction is "field != wildcard", not "field is
        // a list/range".
        let e = validate_cron("0 9 1 * 1").unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("restricting both day-of-month and day-of-week"),
            "msg was: {msg}"
        );
    }

    #[test]
    fn validate_cron_accepts_day_only_restricted() {
        assert!(validate_cron("0 9 1,15 * *").is_ok());
    }

    #[test]
    fn validate_cron_accepts_weekday_only_restricted() {
        assert!(validate_cron("0 9 * * 1,5").is_ok());
    }

    // ── MAX_COMBINATIONS: cap raised from 366 to 1000 (#116 follow-up) ────────

    #[test]
    fn validate_cron_accepts_every_day_of_every_month() {
        // 31 days × 12 months = 372 combinations — benign, must be accepted
        // now that the cap is 1000 (was 366, which wrongly rejected this).
        assert!(validate_cron("0 0 1-31 1-12 *").is_ok());
    }

    #[test]
    fn validate_cron_still_rejects_pathological_1440() {
        // Cap raised to 1000 but 1440 must still be rejected.
        let e = validate_cron("*/1 */1 * * *").unwrap_err();
        assert!(e.to_string().contains("exceeding the cap"));
    }

    // ── cron_fields_to_launchd: single-combination form ───────────────────────

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

    // ── cron_fields_to_launchd_expanded: multi-value form ──────────────────────

    #[test]
    fn expanded_step_produces_every_nth_value() {
        let set = cron_fields_to_launchd_expanded("0 */2 * * *");
        assert_eq!(set.hour, vec![0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22]);
        assert_eq!(set.minute, vec![0]);
        assert!(set.day.is_empty());
    }

    #[test]
    fn expanded_list_produces_exact_values() {
        let set = cron_fields_to_launchd_expanded("0 9 * * 1,3,5");
        assert_eq!(set.weekday, vec![1, 3, 5]);
    }

    #[test]
    fn expanded_range_produces_inclusive_values() {
        let set = cron_fields_to_launchd_expanded("0 9 * * 1-5");
        assert_eq!(set.weekday, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn expanded_range_with_step() {
        let set = cron_fields_to_launchd_expanded("0 9 1-10/3 * *");
        assert_eq!(set.day, vec![1, 4, 7, 10]);
    }

    #[test]
    fn expanded_dedupes_overlapping_list_items() {
        let set = cron_fields_to_launchd_expanded("0 9 * * 1,1-3,2");
        assert_eq!(set.weekday, vec![1, 2, 3]);
    }

    #[test]
    fn expanded_wildcard_field_is_empty_vec() {
        let set = cron_fields_to_launchd_expanded("0 9 * * *");
        assert!(set.day.is_empty());
        assert!(set.month.is_empty());
        assert!(set.weekday.is_empty());
    }

    // ── CronFieldSet::combinations ─────────────────────────────────────────────

    #[test]
    fn combinations_single_value_fields_yields_one_combination() {
        let set = cron_fields_to_launchd_expanded("0 9 * * *");
        let combos = set.combinations().unwrap();
        assert_eq!(combos.len(), 1);
        assert_eq!(combos[0].minute, Some(0));
        assert_eq!(combos[0].hour, Some(9));
    }

    #[test]
    fn combinations_step_hour_yields_twelve_combinations() {
        let set = cron_fields_to_launchd_expanded("0 */2 * * *");
        let combos = set.combinations().unwrap();
        assert_eq!(combos.len(), 12);
        let hours: Vec<u32> = combos.iter().map(|c| c.hour.unwrap()).collect();
        assert_eq!(hours, vec![0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22]);
        // Every combination keeps the single-valued minute fixed.
        assert!(combos.iter().all(|c| c.minute == Some(0)));
    }

    #[test]
    fn combinations_list_weekday_yields_three_combinations() {
        let set = cron_fields_to_launchd_expanded("0 9 * * 1,3,5");
        let combos = set.combinations().unwrap();
        assert_eq!(combos.len(), 3);
    }

    #[test]
    fn combinations_two_multi_value_fields_multiply() {
        // hour has 2 values (*/12 → 0,12), weekday has 3 (1,3,5) → 6 combos.
        let set = cron_fields_to_launchd_expanded("0 */12 * * 1,3,5");
        let combos = set.combinations().unwrap();
        assert_eq!(combos.len(), 6);
    }

    #[test]
    fn combinations_rejects_pathological_expansion() {
        // Two simultaneously multi-valued fields (60 minute values ×
        // 24 hour values = 1440) far exceed the cap. A bare `* * * * *`
        // is NOT multi-valued (wildcard fields stay as the `None`
        // sentinel, never enumerated), so it must use an explicit
        // every-value step to force real enumeration here.
        let set = cron_fields_to_launchd_expanded("*/1 */1 * * *");
        let err = set.combinations().unwrap_err();
        assert!(err.contains("exceeding the cap"), "got: {err}");
    }

    // ── MAX_COMBINATIONS exact-boundary (guards `>` vs `>=` regressions) ──────

    #[test]
    fn combinations_exactly_at_cap_succeeds() {
        // 1000 minute values × 1 everything-else = exactly
        // CronFieldSet::MAX_COMBINATIONS (1000) — must succeed.
        let set = CronFieldSet {
            minute: (0..1000).collect(),
            hour: vec![],
            day: vec![],
            month: vec![],
            weekday: vec![],
        };
        let combos = set.combinations().unwrap();
        assert_eq!(combos.len(), 1000);
    }

    #[test]
    fn combinations_one_over_cap_rejected() {
        // 1001 > MAX_COMBINATIONS (1000) — must be rejected.
        let set = CronFieldSet {
            minute: (0..1001).collect(),
            hour: vec![],
            day: vec![],
            month: vec![],
            weekday: vec![],
        };
        let err = set.combinations().unwrap_err();
        assert!(err.contains("exceeding the cap"), "got: {err}");
    }

    #[test]
    fn is_single_combination_true_for_all_wildcards_and_singles() {
        let set = cron_fields_to_launchd_expanded("0 9 * * *");
        assert!(set.is_single_combination());
    }

    #[test]
    fn is_single_combination_false_when_any_field_multi_valued() {
        let set = cron_fields_to_launchd_expanded("0 */2 * * *");
        assert!(!set.is_single_combination());
    }

    #[test]
    #[should_panic(expected = "multi-combination")]
    fn cron_fields_to_launchd_panics_on_multi_combination_input() {
        let _ = cron_fields_to_launchd("0 */2 * * *");
    }

    // ── validate_at / at_to_launchd — unchanged behavior ───────────────────────

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
