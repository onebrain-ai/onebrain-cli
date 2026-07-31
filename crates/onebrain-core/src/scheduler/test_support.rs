//! Shared fixtures for scheduler tests.
//!
//! `#![cfg(test)]` at module level: visible to unit tests in this crate only.
//! Integration-test crates (`crates/onebrain-cli/tests/*`, `tests/*.rs` in this
//! crate) CANNOT reach this module — they are separate crates. Build fixtures
//! locally there; do not fight the visibility.
#![cfg(test)]

use crate::scheduler::{ScheduleEntry, SchedulerContext};
use std::path::{Path, PathBuf};

/// One constructor with explicit knobs — callers override only what they care
/// about via the thin wrappers below.
pub fn ctx(home: &Path, cli: &str, log_base: &Path) -> SchedulerContext {
    SchedulerContext {
        vault_path: PathBuf::from("/v/ob-1"),
        skill_cli_path: cli.to_string(),
        log_base_path: log_base.to_path_buf(),
        homedir: home.to_path_buf(),
        uid: 501,
    }
}

/// Context rooted in `home` with defaults everywhere else.
pub fn ctx_in(home: &Path) -> SchedulerContext {
    let log_base = home.join("Library/Logs/onebrain");
    ctx(home, "/opt/homebrew/bin/onebrain", &log_base)
}

/// Context with a caller-chosen CLI path (Windows paths welcome).
pub fn ctx_with_cli(cli: &str) -> SchedulerContext {
    ctx(Path::new("/home/u"), cli, Path::new("/home/u/logs"))
}

/// `/daily` at 09:00 every day, skill mode, default harness.
pub fn daily_entry() -> ScheduleEntry {
    ScheduleEntry {
        cron: Some("0 9 * * *".to_string()),
        skill: Some("/daily".to_string()),
        ..Default::default()
    }
}

/// One-shot `at:` entry, skill mode.
///
/// NOTE the format: `at_re()` is `^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$` — a LITERAL
/// SPACE, not a `T` (`cron_parse.rs`). `at_fields()` panics on unvalidated
/// input, so a `T` here takes every one-shot test down with it.
pub fn one_shot_entry() -> ScheduleEntry {
    ScheduleEntry {
        at: Some("2026-08-01 09:00".to_string()),
        skill: Some("/daily".to_string()),
        ..Default::default()
    }
}

/// Skill-mode entry with a caller-chosen label. `ScheduleEntry` has no label
/// field — the label is DERIVED by `label_for_entry` from the skill name — so
/// this sets `skill: /<label>`.
pub fn entry_labelled(label: &str) -> ScheduleEntry {
    ScheduleEntry {
        cron: Some("0 9 * * *".to_string()),
        skill: Some(format!("/{label}")),
        ..Default::default()
    }
}

/// Recurring command-mode entry: `command: onebrain, args: [search, reindex]`.
pub fn daily_command_entry() -> ScheduleEntry {
    ScheduleEntry {
        cron: Some("0 3 * * 0".to_string()),
        command: Some("onebrain".to_string()),
        args: Some(crate::scheduler::Args::List(vec![
            "search".to_string(),
            "reindex".to_string(),
        ])),
        ..Default::default()
    }
}

/// One argument list covering every escaping branch both sinks own — and
/// "every" here is a coverage measurement, not a reading of the code.
///
/// An earlier version of this list claimed the same thing and was wrong:
/// `cargo llvm-cov --lib -p onebrain-core --show-missing-lines -- --include-ignored`
/// reported `quote_win_arg`'s doubling loop unexecuted, because every quote in
/// the list was preceded by a letter, and no unit test covered it either. Re-run
/// that command after touching this list; a rule with no argument reaching it is
/// exactly the hole #353 exists to close.
/// Kept identical in the systemd and schtasks regenerators so the two
/// fixtures are readable side by side: `$` and `%` are systemd expansions,
/// the trailing backslash run is the fiddly half of the Windows
/// `CommandLineToArgvW` algorithm, and the space, `;`, `'` and `"` are
/// word-splitting hazards on one or both.
///
/// Until v3.4.22 no fixture in the corpus carried ANY of these, so both
/// escapers could regress and `cargo test`, `systemd-analyze verify` and
/// `schtasks /Create` all stayed green (#353).
pub fn escaping_args() -> Vec<String> {
    [
        "$HOME",           // systemd variable expansion  -> $$HOME
        "100%",            // systemd specifier           -> 100%%
        "a b",             // whitespace: must stay ONE argument on both
        r#"C:\dir\"#,      // trailing backslash, but NO space: must stay unquoted
        r#"C:\My Vault\"#, // space AND trailing backslash — the only shape that
        // reaches the Windows backslash-doubling rule at all, and the real one
        // (`--vault C:\My Vault\ob`) that motivated `quote_win_arg` in v3.4.21.
        "semi;colon",    // systemd word splitter
        "it's",          // stray single quote opens a quoted region
        "quote\"inside", // literal double quote through both layers
        r#"C:\dir\"q""#, // backslash run IMMEDIATELY before a quote — the
        // interior half of the Windows doubling rule. `quote"inside` does not
        // reach it (the quote is preceded by a letter), and neither did any
        // unit test: `cargo llvm-cov` reported the loop body unexecuted.
        "a&b<c>d", // the XML layer, round-tripped through a real sink
        "",        // empty argument: its own branch in BOTH quoters
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The escaping fixture's entry, built once so the regenerator that WRITES
/// it and the drift guard that CHECKS it cannot come to describe different
/// things.
pub fn escaping_entry(command: &str) -> ScheduleEntry {
    let mut e = daily_command_entry();
    e.command = Some(command.to_string());
    e.args = Some(crate::scheduler::Args::List(escaping_args()));
    e
}

/// A command that is NOT onebrain — guards `should_append_vault`.
pub fn foreign_command_entry() -> ScheduleEntry {
    ScheduleEntry {
        cron: Some("0 5 * * *".to_string()),
        command: Some("rsync".to_string()),
        args: Some(crate::scheduler::Args::List(vec![
            "-av".to_string(),
            "/a".to_string(),
            "/b".to_string(),
        ])),
        ..Default::default()
    }
}

/// Skill-mode entry with an arbitrary cron. Built WITHOUT running
/// `validate_cron`, which lets shape tests exercise defensive arms that
/// validation normally makes unreachable (DOM+DOW). The single most-used
/// fixture in the Windows-translation tests.
pub fn entry_cron(cron: &str) -> ScheduleEntry {
    ScheduleEntry {
        cron: Some(cron.to_string()),
        skill: Some("/daily".to_string()),
        ..Default::default()
    }
}
