//! Linux systemd user-timer rendering (pure).
//!
//! Shaped after `tests/scheduler-corpus/linux/`, which `systemd-analyze
//! verify` checks in CI on every PR — the one static systemd check hosted
//! runners can run (no user session, no D-Bus needed), which is why Linux has
//! a real CI gate here despite the fire-proof staying a manual VM run (see
//! the epic waiver).
//!
//! Deliberate absences, each a settled decision:
//! - no `Environment=PATH=` — the PATH-in-artifact track was dropped once
//!   `run_skill.rs` was found to already handle it (v3.4.4, #124);
//! - no `StandardOutput=append:` — file logging was dropped once it was
//!   established only launchd has a redirect primitive; Linux output goes to
//!   the JOURNAL, and "did it fail?" is the outcome check's job
//!   (onebrain#230);
//! - no `loginctl enable-linger` — logged-in-only on every platform, the
//!   no-stored-credential decision;
//! - `Persistent=false` — a fire missed while asleep is NOT caught up, where
//!   launchd catches up. A real cross-platform divergence, documented rather
//!   than discovered.

use crate::scheduler::context::SchedulerContext;
use crate::scheduler::cron_parse::{at_fields, cron_fields_expanded};
use crate::scheduler::entry::{is_command_mode, is_one_shot};
use crate::scheduler::error::SchedulerError;
use crate::scheduler::launchd::should_append_vault;
use crate::scheduler::types::{Args, ScheduleEntry};
use std::path::{Path, PathBuf};

/// `onebrain-<label>` — the stem shared by the `.service`/`.timer` pair.
pub fn unit_base_name(label_safe: &str) -> String {
    format!("onebrain-{label_safe}")
}

/// `~/.config/systemd/user` — the XDG user-unit convention.
pub fn unit_dir(homedir: &Path) -> PathBuf {
    homedir.join(".config/systemd/user")
}

/// systemd-quote one argv element for an `ExecStart=` line.
///
/// Four escapes, each for a distinct systemd behaviour — quoting alone is
/// NOT enough, because systemd expands inside double quotes:
/// - `\` and `"` — the quoting syntax itself
/// - `$` → `$$` — systemd expands `$VAR` / `${VAR}` in `ExecStart` even
///   within double quotes; an unescaped `$` silently becomes an empty
///   string (or a leaked environment value) at run time
/// - `%` → `%%` — unit **specifiers** (`%h`, `%i`, `%n`, …) expand the same
///   way; `date +%F` in an arg would otherwise be rewritten before exec
///
/// Two more characters do not need ESCAPING but do need QUOTING, because
/// systemd gives them meaning between words rather than inside one:
/// - `;` — a lone `;` separates command lines within a single `ExecStart=`,
///   so `args: [hi, ";", /bin/rm, -f, /tmp/x]` rendered TWO commands out of
///   what the config, `--dry-run` and `--status` all present as one
/// - `'` — systemd's word splitter is shell-like, so a stray single quote
///   opens a quoted region that swallows the following words
///
/// Wrapping either in double quotes makes it an ordinary character of its own
/// argument. Found by the v3.4.21 cold injection review; guarded by
/// `semicolon_and_single_quote_stay_inside_one_argument`.
///
/// A newline or carriage return cannot be escaped at all: unit files are
/// line-oriented, so a raw `\n` in a value ENDS the `ExecStart=` line and
/// whatever follows is parsed as a new directive. `sanitize_unit_value`
/// rejects those before we get here — the one case where refusing beats
/// escaping, because the format has no representation for it.
fn quote_arg(arg: &str) -> String {
    let escaped = arg
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "$$")
        .replace('%', "%%");
    if arg.is_empty()
        || arg.chars().any(char::is_whitespace)
        || arg.contains([';', '\''])
        || escaped != arg
    {
        format!("\"{escaped}\"")
    } else {
        arg.to_string()
    }
}

/// Reject values that a line-oriented unit file cannot carry at all.
///
/// A newline inside an `ExecStart=` value terminates the directive; the
/// remainder of the value is then read as a unit directive in its own right
/// (`x\nExecStartPost=/bin/sh -c '…'` injects a real hook). No escape exists
/// for it in the unit-file format, so the renderer refuses instead — the
/// single character class where refusal is the correct answer rather than
/// escaping. Applies to the argv elements that reach a unit line. The LABEL
/// needs no check — `sanitize_label` has already restricted it to
/// `[A-Za-z0-9-]`, so it cannot carry a line break; the call site says so
/// too, and this docstring used to claim the opposite.
pub fn sanitize_unit_value(value: &str) -> Result<(), SchedulerError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(SchedulerError::InvalidEntry {
            reason: format!(
                "value contains a newline, which a systemd unit file cannot carry: {value:?}"
            ),
        });
    }
    Ok(())
}

/// The same argv every backend agrees on (mirrors `schtasks::argv_for_entry`;
/// command mode shares launchd's `should_append_vault`, #263 R2 guard).
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

/// `OnCalendar=` expressions for the entry — one line per emitted expression,
/// ORed by systemd exactly like multiple `CalendarTrigger`s on Windows.
///
/// systemd calendar syntax: `DOW *-M-D HH:MM:SS`, `*` for any, lists joined
/// with `,`. Cron weekday 0/7=Sunday maps to systemd's `Sun`.
pub fn on_calendar_exprs(entry: &ScheduleEntry) -> Vec<String> {
    if is_one_shot(entry) {
        let f = at_fields(entry.at.as_deref().unwrap_or_default());
        return vec![format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:00",
            f.year, f.month, f.day, f.hour, f.minute
        )];
    }
    let set = cron_fields_expanded(entry.cron.as_deref().unwrap_or_default());

    const DOW: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let dow = if set.weekday.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = set.weekday.iter().map(|&d| DOW[(d % 7) as usize]).collect();
        format!("{} ", names.join(","))
    };
    let month = if set.month.is_empty() {
        "*".to_string()
    } else {
        set.month
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let day = if set.day.is_empty() {
        "*".to_string()
    } else {
        set.day
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    // Zero-padded to the exact form the corpus measured through
    // systemd-analyze (`09:00:00`) — no guessing about shorthand acceptance.
    let hour = if set.hour.is_empty() {
        "*".to_string()
    } else {
        set.hour
            .iter()
            .map(|h| format!("{h:02}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let minute = if set.minute.is_empty() {
        "*".to_string()
    } else {
        set.minute
            .iter()
            .map(|m| format!("{m:02}"))
            .collect::<Vec<_>>()
            .join(",")
    };

    vec![format!("{dow}*-{month}-{day} {hour}:{minute}:00")]
}

/// The `.service` unit.
pub fn generate_service_unit(
    entry: &ScheduleEntry,
    ctx: &SchedulerContext,
) -> Result<String, SchedulerError> {
    // The same refusal the other two renderers apply. `sanitize_unit_value`
    // below covers only what a LINE-oriented format cannot carry (newline/CR);
    // this covers what no scheduler format can carry honestly. Caught by a
    // Linux gate-8 run: without it, `\u{1}` registered cleanly here while
    // being refused on macOS and Windows (#355).
    crate::scheduler::entry::reject_control_chars_in_entry(entry)?;
    let label = crate::scheduler::launchd::label_for_entry(entry);
    let base = unit_base_name(&label);
    let argv = argv_for_entry(entry, ctx);
    // Refuse before rendering: a newline has no escape in this format.
    // (The label needs no check — `sanitize_label` restricts it to
    // `[A-Za-z0-9-]`.)
    for a in &argv {
        sanitize_unit_value(a)?;
    }
    let exec = argv
        .iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");

    let mut out = format!(
        "[Unit]\nDescription=OneBrain scheduled entry ({label})\n\n[Service]\nType=oneshot\nExecStart={exec}\n"
    );
    if is_one_shot(entry) {
        // Self-removal needs THREE operations, so it is /bin/sh -c (the
        // corpus documents an earlier single-command version that dropped
        // the symlink but LEFT THE TIMER FIRING — it lacked --now):
        //   1. disable --now   stop AND unlink
        //   2. rm both units   or systemd still knows them after reload
        //   3. daemon-reload   so the removal is observed
        // --no-block is mandatory: ExecStopPost runs while systemd is
        // stopping this unit; a blocking systemctl waits on the manager's
        // job queue while the manager waits for ExecStopPost — a deadlock.
        // NOTE: fires on EVERY stop, including a failed start. Documented
        // property, not a surprise.
        out.push_str(&format!(
            "ExecStopPost=/bin/sh -c 'systemctl --user --no-block disable --now {base}.timer; rm -f \"$HOME/.config/systemd/user/{base}.timer\" \"$HOME/.config/systemd/user/{base}.service\"; systemctl --user --no-block daemon-reload'\n"
        ));
    }
    Ok(out)
}

/// The `.timer` unit.
pub fn generate_timer_unit(entry: &ScheduleEntry, _ctx: &SchedulerContext) -> String {
    let label = crate::scheduler::launchd::label_for_entry(entry);
    let mut out =
        format!("[Unit]\nDescription=OneBrain scheduled entry timer ({label})\n\n[Timer]\n");
    for expr in on_calendar_exprs(entry) {
        out.push_str(&format!("OnCalendar={expr}\n"));
    }
    out.push_str("Persistent=false\n\n[Install]\nWantedBy=timers.target\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::test_support::{
        ctx_with_cli, daily_command_entry, daily_entry, entry_cron, foreign_command_entry,
        one_shot_entry,
    };

    #[test]
    fn unit_dir_follows_the_xdg_user_convention() {
        assert_eq!(
            unit_dir(Path::new("/home/u")),
            PathBuf::from("/home/u/.config/systemd/user")
        );
    }

    #[test]
    fn daily_cron_renders_a_daily_oncalendar() {
        assert_eq!(
            on_calendar_exprs(&entry_cron("0 9 * * *")),
            vec!["*-*-* 09:00:00".to_string()]
        );
    }

    #[test]
    fn weekday_maps_to_systemd_day_names() {
        // cron 0/7 = Sunday → Sun; an off-by-one is wrong-day-forever (MJ-2's
        // Linux sibling).
        assert_eq!(
            on_calendar_exprs(&entry_cron("0 17 * * 5")),
            vec!["Fri *-*-* 17:00:00".to_string()]
        );
        assert_eq!(
            on_calendar_exprs(&entry_cron("0 8 * * 0")),
            vec!["Sun *-*-* 08:00:00".to_string()]
        );
    }

    #[test]
    fn month_restriction_is_carried_never_dropped() {
        // The Linux sibling of the Windows <Months> bug.
        assert_eq!(
            on_calendar_exprs(&entry_cron("0 9 1 3 *")),
            vec!["*-3-1 09:00:00".to_string()]
        );
    }

    #[test]
    fn one_shot_renders_an_absolute_datetime() {
        assert_eq!(
            on_calendar_exprs(&one_shot_entry()),
            vec!["2026-08-01 09:00:00".to_string()]
        );
    }

    #[test]
    fn service_never_carries_dropped_designs_or_linger() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        let out = generate_service_unit(&daily_entry(), &ctx).unwrap();
        assert!(!out.contains("Environment=PATH"), "dropped design: {out}");
        assert!(!out.contains("StandardOutput"), "journal, not files: {out}");
        assert!(!out.contains("linger"), "logged-in-only: {out}");
    }

    #[test]
    fn one_shot_cleanup_has_all_three_operations_and_cannot_deadlock() {
        // The corpus documents an earlier version that ran only `disable`
        // (no --now): symlink dropped, TIMER KEPT FIRING. And a blocking
        // systemctl in ExecStopPost deadlocks against the manager [M9].
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        let out = generate_service_unit(&one_shot_entry(), &ctx).unwrap();
        let line = out
            .lines()
            .find(|l| l.starts_with("ExecStopPost="))
            .expect("one-shot must clean itself up");
        assert!(line.contains("--no-block"), "deadlock guard: {line}");
        assert!(
            line.contains("disable --now"),
            "without --now the timer keeps firing: {line}"
        );
        assert!(line.contains("rm -f"), "units must be removed: {line}");
        assert!(
            line.contains("daemon-reload"),
            "removal must be observed: {line}"
        );
    }

    #[test]
    fn recurring_service_has_no_stop_hook() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        assert!(!generate_service_unit(&daily_entry(), &ctx)
            .unwrap()
            .contains("ExecStopPost"));
    }

    #[test]
    fn timer_is_not_persistent_and_installs_into_timers_target() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        let out = generate_timer_unit(&daily_entry(), &ctx);
        assert!(out.contains("Persistent=false"), "{out}");
        assert!(out.contains("WantedBy=timers.target"), "{out}");
    }

    #[test]
    fn command_mode_execs_the_entry_command_with_vault_rules() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        let cmd = generate_service_unit(&daily_command_entry(), &ctx).unwrap();
        assert!(
            cmd.contains("ExecStart=onebrain search reindex --vault"),
            "{cmd}"
        );
        let foreign = generate_service_unit(&foreign_command_entry(), &ctx).unwrap();
        assert!(foreign.contains("ExecStart=rsync -av /a /b"), "{foreign}");
        assert!(!foreign.contains("--vault"), "#263 R2: {foreign}");
    }

    /// systemd rejects a relative ExecStart binary (measured, #330 — that
    /// exact case went red on main). The CLI resolves command binaries at
    /// register time; given that contract, both modes must yield an
    /// absolute ExecStart.
    #[test]
    fn execstart_is_absolute_for_resolved_entries() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        let resolved_command = {
            let mut e = daily_command_entry();
            e.command = Some("/usr/bin/rsync".to_string());
            e
        };
        for entry in [daily_entry(), one_shot_entry(), resolved_command] {
            let out = generate_service_unit(&entry, &ctx).unwrap();
            let exec = out
                .lines()
                .find(|l| l.starts_with("ExecStart="))
                .expect("service must have ExecStart");
            assert!(exec.starts_with("ExecStart=/"), "{exec}");
        }
    }

    /// M1 (v3.4.21 cold review) — LIVE bug in shipped v3.4.20: a newline in
    /// an arg was written verbatim into a line-oriented unit file, so the
    /// text after it parsed as a fresh directive. Recurring command-mode
    /// args had no register-time guard at all, so this was reachable from
    /// plain YAML. The renderer now refuses; there is no escape for it.
    #[test]
    fn newline_in_an_arg_is_refused_not_written() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        let mut e = daily_command_entry();
        e.command = Some("/bin/echo".into());
        e.args = Some(crate::scheduler::types::Args::List(vec![
            "x\nExecStartPost=/bin/sh -c 'touch /tmp/pwned'".to_string(),
        ]));
        let err = generate_service_unit(&e, &ctx).unwrap_err();
        assert!(
            matches!(err, SchedulerError::InvalidEntry { .. }),
            "got {err:?}"
        );
        // Which guard fired matters. Since #355 the shared control-character
        // refusal runs FIRST, so this case no longer reaches
        // `sanitize_unit_value` — asserting only `InvalidEntry` would let this
        // test keep passing while the systemd-specific newline ban rotted away
        // underneath it. Pin the wording, and cover the newline ban directly in
        // `the_newline_ban_still_stands_on_its_own` below.
        assert!(
            err.to_string().contains("control character"),
            "expected the shared control-char guard to fire first, got {err:?}"
        );
    }

    /// `sanitize_unit_value` still guards the values the shared entry check
    /// cannot see: argv derived from the CONTEXT (vault path, CLI path), not
    /// from the user's `onebrain.yml` entry. Without this, the newline ban has
    /// no test of its own.
    #[test]
    fn the_newline_ban_still_stands_on_its_own() {
        assert!(sanitize_unit_value("plain value").is_ok());
        assert!(sanitize_unit_value("tab\there is fine").is_ok());
        for bad in ["x\nExecStartPost=/bin/sh -c 'touch /tmp/pwned'", "x\ry"] {
            let err = sanitize_unit_value(bad).unwrap_err();
            assert!(err.to_string().contains("newline"), "{bad:?} gave {err:?}");
        }
    }

    /// systemd expands `$VAR` and `%SPECIFIER` INSIDE double quotes, so
    /// quoting alone silently rewrote these args at run time.
    #[test]
    fn dollar_and_percent_are_escaped_for_systemd_expansion() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        let mut e = daily_command_entry();
        e.command = Some("/bin/echo".into());
        e.args = Some(crate::scheduler::types::Args::List(vec![
            "$HOME/%h-report".to_string()
        ]));
        let out = generate_service_unit(&e, &ctx).unwrap();
        assert!(out.contains("$$HOME/%%h-report"), "{out}");
    }

    /// A lone `;` separates command lines inside one `ExecStart=`, so an
    /// unquoted one turned a single configured command into two. A stray `'`
    /// opens a quoted region in systemd's shell-like splitter and swallows
    /// the words after it. Neither needs escaping — both need QUOTING.
    /// (v3.4.21 cold injection review.)
    #[test]
    fn semicolon_and_single_quote_stay_inside_one_argument() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        let mut e = daily_command_entry();
        e.command = Some("/bin/echo".into());
        e.args = Some(crate::scheduler::types::Args::List(vec![
            "hi".to_string(),
            ";".to_string(),
            "/bin/rm".to_string(),
            "-f".to_string(),
            "/tmp/x".to_string(),
            "it's".to_string(),
        ]));
        let out = generate_service_unit(&e, &ctx).unwrap();
        let exec = out
            .lines()
            .find(|l| l.starts_with("ExecStart="))
            .expect("ExecStart line");
        assert!(
            exec.contains("\";\""),
            "a bare `;` would start a second command line:\n{exec}"
        );
        assert!(
            exec.contains("\"it's\""),
            "a bare `'` would open a quoted region:\n{exec}"
        );
        assert!(
            !exec.contains(" ; "),
            "no unquoted command separator may survive:\n{exec}"
        );
    }

    /// #344 on the Linux sink: a Windows-style path is data, and a
    /// space-bearing POSIX path must survive quoting intact.
    #[test]
    fn backslash_and_space_paths_survive_quoting() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        let mut e = daily_command_entry();
        e.command = Some("/bin/echo".into());
        e.args = Some(crate::scheduler::types::Args::List(vec![
            "/home/u/ob test/out.txt".to_string(),
            r"C:\ob test\out.txt".to_string(),
        ]));
        let out = generate_service_unit(&e, &ctx).unwrap();
        assert!(out.contains(r#""/home/u/ob test/out.txt""#), "{out}");
        assert!(out.contains(r#""C:\\ob test\\out.txt""#), "{out}");
    }

    #[test]
    fn args_with_spaces_are_quoted() {
        let mut ctx = ctx_with_cli("/usr/local/bin/onebrain");
        ctx.vault_path = PathBuf::from("/home/u/My Vault/ob-1");
        let out = generate_service_unit(&daily_entry(), &ctx).unwrap();
        assert!(
            out.contains("\"/home/u/My Vault/ob-1\""),
            "space-bearing paths must be quoted: {out}"
        );
    }

    /// Writes this renderer's REAL output into the corpus, where
    /// `systemd-analyze verify` checks it on every PR — same pattern as the
    /// Windows generated corpus. ExecStart must point at a binary that
    /// exists on the CHECKING machine (verify resolves it against the
    /// filesystem — measured), so the fixture uses /bin/echo with the argv
    /// shape preserved. Regenerate:
    ///   cargo test -p onebrain-core systemd::tests::emit_generated_corpus -- --ignored
    #[test]
    #[ignore = "writes files into tests/scheduler-corpus — run explicitly after renderer changes"]
    fn emit_generated_corpus() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/scheduler-corpus/linux");
        assert!(dir.is_dir(), "corpus dir missing: {}", dir.display());
        let ctx = crate::scheduler::test_support::ctx(
            "/home/u".as_ref(),
            "/bin/echo",
            "/home/u/logs".as_ref(),
        );
        for (name, entry) in [
            ("daily", daily_entry()),
            ("weekly", entry_cron("0 17 * * 5")),
            ("monthly", entry_cron("0 9 1 3 *")),
            ("one-shot", one_shot_entry()),
            // systemd requires an ABSOLUTE ExecStart binary — verify rejects
            // a bare name (measured: this exact case went red on main). The
            // real CLI guarantees absoluteness via resolve_command_binary at
            // register time, so the fixture models that contract, not the
            // raw yaml. Same binary as skill_cli_path → the path-equality arm
            // of command_is_onebrain keeps the --vault append covered.
            ("command", {
                let mut e = daily_command_entry();
                e.command = Some("/bin/echo".to_string());
                e
            }),
        ] {
            std::fs::write(
                dir.join(format!("accept-generated-{name}.service")),
                generate_service_unit(&entry, &ctx).unwrap(),
            )
            .unwrap();
            std::fs::write(
                dir.join(format!("accept-generated-{name}.timer")),
                generate_timer_unit(&entry, &ctx),
            )
            .unwrap();
        }
    }

    #[test]
    fn service_snapshot() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        insta::assert_snapshot!(generate_service_unit(&daily_entry(), &ctx).unwrap());
    }

    #[test]
    fn timer_snapshot() {
        let ctx = ctx_with_cli("/usr/local/bin/onebrain");
        insta::assert_snapshot!(generate_timer_unit(&daily_entry(), &ctx));
    }
}
