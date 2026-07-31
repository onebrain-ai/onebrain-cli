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

use crate::scheduler::context::SchedulerContext;
use crate::scheduler::cron_parse::{at_fields, cron_fields_expanded};
use crate::scheduler::entry::{is_command_mode, is_one_shot};
use crate::scheduler::error::SchedulerError;
use crate::scheduler::launchd::should_append_vault;
use crate::scheduler::types::Args;
use crate::scheduler::types::ScheduleEntry;
use crate::scheduler::xml::escape as xml_escape;

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

// ─── rendering ───────────────────────────────────────────────────────────────

/// Task Scheduler task name, namespaced so `/doctor` and cleanup can
/// enumerate ours with one `\OneBrain\` query.
pub fn task_name(label_safe: &str) -> String {
    format!("\\OneBrain\\{label_safe}")
}

/// Cron weekday number → `<DaysOfWeek>` child element name.
///
/// Cron is `0 = Sunday … 6 = Saturday` (7 normalised to 0 by validation);
/// the schema wants empty named elements. An off-by-one here is a silent
/// wrong-day-forever bug, and nothing else in the chain catches it — the
/// hand-written corpus case hardcodes `<Friday/>`, so it pins the schema,
/// not this mapping. The exhaustive test below is the guard.
pub fn weekday_element(cron_weekday: u32) -> &'static str {
    match cron_weekday % 7 {
        0 => "Sunday",
        1 => "Monday",
        2 => "Tuesday",
        3 => "Wednesday",
        4 => "Thursday",
        5 => "Friday",
        _ => "Saturday",
    }
}

/// Cron month number (1–12) → `<Months>` child element name.
pub fn month_element(cron_month: u32) -> &'static str {
    match cron_month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        _ => "December",
    }
}

/// ISO-8601 duration for a repetition interval/window, in the exact forms the
/// corpus measured: `PT1M` (spike D), `PT1H` and `P1D`
/// (`accept-multi-repetition.xml`). Other minute counts use the same `PT<n>M`
/// lexical family.
fn iso_minutes(mins: u32) -> String {
    match mins {
        60 => "PT1H".to_string(),
        1440 => "P1D".to_string(),
        m => format!("PT{m}M"),
    }
}

fn schedule_by_block(shape: &TriggerShape) -> String {
    match shape {
        TriggerShape::Daily => {
            "      <ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>\n".to_string()
        }
        TriggerShape::Weekly { days } => {
            let days: String = days
                .iter()
                .map(|&d| format!("<{0} />", weekday_element(d)))
                .collect();
            format!(
                "      <ScheduleByWeek>\n        <WeeksInterval>1</WeeksInterval>\n        <DaysOfWeek>{days}</DaysOfWeek>\n      </ScheduleByWeek>\n"
            )
        }
        TriggerShape::Monthly { days, months } => {
            let days: String = days.iter().map(|d| format!("<Day>{d}</Day>")).collect();
            // months empty = every month. Omission measurably means exactly
            // that ("Every month" — the accept-monthly pair), so omitting is
            // both correct and the smaller document. When restricted, <Months>
            // is MANDATORY or 0 9 1 3 * fires 12x/year.
            let months_block = if months.is_empty() {
                String::new()
            } else {
                let m: String = months
                    .iter()
                    .map(|&m| format!("<{0} />", month_element(m)))
                    .collect();
                format!("        <Months>{m}</Months>\n")
            };
            format!(
                "      <ScheduleByMonth>\n        <DaysOfMonth>{days}</DaysOfMonth>\n{months_block}      </ScheduleByMonth>\n"
            )
        }
        TriggerShape::MonthlyByWeekday {
            weeks,
            days_of_week,
            months,
        } => {
            let weeks: String = weeks
                .iter()
                .map(|w| match w {
                    Week::N(n) => format!("<Week>{n}</Week>"),
                    Week::Last => "<Week>Last</Week>".to_string(),
                })
                .collect();
            let dows: String = days_of_week
                .iter()
                .map(|&d| format!("<{0} />", weekday_element(d)))
                .collect();
            let m: String = months
                .iter()
                .map(|&m| format!("<{0} />", month_element(m)))
                .collect();
            format!(
                "      <ScheduleByMonthDayOfWeek>\n        <Weeks>{weeks}</Weeks>\n        <DaysOfWeek>{dows}</DaysOfWeek>\n        <Months>{m}</Months>\n      </ScheduleByMonthDayOfWeek>\n"
            )
        }
    }
}

/// The argv every backend agrees on. Skill mode mirrors the launchd emitter's
/// `onebrain skill run --vault … --skill … --harness … [--arg k=v]`; command
/// mode is `entry.command` + its args, with `--vault` appended only when
/// `should_append_vault` says so — the same #263 R2-guarded helper launchd
/// uses, shared rather than re-derived.
/// Join argv into the single `<Arguments>` string Task Scheduler passes to
/// `CreateProcess` — quoting per element, EXCEPT for a `cmd.exe /c` payload.
///
/// Two different parsers live behind this one string, and picking the wrong
/// one produces a task that registers, fires, and does nothing:
/// - a normal program re-splits with `CommandLineToArgvW` → per-arg quoting
///   is required (without it `--vault C:\My Vault\ob` arrives as three
///   arguments — the v3.4.21 cold review's M2)
/// - `cmd.exe` parses its own `/c` (or `/k`) payload with **CMD** rules,
///   which do not recognise `\"`. Quoting that payload turns a working
///   command into a literal-backslash mess. Measured on the ARM64 VM: a
///   quoted `/c copy … "C:\ob test\out.txt"` registered and then silently
///   did nothing, while the same entry worked pre-v3.4.21.
///
/// So: everything up to and including the switch is quoted normally; the
/// payload after it is passed through verbatim, which is exactly what cmd
/// expects to receive.
fn join_win_args(command: &str, rest: &[String]) -> String {
    // Split on BOTH separators by hand: this renderer is pure and its tests
    // run on any host, where `std::path::Path` would treat a Windows `\` as
    // an ordinary character and hand back the whole string as the basename.
    let basename = command.rsplit(['\\', '/']).next().unwrap_or(command);
    // Only bare `cmd` or `cmd.exe`. Matching on the STEM alone also swept in
    // `cmd.com`, `cmd.bat`, `cmd.anything` — programs that do NOT parse their
    // tail with CMD rules, and which would therefore silently lose per-arg
    // quoting. The carve-out's whole justification is "cmd.exe parses its own
    // /c payload", so it should reach exactly that (v3.4.21 cold injection
    // review).
    let is_cmd = basename.eq_ignore_ascii_case("cmd") || basename.eq_ignore_ascii_case("cmd.exe");
    let switch_at = rest
        .iter()
        .position(|a| a.eq_ignore_ascii_case("/c") || a.eq_ignore_ascii_case("/k"));

    match (is_cmd, switch_at) {
        (true, Some(i)) => {
            let mut parts: Vec<String> = rest[..=i].iter().map(|a| quote_win_arg(a)).collect();
            // Verbatim payload — CMD's parser owns everything past the switch.
            parts.extend(rest[i + 1..].iter().cloned());
            parts.join(" ")
        }
        _ => rest
            .iter()
            .map(|a| quote_win_arg(a))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Quote one argv element per the `CommandLineToArgvW` rules every Windows
/// process uses to re-split `<Arguments>`.
///
/// Task Scheduler hands `<Arguments>` to `CreateProcess` as ONE string; the
/// child then splits it. Joining with a bare space (what this file did
/// before v3.4.21) meant `--vault C:\My Vault\ob` arrived as three
/// arguments — the task registered, fired, and ran wrong, which no
/// XML-contents assertion could catch (v3.4.21 cold review, M2).
///
/// The backslash rule is the fiddly part of the Microsoft algorithm:
/// backslashes are literal EXCEPT when they immediately precede a `"`, where
/// each must be doubled — so `C:\dir\` inside quotes becomes `C:\dir\\"`.
/// XML escaping runs afterwards and is a separate layer.
fn quote_win_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // Double the run that precedes the quote, then escape it.
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('\\');
                out.push('"');
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // A trailing run would otherwise escape our own closing quote.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

fn argv_for_entry(entry: &ScheduleEntry, ctx: &SchedulerContext) -> Vec<String> {
    if is_command_mode(entry) {
        let mut argv = vec![entry.command.clone().unwrap_or_default()];
        if let Some(Args::List(list)) = &entry.args {
            argv.extend(list.iter().cloned());
        }
        if should_append_vault(entry, ctx) {
            argv.push("--vault".to_string());
            argv.push(ctx.vault_path.to_string_lossy().into_owned());
        }
        argv
    } else {
        let mut argv = vec![
            ctx.skill_cli_path.clone(),
            "skill".to_string(),
            "run".to_string(),
            "--vault".to_string(),
            ctx.vault_path.to_string_lossy().into_owned(),
            "--skill".to_string(),
            entry.skill.clone().unwrap_or_default(),
            "--harness".to_string(),
            entry.effective_harness().as_str().to_string(),
        ];
        if let Some(Args::Map(map)) = &entry.args {
            for (k, v) in map {
                argv.push("--arg".to_string());
                argv.push(format!("{k}={v}"));
            }
        }
        argv
    }
}

/// One-shot boundaries: `StartBoundary` at the `at:` datetime, `EndBoundary`
/// 60 s later. All three self-deletion requirements are measured facts:
/// a trigger with no `EndBoundary` NEVER expires (so the task is never
/// deleted), `DeleteExpiredTaskAfter` lives under `<Settings>`, and its
/// clock runs from the EndBoundary — observed disappearance ~180 s after
/// registration with this exact shape (spike section E). The short window's
/// cost is real and documented: a one-shot missed while the machine sleeps
/// expires unfired.
fn one_shot_boundaries(at: &str) -> (String, String) {
    let f = at_fields(at);
    let start = chrono::NaiveDate::from_ymd_opt(f.year as i32, f.month, f.day)
        .and_then(|d| d.and_hms_opt(f.hour, f.minute, 0))
        .expect("validated at: fields form a real datetime");
    let end = start + chrono::Duration::seconds(60);
    (
        start.format("%Y-%m-%dT%H:%M:%S").to_string(),
        end.format("%Y-%m-%dT%H:%M:%S").to_string(),
    )
}

/// Emit the full Task Scheduler document.
///
/// Structure, element ORDER, and the `__USER__` placeholder are copied from
/// the corpus cases the schema workflow round-trips against a real Task
/// Scheduler (`tests/scheduler-corpus/windows/`) — order was measured
/// accepted there, so this renderer does not improvise its own. `__USER__`
/// is substituted with the real account by the installer (7c) and by the
/// workflow; keeping it out of the renderer keeps this function pure.
///
/// Deliberately absent, per measurement: any environment element
/// (`ExecAction` admits only Command/Arguments/WorkingDirectory), any shell
/// wrapper, any password (`LogonType=InteractiveToken`, verified to fire on
/// hosted runners).
pub fn generate_task_xml(
    entry: &ScheduleEntry,
    ctx: &SchedulerContext,
) -> Result<String, SchedulerError> {
    // Refuse what XML cannot carry BEFORE rendering anything — a control
    // character has no escape (`&#1;` is itself illegal), so the alternative is
    // a document `schtasks /Create /XML` rejects with an opaque error naming
    // nothing (#355).
    crate::scheduler::entry::reject_control_chars_in_entry(entry)?;

    let mut triggers = String::new();
    let mut expired_settings = String::new();

    if is_one_shot(entry) {
        let (start, end) = one_shot_boundaries(entry.at.as_deref().unwrap_or_default());
        triggers.push_str(&format!(
            "    <TimeTrigger>\n      <StartBoundary>{start}</StartBoundary>\n      <EndBoundary>{end}</EndBoundary>\n      <Enabled>true</Enabled>\n    </TimeTrigger>\n"
        ));
        expired_settings =
            "    <DeleteExpiredTaskAfter>PT1M</DeleteExpiredTaskAfter>\n".to_string();
    } else {
        // The shared cross-platform validator caps combinations at 1000 (the
        // launchd array shape) but the 48-trigger Task Scheduler cap is
        // enforced only here — a cron between 49 and 1000 triggers passes
        // validation everywhere and must surface at render time as the
        // friendly TooManyTriggers error, not a panic (2026-07-29 full-epic
        // audit, BLOCKER 2).
        let shape = trigger_shape(entry)?;
        let timing = trigger_timing(entry)?;
        let by_block = schedule_by_block(&shape);
        // Fixed epoch date: a recurring trigger's start DATE is semantically
        // irrelevant, and a clock here would make every snapshot
        // nondeterministic.
        let render_one = |h: u32, m: u32, repetition: &str| {
            format!(
                "    <CalendarTrigger>\n      <StartBoundary>2000-01-01T{h:02}:{m:02}:00</StartBoundary>\n      <Enabled>true</Enabled>\n{repetition}{by_block}    </CalendarTrigger>\n"
            )
        };
        match timing {
            TriggerTiming::Explicit(times) => {
                for (h, m) in times {
                    triggers.push_str(&render_one(h, m, ""));
                }
            }
            TriggerTiming::Repeating {
                starts,
                interval_minutes,
                duration_minutes,
            } => {
                let rep = format!(
                    "      <Repetition><Interval>{}</Interval><Duration>{}</Duration></Repetition>\n",
                    iso_minutes(interval_minutes),
                    iso_minutes(duration_minutes)
                );
                for (h, m) in starts {
                    triggers.push_str(&render_one(h, m, &rep));
                }
            }
        }
    }

    let arguments = argv_for_entry(entry, ctx)
        .split_first()
        .map(|(cmd, rest)| (cmd.clone(), join_win_args(cmd, rest)))
        .map(|(cmd, args)| (xml_escape(&cmd), xml_escape(&args)))
        .expect("argv always has a command");

    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
         \x20 <RegistrationInfo><Description>OneBrain scheduled entry</Description></RegistrationInfo>\n\
         \x20 <Triggers>\n{triggers}  </Triggers>\n\
         \x20 <Principals>\n\
         \x20   <Principal id=\"Author\">\n\
         \x20     <UserId>__USER__</UserId>\n\
         \x20     <LogonType>InteractiveToken</LogonType>\n\
         \x20     <RunLevel>LeastPrivilege</RunLevel>\n\
         \x20   </Principal>\n\
         \x20 </Principals>\n\
         \x20 <Settings>\n\
         \x20   <Enabled>true</Enabled>\n\
         \x20   <AllowStartOnDemand>true</AllowStartOnDemand>\n\
         \x20   <StartWhenAvailable>true</StartWhenAvailable>\n\
         \x20   <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n{expired_settings}\
         \x20 </Settings>\n\
         \x20 <Actions Context=\"Author\">\n\
         \x20   <Exec><Command>{}</Command><Arguments>{}</Arguments></Exec>\n\
         \x20 </Actions>\n\
         </Task>\n",
        arguments.0, arguments.1
    ))
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

    /// M2 (v3.4.21 cold review) — LIVE bug in shipped v3.4.20: args were
    /// joined with a bare space, so a space-bearing path arrived at the child
    /// as several arguments. Task Scheduler passes `<Arguments>` to
    /// CreateProcess as ONE string and the child re-splits it with
    /// CommandLineToArgvW, so each element must be quoted per those rules —
    /// skill mode included, which is where `--vault "C:\My Vault\ob"` lives.
    #[test]
    fn skill_mode_vault_path_with_spaces_stays_one_argument() {
        let mut ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        ctx.vault_path = std::path::PathBuf::from(r"C:\My Vault\ob");
        let out = generate_task_xml(&daily_entry(), &ctx).unwrap();
        assert!(
            out.contains(r#"--vault &quot;C:\My Vault\ob&quot;"#),
            "{out}"
        );
    }

    /// Measured on the ARM64 VM (v3.4.21 Track A verify): quoting a
    /// `cmd.exe /c` payload made the task register, fire, and do NOTHING —
    /// CMD parses its own payload and does not recognise `\"`. The payload
    /// after the switch is therefore passed through verbatim, while
    /// everything else keeps CommandLineToArgvW quoting. This is also a
    /// REGRESSION GUARD: the pattern below is the one the v3.4.20 Windows
    /// audit used successfully, and per-arg quoting had broken it.
    #[test]
    fn cmd_slash_c_payload_is_passed_through_verbatim() {
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        let mut e = daily_command_entry();
        e.command = Some(r"C:\Windows\system32\cmd.exe".into());
        e.args = Some(Args::List(vec![
            "/c".into(),
            r#"copy C:\Windows\win.ini "C:\ob test\out.txt""#.into(),
        ]));
        let out = generate_task_xml(&e, &ctx).unwrap();
        assert!(
            out.contains(r#"/c copy C:\Windows\win.ini &quot;C:\ob test\out.txt&quot;"#),
            "cmd payload must not be re-quoted: {out}"
        );
    }

    /// A non-cmd binary keeps per-arg quoting — the M2 fix must not be
    /// undone by the cmd carve-out.
    #[test]
    fn non_cmd_binaries_still_get_per_arg_quoting() {
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        let mut e = daily_command_entry();
        e.command = Some(r"C:\tools\rsync.exe".into());
        e.args = Some(Args::List(vec!["-av".into(), r"C:\My Vault\ob".into()]));
        let out = generate_task_xml(&e, &ctx).unwrap();
        assert!(
            out.contains(r#"-av &quot;C:\My Vault\ob&quot;"#),
            "space-bearing path must stay one argument: {out}"
        );
    }

    /// The carve-out exists because `cmd.exe` parses its own `/c` tail with
    /// CMD rules. Matching on the STEM let `cmd.com` / `cmd.bat` / any
    /// `cmd.*` take the verbatim path too — programs that do no such thing,
    /// and which therefore silently lost per-arg quoting.
    /// (v3.4.21 cold injection review.)
    #[test]
    fn only_real_cmd_takes_the_verbatim_path() {
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        let render = |command: &str| {
            let mut e = daily_command_entry();
            e.command = Some(command.into());
            e.args = Some(Args::List(vec!["/c".into(), r"C:\My Vault\ob".into()]));
            generate_task_xml(&e, &ctx).unwrap()
        };
        for real in ["cmd", "cmd.exe", "CMD.EXE", r"C:\WINDOWS\system32\cmd.exe"] {
            assert!(
                render(real).contains(r#"/c C:\My Vault\ob"#),
                "{real}: CMD must receive its payload verbatim"
            );
        }
        for impostor in [r"C:\evil\cmd.com", r"C:\evil\cmd.bat", "notcmd.exe"] {
            assert!(
                render(impostor).contains(r#"&quot;C:\My Vault\ob&quot;"#),
                "{impostor}: does not parse CMD-style, so it must get per-arg quoting"
            );
        }
    }

    /// The CommandLineToArgvW backslash rule: a run of backslashes is
    /// literal unless it precedes a quote, where it must be doubled. A
    /// trailing run before our own closing quote counts.
    #[test]
    fn trailing_backslash_run_is_doubled_before_the_closing_quote() {
        // `C:\ob test\` → the trailing run doubles so it cannot escape our
        // own closing quote.
        assert_eq!(quote_win_arg("C:\\ob test\\"), r#""C:\ob test\\""#);
        // `a"b` → the embedded quote is escaped.
        assert_eq!(quote_win_arg("a\"b"), r#""a\"b""#);
        // No metacharacters: left bare, so nothing double-quotes a flag.
        assert_eq!(quote_win_arg("plain"), "plain");
        // Empty string still needs to occupy an argv slot.
        assert_eq!(quote_win_arg(""), r#""""#);
    }

    /// Audit BLOCKER 2 regression (v3.4.20): a cron between 49 and 1000
    /// triggers passes the shared cross-platform validator (1000-combination
    /// cap), so the 48-trigger Task Scheduler cap is only reachable at render
    /// time — which must surface as the friendly TooManyTriggers error, never
    /// a panic (the old `.expect` crashed register AND --dry-run on Windows).
    #[test]
    fn render_surfaces_too_many_triggers_instead_of_panicking() {
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        let entry =
            crate::scheduler::test_support::entry_cron("1,2,4,8,16,32,59 0,1,3,7,9,13,19,23 * * *");
        let err = generate_task_xml(&entry, &ctx).unwrap_err();
        assert!(
            matches!(err, SchedulerError::TooManyTriggers { .. }),
            "got {err:?}"
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

    // ── rendering ────────────────────────────────────────────────────────

    use crate::scheduler::test_support::{
        ctx_with_cli, daily_command_entry, daily_entry, foreign_command_entry, one_shot_entry,
    };

    #[test]
    fn task_name_is_namespaced_under_onebrain() {
        assert_eq!(task_name("daily"), r"\OneBrain\daily");
    }

    #[test]
    fn every_weekday_number_maps_to_the_right_element() {
        // MJ-2: nothing else catches an off-by-one here — the corpus case
        // hardcodes <Friday/>, pinning the schema, not this mapping.
        let names = [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ];
        for (n, want) in names.iter().enumerate() {
            assert_eq!(weekday_element(n as u32), *want, "cron weekday {n}");
        }
        assert_eq!(weekday_element(7), "Sunday", "cron 7 normalises to 0");
    }

    #[test]
    fn every_month_number_maps_to_the_right_element() {
        let names = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        for (i, want) in names.iter().enumerate() {
            assert_eq!(month_element(i as u32 + 1), *want, "cron month {}", i + 1);
        }
    }

    #[test]
    fn recurring_entry_has_a_start_boundary_and_no_password() {
        let ctx = ctx_with_cli(r"C:\Users\u\scoop\shims\onebrain.exe");
        let out = generate_task_xml(&daily_entry(), &ctx).unwrap();
        assert!(out.contains("<CalendarTrigger>"), "{out}");
        assert!(
            out.contains("<StartBoundary>2000-01-01T09:00:00</StartBoundary>"),
            "fixed epoch date, time from the cron: {out}"
        );
        assert!(
            out.contains("<LogonType>InteractiveToken</LogonType>"),
            "{out}"
        );
        assert!(
            !out.to_lowercase().contains("password"),
            "must never emit a password: {out}"
        );
    }

    #[test]
    fn the_renderer_emits_no_environment_element() {
        // ExecAction admits only Command/Arguments/WorkingDirectory; emitting
        // an environment element makes schtasks reject the document while
        // every pure test still passes (round 2, B4).
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        assert!(!generate_task_xml(&daily_entry(), &ctx)
            .unwrap()
            .contains("<Environment"));
    }

    #[test]
    fn command_mode_renders_the_entry_command_not_the_cli() {
        // Round 4 M6: an earlier element table hardcoded <Command> to the CLI
        // path — a `command: rsync` entry would have run onebrain with rsync's
        // arguments.
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        let out = generate_task_xml(&foreign_command_entry(), &ctx).unwrap();
        assert!(out.contains("<Command>rsync</Command>"), "{out}");
        assert!(
            !out.contains("--vault"),
            "#263 R2: a non-onebrain command must not get --vault appended: {out}"
        );
    }

    #[test]
    fn command_mode_onebrain_gets_the_vault_appended() {
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        let out = generate_task_xml(&daily_command_entry(), &ctx).unwrap();
        assert!(out.contains("<Command>onebrain</Command>"), "{out}");
        assert!(
            out.contains("search reindex --vault"),
            "shared should_append_vault must fire for an onebrain command: {out}"
        );
    }

    #[test]
    fn one_shot_can_actually_expire() {
        // All three requirements measured (spike E): EndBoundary or the
        // trigger never expires; DeleteExpiredTaskAfter under <Settings>;
        // clock runs from the EndBoundary.
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        let out = generate_task_xml(&one_shot_entry(), &ctx).unwrap();
        assert!(out.contains("<TimeTrigger>"), "{out}");
        assert!(
            out.contains("<StartBoundary>2026-08-01T09:00:00</StartBoundary>"),
            "{out}"
        );
        assert!(
            out.contains("<EndBoundary>2026-08-01T09:01:00</EndBoundary>"),
            "{out}"
        );
        let settings = out.split("<Settings>").nth(1).expect("no Settings block");
        let settings = settings.split("</Settings>").next().unwrap();
        assert!(
            settings.contains("<DeleteExpiredTaskAfter>"),
            "DeleteExpiredTaskAfter must sit inside Settings, not the trigger: {out}"
        );
        assert!(
            !out.contains("/bin/sh"),
            "no shell wrapper on Windows: {out}"
        );
    }

    #[test]
    fn month_weekday_renders_all_five_weeks() {
        // MJ-1 / round 4 BL-4: <Week>1</Week> alone is a silent 5x under-fire
        // that the shape test, the hand-written corpus, and the month-only
        // .expect are all blind to. This is the layer that pins it.
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        let out = generate_task_xml(
            &crate::scheduler::test_support::entry_cron("0 17 * 3 5"),
            &ctx,
        )
        .unwrap();
        assert!(
            out.contains(
                "<Weeks><Week>1</Week><Week>2</Week><Week>3</Week><Week>4</Week><Week>Last</Week></Weeks>"
            ),
            "{out}"
        );
        assert!(out.contains("<DaysOfWeek><Friday /></DaysOfWeek>"), "{out}");
        assert!(out.contains("<Months><March /></Months>"), "{out}");
    }

    #[test]
    fn an_irregular_minute_list_renders_one_repeating_trigger_per_start() {
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        let out = generate_task_xml(
            &crate::scheduler::test_support::entry_cron("0,5,10 * * * *"),
            &ctx,
        )
        .unwrap();
        assert_eq!(out.matches("<CalendarTrigger>").count(), 3, "{out}");
        assert_eq!(
            out.matches(
                "<Repetition><Interval>PT1H</Interval><Duration>P1D</Duration></Repetition>"
            )
            .count(),
            3,
            "each start carries its own repetition (accept-multi-repetition): {out}"
        );
    }

    #[test]
    fn snapshot_recurring_skill() {
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        insta::assert_snapshot!(generate_task_xml(&daily_entry(), &ctx).unwrap());
    }

    /// Writes this renderer's REAL output into the corpus, where the schema
    /// workflow round-trips it through an actual Task Scheduler on every PR.
    ///
    /// Hand-written corpus cases prove what the schema accepts; these prove
    /// what THIS CODE emits is inside that. A renderer that dropped <Months>
    /// would pass every unit test and snapshot above — and fail
    /// accept-generated-monthly's .expect when the resolved date leaves
    /// March. Regenerate with:
    ///   cargo test -p onebrain-core emit_generated_corpus -- --ignored
    #[test]
    #[ignore = "writes files into tests/scheduler-corpus — run explicitly after renderer changes"]
    fn emit_generated_corpus() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/scheduler-corpus/windows");
        assert!(dir.is_dir(), "corpus dir missing: {}", dir.display());
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        for (name, cron) in [
            ("daily", "0 9 * * *"),
            ("weekly", "0 17 * * 5"),
            ("monthly", "0 9 1 3 *"),
            ("monthly-weekday", "0 17 * 3 5"),
            ("repeating", "*/5 * * * *"),
            ("multi-repetition", "0,5,10 * * * *"),
        ] {
            let entry = crate::scheduler::test_support::entry_cron(cron);
            std::fs::write(
                dir.join(format!("accept-generated-{name}.xml")),
                generate_task_xml(&entry, &ctx).unwrap(),
            )
            .unwrap();
        }
        // command mode — the quarter of the matrix no hand-written case covers
        std::fs::write(
            dir.join("accept-generated-command.xml"),
            generate_task_xml(&daily_command_entry(), &ctx).unwrap(),
        )
        .unwrap();
        // #353: no fixture carried an escaped argument, so `quote_win_arg`
        // could regress without `schtasks /Create` ever noticing. Its
        // `.expect` pins the arguments as Task Scheduler reports them BACK,
        // which is the half a well-formed document cannot prove on its own.
        std::fs::write(
            dir.join("accept-generated-escaping.xml"),
            generate_task_xml(
                &{
                    let mut e = daily_command_entry();
                    e.command = Some(r"C:\Windows\System32\cmd.exe".to_string());
                    e.args = Some(crate::scheduler::types::Args::List(
                        crate::scheduler::test_support::escaping_args(),
                    ));
                    e
                },
                &ctx,
            )
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn snapshot_is_stable_across_runs() {
        // Guards against a clock creeping into StartBoundary.
        let ctx = ctx_with_cli(r"C:\bin\onebrain.exe");
        assert_eq!(
            generate_task_xml(&daily_entry(), &ctx).unwrap(),
            generate_task_xml(&daily_entry(), &ctx).unwrap()
        );
    }
}
