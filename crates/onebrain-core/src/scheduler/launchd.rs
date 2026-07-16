//! Launchd plist emitter. Mirrors Bun `src/lib/scheduler/launchd.ts`
//! byte-for-byte for **skill-mode** plists — every newline and indent space
//! must match for the Layer-4 parity test against the Bun v2.3.3 binary.
//!
//! **Command-mode is a deliberate divergence from Bun (#263):** onebrain
//! command-mode entries now append `--vault <path>` to their argv (launchd
//! runs jobs with `cwd=/`, so the binary can't otherwise find the vault).
//! Bun v2.3.3 never emitted this, so the byte-parity claim above is scoped
//! to skill-mode; command-mode output intentionally differs.
//!
//! Implementation is **string templating, not `quick-xml`.** A round-trip
//! through `quick-xml` would re-format whitespace, breaking the byte
//! contract. The escape helper [`xml_escape`] mirrors Bun's exact
//! `.replace(/&/g, '&amp;')...` chain.

use crate::scheduler::cron_parse::{at_to_launchd, cron_fields_to_launchd_expanded, CronFields};
use crate::scheduler::entry::{is_command_mode, is_one_shot};
use crate::scheduler::types::{Args, ScheduleEntry};
use std::path::{Path, PathBuf};

/// Context required to emit a single plist — paths, the CLI binary path
/// that launchd should exec, and the user's UID for `launchctl bootout`.
pub struct LaunchdContext {
    /// Absolute path to the vault root (passed as `--vault` in skill-mode plists).
    pub vault_path: PathBuf,

    /// Absolute path to the `onebrain` binary launchd should exec.
    pub skill_cli_path: String,

    /// Absolute path to `07-logs/scheduler` (or vault-yml-overridden equivalent).
    pub log_base_path: PathBuf,

    /// User homedir (drives `~/Library/LaunchAgents/<label>.plist`).
    pub homedir: PathBuf,

    /// User's effective UID (drives `launchctl bootout gui/<uid>/<label>`).
    pub uid: u32,
}

/// Escape XML-sensitive chars in attribute and text-content positions.
///
/// Mirrors Bun's chain `.replace(/&/, '&amp;').replace(/</, '&lt;')
/// .replace(/>/, '&gt;').replace(/"/, '&quot;')`. Order matters — `&` first
/// so the literal ampersand in `&amp;` is not double-escaped.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Derive the launchd label suffix from an entry.
///
/// Command-mode label uses the basename of `entry.command` so
/// `command: onebrain` and `command: /opt/homebrew/bin/onebrain` produce
/// the same plist file path when everything else about the entry matches
/// (the collision detector relies on this: same binary, different
/// spelling, is intentionally one label).
///
/// Two `command:` entries invoking the *same* binary with *different* args
/// or cron/at expressions are distinct schedules and must NOT collapse to
/// one plist — see #116 bug 2. We append a short discriminator derived
/// from the entry's args (preferred) or, if args are absent/empty, from
/// its cron/at expression, so they land on different plist paths. If
/// command + args + cron/at are all identical, the discriminator is also
/// identical and the entries correctly collapse to one label (a genuine
/// duplicate, still caught by `detect_collisions`).
///
/// Skill-mode strips the leading `/` and is left unchanged (`com.onebrain.daily`
/// etc.) — this discriminator only applies to command mode.
///
/// Note: a skill-mode label and a command-mode label are never derived from
/// the same input space in a way that could collide in practice — a skill
/// name always starts as a real `SKILL.md` directory name under
/// `.claude/plugins/onebrain/skills/`, validated by `validate_schedulable`
/// before registration, whereas a command-mode label is a binary basename
/// plus an args/cron discriminator suffix. `detect_collisions` still checks
/// the final plist PATH regardless of mode, so even a hypothetical
/// literal-string collision between the two would be caught there, not
/// silently accepted.
pub fn label_for_entry(entry: &ScheduleEntry) -> String {
    if is_command_mode(entry) {
        let cmd = entry.command.as_deref().unwrap_or("");
        let basename = Path::new(cmd)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(cmd);
        match command_discriminator(entry) {
            Some(disc) => sanitize_label(&format!("{basename}-{disc}")),
            None => sanitize_label(basename),
        }
    } else {
        let raw = entry.skill.as_deref().unwrap_or("").trim_start_matches('/');
        sanitize_label(raw)
    }
}

/// Derive a short label discriminator for a command-mode entry from its
/// args (preferred, since args are the more common source of distinction
/// between two entries sharing a binary — e.g. `rsync -av src dst` vs
/// `rsync -av other-src other-dst`) or, when args are absent/empty, from
/// its schedule expression (cron or at). Returns `None` when neither is
/// present (nothing to discriminate on — label falls back to the bare
/// basename, matching the pre-#116 behavior for the common single-entry
/// case).
fn command_discriminator(entry: &ScheduleEntry) -> Option<String> {
    let from_args = match &entry.args {
        Some(Args::List(argv)) if !argv.is_empty() => Some(argv.join("-")),
        _ => None,
    };
    let raw = from_args.or_else(|| entry.cron.clone().or_else(|| entry.at.clone()))?;
    const MAX_LEN: usize = 40;
    let truncated: String = raw.chars().take(MAX_LEN).collect();
    Some(sanitize_label(&truncated))
}

fn sanitize_label(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Compute the on-disk plist path for a skill or label string.
///
/// Accepts either a leading-slash skill name (`/daily`) or a pre-stripped
/// label (`daily`) — same dual-input shape as Bun. Non-`[a-zA-Z0-9-]` are
/// replaced with `-`.
pub fn plist_path(skill_or_label: &str, homedir: &Path) -> PathBuf {
    let stripped = skill_or_label.trim_start_matches('/');
    let label_safe = sanitize_label(stripped);
    homedir
        .join("Library/LaunchAgents")
        .join(format!("com.onebrain.{label_safe}.plist"))
}

/// Build the `<ProgramArguments>` body for a recurring skill-mode entry.
fn recurring_skill_block(entry: &ScheduleEntry, ctx: &LaunchdContext) -> String {
    let mut out = format!(
        "        <string>{}</string>\n\
         \x20       <string>skill</string>\n\
         \x20       <string>run</string>\n\
         \x20       <string>--vault</string>\n\
         \x20       <string>{}</string>\n\
         \x20       <string>--skill</string>\n\
         \x20       <string>{}</string>",
        xml_escape(&ctx.skill_cli_path),
        xml_escape(&ctx.vault_path.to_string_lossy()),
        xml_escape(entry.skill.as_deref().unwrap_or("")),
    );
    if let Some(Args::Map(map)) = &entry.args {
        for (k, v) in map.iter() {
            out.push('\n');
            out.push_str("        <string>--arg</string>\n        <string>");
            out.push_str(&xml_escape(&format!("{k}={v}")));
            out.push_str("</string>");
        }
    }
    out
}

/// Strip a trailing `.exe` (Windows) from a filename and return the bare
/// stem. `onebrain.exe` → `onebrain`; `rsync` → `rsync`.
fn strip_exe(name: &str) -> &str {
    name.strip_suffix(".exe").unwrap_or(name)
}

/// The basename of a path/command string, with any `.exe` suffix stripped.
/// Returns `None` when the path has no final component.
fn command_basename(cmd: &str) -> Option<&str> {
    Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .map(strip_exe)
}

/// True when a command-mode entry's `command` resolves to the onebrain
/// binary itself — the ONLY case where appending `--vault` is meaningful.
///
/// Command-mode explicitly supports generic binaries (`command: rsync,
/// args: [-av, /vault, /backup]` is a documented preset). Appending
/// `--vault <path>` to `rsync` would produce `rsync … --vault <path>`,
/// which rsync rejects as an unknown flag — the scheduled job would fail
/// on every fire (#263 R2 regression). So we gate the append on the
/// command actually being onebrain.
///
/// Detection is by basename so both the bare `onebrain` and an absolute
/// `/opt/homebrew/bin/onebrain` spelling match (the register path rewrites
/// command to an absolute path before this runs), plus a Windows
/// `onebrain.exe`. We also match when the command's basename equals
/// `ctx.skill_cli_path`'s basename — the exact binary launchd would exec
/// for skill-mode — so a renamed/aliased install still resolves.
fn command_is_onebrain(cmd: &str, ctx: &LaunchdContext) -> bool {
    let Some(cmd_base) = command_basename(cmd) else {
        return false;
    };
    if cmd_base == "onebrain" {
        return true;
    }
    command_basename(&ctx.skill_cli_path).is_some_and(|cli_base| cli_base == cmd_base)
}

/// True when the entry's command-mode `args:` already carry an explicit
/// vault flag — `--vault`, `--vault-dir`, or their `=`-joined forms. When
/// present we must NOT append our own `--vault`: respect the user's
/// explicit choice and avoid a double-flag clobber (#263 R2 finding).
fn args_have_explicit_vault(entry: &ScheduleEntry) -> bool {
    match &entry.args {
        Some(Args::List(argv)) => argv.iter().any(|a| {
            a == "--vault"
                || a == "--vault-dir"
                || a.starts_with("--vault=")
                || a.starts_with("--vault-dir=")
        }),
        _ => false,
    }
}

/// Whether `--vault <ctx.vault_path>` should be appended to a command-mode
/// entry's argv: only when the command is onebrain (a generic binary would
/// choke on the flag) AND the user hasn't already supplied a vault flag.
fn should_append_vault(entry: &ScheduleEntry, ctx: &LaunchdContext) -> bool {
    command_is_onebrain(entry.command.as_deref().unwrap_or(""), ctx)
        && !args_have_explicit_vault(entry)
}

/// Build the `<ProgramArguments>` body for a recurring command-mode entry.
/// Each argv element becomes a `<string>` line.
///
/// launchd runs every job with `cwd=/`, so an onebrain command-mode entry
/// can't rely on cwd to find the vault the way an interactive shell
/// invocation would. When the command is onebrain (see [`should_append_vault`])
/// we append `--vault <ctx.vault_path>` after the user's own args — an
/// explicit flag, preferred over setting `WorkingDirectory` (which would
/// also change the meaning of any relative paths the user's `args:` already
/// assume). Generic commands (rsync, etc.) get their argv UNCHANGED.
/// Mirrors `recurring_skill_block`, which already embeds `--vault` (#263 bug 1).
fn recurring_command_block(entry: &ScheduleEntry, ctx: &LaunchdContext) -> String {
    let cmd = entry.command.as_deref().unwrap_or("");
    let mut lines: Vec<String> = vec![format!("        <string>{}</string>", xml_escape(cmd))];
    if let Some(Args::List(argv)) = &entry.args {
        for a in argv {
            lines.push(format!("        <string>{}</string>", xml_escape(a)));
        }
    }
    if should_append_vault(entry, ctx) {
        lines.push("        <string>--vault</string>".to_string());
        lines.push(format!(
            "        <string>{}</string>",
            xml_escape(&ctx.vault_path.to_string_lossy())
        ));
    }
    lines.join("\n")
}

/// Build the `<ProgramArguments>` body for a one-shot skill-mode entry —
/// wraps a self-deleting shell command for launchctl bootout + rm.
fn one_shot_skill_block(entry: &ScheduleEntry, ctx: &LaunchdContext, label: &str) -> String {
    let plist_file = format!(
        "{}/Library/LaunchAgents/{label}.plist",
        ctx.homedir.to_string_lossy()
    );
    let args_flags: String = match &entry.args {
        Some(Args::Map(map)) if !map.is_empty() => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("--arg=\"{k}={v}\""))
                .collect();
            format!(" {}", parts.join(" "))
        }
        _ => String::new(),
    };
    let shell = format!(
        "\"{}\" skill run --vault=\"{}\" --skill=\"{}\"{}; launchctl bootout gui/{}/{}; rm -f \"{}\"",
        ctx.skill_cli_path,
        ctx.vault_path.to_string_lossy(),
        entry.skill.as_deref().unwrap_or(""),
        args_flags,
        ctx.uid,
        label,
        plist_file,
    );
    format!(
        "        <string>/bin/sh</string>\n\
         \x20       <string>-c</string>\n\
         \x20       <string>{}</string>",
        xml_escape(&shell)
    )
}

/// Build the `<ProgramArguments>` body for a one-shot command-mode entry.
///
/// Same `--vault` rationale and gating as [`recurring_command_block`]
/// (#263 bug 1 + R2): launchd's `cwd=/` means an onebrain command needs the
/// vault path in its own argv, appended after any user-supplied args and
/// quoted the same way. A generic command (rsync, etc.) is left untouched.
fn one_shot_command_block(entry: &ScheduleEntry, ctx: &LaunchdContext, label: &str) -> String {
    let plist_file = format!(
        "{}/Library/LaunchAgents/{label}.plist",
        ctx.homedir.to_string_lossy()
    );
    let mut parts: Vec<String> = match &entry.args {
        Some(Args::List(argv)) if !argv.is_empty() => {
            argv.iter().map(|a| format!("\"{a}\"")).collect()
        }
        _ => Vec::new(),
    };
    if should_append_vault(entry, ctx) {
        parts.push("\"--vault\"".to_string());
        parts.push(format!("\"{}\"", ctx.vault_path.to_string_lossy()));
    }
    let quoted_args = if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" "))
    };
    let inner = format!(
        "\"{}\"{}",
        entry.command.as_deref().unwrap_or(""),
        quoted_args
    );
    let shell = format!(
        "{}; launchctl bootout gui/{}/{}; rm -f \"{}\"",
        inner, ctx.uid, label, plist_file
    );
    format!(
        "        <string>/bin/sh</string>\n\
         \x20       <string>-c</string>\n\
         \x20       <string>{}</string>",
        xml_escape(&shell)
    )
}

/// Format one `CronFields` combination's non-wildcard keys, in (Minute,
/// Hour, Day, Month, Weekday) insertion order, indented as the contents of
/// a single `<dict>`.
fn single_combination_dict_body(f: &CronFields) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = f.minute {
        parts.push(format!(
            "        <key>Minute</key>\n        <integer>{m}</integer>"
        ));
    }
    if let Some(h) = f.hour {
        parts.push(format!(
            "        <key>Hour</key>\n        <integer>{h}</integer>"
        ));
    }
    if let Some(d) = f.day {
        parts.push(format!(
            "        <key>Day</key>\n        <integer>{d}</integer>"
        ));
    }
    if let Some(mo) = f.month {
        parts.push(format!(
            "        <key>Month</key>\n        <integer>{mo}</integer>"
        ));
    }
    if let Some(w) = f.weekday {
        parts.push(format!(
            "        <key>Weekday</key>\n        <integer>{w}</integer>"
        ));
    }
    parts.join("\n")
}

/// Re-indent a block of lines by `indent` extra spaces (used to nest a
/// `<dict>` body one level deeper inside `<array>...</array>`).
fn indent_lines(body: &str, indent: &str) -> String {
    body.lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the complete `<key>StartCalendarInterval</key>` block (key +
/// value) — cron emits either a single `<dict>` (the common case: every
/// field is a bare value or wildcard, byte-identical to the pre-#116
/// shape) or an `<array>` of `<dict>`s (#116 bug 1: step/list/range fields
/// that expand to more than one concrete combination — launchd ORs across
/// array entries, which is exactly cron's "any of these values" semantics).
/// One-shot entries always emit a single `<dict>` (a one-shot fires once —
/// its Year/Month/Day/Hour/Minute are always fully concrete).
fn calendar_block(entry: &ScheduleEntry) -> String {
    if is_one_shot(entry) {
        let f = at_to_launchd(entry.at.as_deref().unwrap());
        let body = format!(
            "        <key>Year</key>\n\
             \x20       <integer>{}</integer>\n\
             \x20       <key>Month</key>\n\
             \x20       <integer>{}</integer>\n\
             \x20       <key>Day</key>\n\
             \x20       <integer>{}</integer>\n\
             \x20       <key>Hour</key>\n\
             \x20       <integer>{}</integer>\n\
             \x20       <key>Minute</key>\n\
             \x20       <integer>{}</integer>",
            f.year, f.month, f.day, f.hour, f.minute
        );
        format!("    <key>StartCalendarInterval</key>\n    <dict>\n{body}\n    </dict>")
    } else {
        let set = cron_fields_to_launchd_expanded(entry.cron.as_deref().unwrap());
        let combos = set
            .combinations()
            .expect("validate_cron failed to gate the StartCalendarInterval combination cap");
        if combos.len() <= 1 {
            // Single combination — identical shape to the pre-#116 plist
            // (byte-parity snapshot depends on this staying a plain
            // `<dict>`, not a one-element `<array>`).
            let body = combos
                .first()
                .map(single_combination_dict_body)
                .unwrap_or_default();
            format!("    <key>StartCalendarInterval</key>\n    <dict>\n{body}\n    </dict>")
        } else {
            let dicts: Vec<String> = combos
                .iter()
                .map(|f| {
                    let body = single_combination_dict_body(f);
                    format!(
                        "        <dict>\n{}\n        </dict>",
                        indent_lines(&body, "    ")
                    )
                })
                .collect();
            format!(
                "    <key>StartCalendarInterval</key>\n    <array>\n{}\n    </array>",
                dicts.join("\n")
            )
        }
    }
}

/// Emit a complete launchd plist for the given entry. Byte parity with Bun
/// is mandatory — adjust whitespace only with the parity test running.
pub fn generate_plist(entry: &ScheduleEntry, ctx: &LaunchdContext) -> String {
    let label_safe = label_for_entry(entry);
    let label = format!("com.onebrain.{label_safe}");

    let calendar = calendar_block(entry);
    let program_args = match (is_one_shot(entry), is_command_mode(entry)) {
        (true, true) => one_shot_command_block(entry, ctx, &label),
        (true, false) => one_shot_skill_block(entry, ctx, &label),
        (false, true) => recurring_command_block(entry, ctx),
        (false, false) => recurring_skill_block(entry, ctx),
    };

    let log_base = ctx.log_base_path.to_string_lossy();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>Label</key>\n\
         \x20   <string>{}</string>\n\
         \x20   <key>ProgramArguments</key>\n\
         \x20   <array>\n\
         {}\n\
         \x20   </array>\n\
         {}\n\
         \x20   <key>StandardOutPath</key>\n\
         \x20   <string>{}/onebrain-{}.stdout</string>\n\
         \x20   <key>StandardErrorPath</key>\n\
         \x20   <string>{}/onebrain-{}.stderr</string>\n\
         \x20   <key>RunAtLoad</key>\n\
         \x20   <false/>\n\
         </dict>\n\
         </plist>",
        xml_escape(&label),
        program_args,
        calendar,
        xml_escape(&log_base),
        label_safe,
        xml_escape(&log_base),
        label_safe,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn test_ctx() -> LaunchdContext {
        test_ctx_with_vault("/Users/test/vault")
    }

    /// Same as [`test_ctx`] but with an arbitrary vault path — used to
    /// exercise paths containing spaces (#263 motivating scenario).
    fn test_ctx_with_vault(vault: &str) -> LaunchdContext {
        LaunchdContext {
            vault_path: PathBuf::from(vault),
            skill_cli_path: "/opt/homebrew/bin/onebrain".into(),
            log_base_path: PathBuf::from("/Users/test/vault/07-logs/scheduler/2026/05"),
            homedir: PathBuf::from("/Users/test"),
            uid: 501,
        }
    }

    fn skill_entry(skill: &str, cron: &str) -> ScheduleEntry {
        ScheduleEntry {
            cron: Some(cron.into()),
            skill: Some(skill.into()),
            ..Default::default()
        }
    }

    #[test]
    fn xml_escape_handles_all_four_chars() {
        assert_eq!(
            xml_escape("a & b < c > d \"e\""),
            "a &amp; b &lt; c &gt; d &quot;e&quot;"
        );
    }

    #[test]
    fn xml_escape_amp_not_double_escaped() {
        // If we replaced '<' before '&', "&lt;" would become "&amp;lt;".
        assert_eq!(xml_escape("&"), "&amp;");
    }

    #[test]
    fn label_for_skill_strips_leading_slash() {
        let e = skill_entry("/daily", "0 9 * * *");
        assert_eq!(label_for_entry(&e), "daily");
    }

    #[test]
    fn label_for_command_uses_basename_plus_cron_discriminator() {
        // No args → falls back to a cron-derived discriminator (#116 bug 2)
        // so two `command:` entries on different schedules don't collide.
        let e = ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("/opt/homebrew/bin/onebrain".into()),
            ..Default::default()
        };
        assert_eq!(label_for_entry(&e), "onebrain-0-3-----0");
    }

    #[test]
    fn label_replaces_non_alphanum_with_dash() {
        // hypothetical skill with `.` — must be sanitized
        let e = skill_entry("/foo.bar", "0 9 * * *");
        assert_eq!(label_for_entry(&e), "foo-bar");
    }

    #[test]
    fn plist_path_returns_launch_agents_path() {
        let p = plist_path("/daily", Path::new("/Users/test"));
        assert_eq!(
            p,
            PathBuf::from("/Users/test/Library/LaunchAgents/com.onebrain.daily.plist")
        );
    }

    #[test]
    fn plist_path_handles_pre_stripped_label() {
        let p = plist_path("daily", Path::new("/Users/test"));
        assert_eq!(
            p,
            PathBuf::from("/Users/test/Library/LaunchAgents/com.onebrain.daily.plist")
        );
    }

    #[test]
    fn recurring_skill_emits_skill_run_subcommand() {
        let out = generate_plist(&skill_entry("/daily", "0 9 * * *"), &test_ctx());
        assert!(out.contains("<string>com.onebrain.daily</string>"));
        assert!(
            out.contains("<key>Hour</key>\n        <integer>9</integer>"),
            "out:\n{out}"
        );
        assert!(out.contains("<string>/opt/homebrew/bin/onebrain</string>"));
        assert!(out.contains("<string>skill</string>\n        <string>run</string>"));
        assert!(out.contains("<string>--vault</string>"));
        assert!(out.contains("<string>/Users/test/vault</string>"));
        assert!(out.contains("<string>--skill</string>"));
        assert!(out.contains("<string>/daily</string>"));
        // Pre-v2.3.3 contract used --headless. Verify it's gone.
        assert!(!out.contains("<string>--headless</string>"));
    }

    #[test]
    fn recurring_skill_with_args_emits_arg_kv_pairs() {
        let mut e = skill_entry("/distill", "0 12 * * 0");
        let mut map = IndexMap::new();
        map.insert("topic".into(), "this-week".into());
        e.args = Some(Args::Map(map));
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("<string>--arg</string>"));
        assert!(
            out.contains("<string>topic=this-week</string>"),
            "out:\n{out}"
        );
    }

    #[test]
    fn recurring_skill_escapes_xml_special_chars_in_arg_values() {
        let mut e = skill_entry("/echo", "0 9 * * *");
        let mut map = IndexMap::new();
        map.insert("msg".into(), "a & b < c".into());
        e.args = Some(Args::Map(map));
        let out = generate_plist(&e, &test_ctx());
        assert!(
            out.contains("<string>msg=a &amp; b &lt; c</string>"),
            "out:\n{out}"
        );
        // Raw chars must not appear in the output anywhere.
        assert!(!out.contains("msg=a & b"));
    }

    #[test]
    fn recurring_skill_no_blank_line_when_args_absent() {
        let out = generate_plist(&skill_entry("/daily", "0 9 * * *"), &test_ctx());
        // Bun guarantees a tight `<string>/daily</string>\n    </array>` join.
        assert!(!out.contains("<string>/daily</string>\n\n"), "out:\n{out}");
    }

    #[test]
    fn one_shot_skill_emits_year_month_day_hour_minute() {
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            skill: Some("/reminder".into()),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("<key>Year</key>\n        <integer>2026</integer>"));
        assert!(out.contains("<key>Month</key>\n        <integer>5</integer>"));
        assert!(out.contains("<key>Day</key>\n        <integer>13</integer>"));
        assert!(out.contains("<key>Hour</key>\n        <integer>14</integer>"));
        assert!(out.contains("<key>Minute</key>\n        <integer>30</integer>"));
    }

    #[test]
    fn one_shot_skill_shell_wrapper_invokes_skill_run_and_self_deletes() {
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            skill: Some("/reminder".into()),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("<string>/bin/sh</string>"));
        assert!(out.contains("<string>-c</string>"));
        assert!(out.contains("skill run"));
        assert!(out.contains("--vault=&quot;/Users/test/vault&quot;"));
        assert!(out.contains("--skill=&quot;/reminder&quot;"));
        assert!(out.contains("launchctl bootout gui/501/com.onebrain.reminder"));
        assert!(out.contains("rm -f"));
        assert!(!out.contains("--headless"));
    }

    #[test]
    fn one_shot_skill_args_use_arg_quoted_form_inside_wrapper() {
        let mut e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            skill: Some("/echo".into()),
            ..Default::default()
        };
        let mut map = IndexMap::new();
        map.insert("msg".into(), "hello".into());
        e.args = Some(Args::Map(map));
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("--arg=&quot;msg=hello&quot;"));
    }

    #[test]
    fn recurring_command_emits_hook_style_program_arguments() {
        // #263 bug 1: launchd runs jobs with cwd=/, so a command-mode entry
        // MUST embed `--vault <path>` in its own argv — it can't rely on cwd
        // to find the vault (skill-mode already does this; command-mode was
        // the gap). The `--skill` / `skill run` shape stays absent — that's
        // what makes this "hook-style" (plain `command + args[]`, matching
        // Claude Code's hooks.json convention) rather than skill-mode.
        let e = ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("/opt/homebrew/bin/onebrain".into()),
            args: Some(Args::List(vec!["qmd-reindex".into()])),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("<string>/opt/homebrew/bin/onebrain</string>"));
        assert!(out.contains("<string>qmd-reindex</string>"));
        assert!(!out.contains("<string>--skill</string>"));
        assert!(
            out.contains("<string>--vault</string>"),
            "command-mode argv must embed --vault so launchd's cwd=/ can't \
             break vault discovery, out:\n{out}"
        );
        assert!(
            out.contains("<string>/Users/test/vault</string>"),
            "expected the ctx vault_path after --vault, out:\n{out}"
        );
        assert!(!out.contains("<string>skill</string>\n        <string>run</string>"));
    }

    #[test]
    fn command_label_consistent_between_absolute_and_bare_form() {
        // Same binary (bare vs absolute spelling), same args, same cron →
        // same label. This is the "intentionally one label" case #116 bug 2
        // must preserve: only *differing* args/cron should split the label.
        let bare = ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("onebrain".into()),
            args: Some(Args::List(vec!["qmd-reindex".into()])),
            ..Default::default()
        };
        let abs = ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("/opt/homebrew/bin/onebrain".into()),
            args: Some(Args::List(vec!["qmd-reindex".into()])),
            ..Default::default()
        };
        let out_bare = generate_plist(&bare, &test_ctx());
        let out_abs = generate_plist(&abs, &test_ctx());
        assert!(out_bare.contains("<string>com.onebrain.onebrain-qmd-reindex</string>"));
        assert!(out_abs.contains("<string>com.onebrain.onebrain-qmd-reindex</string>"));
    }

    #[test]
    fn command_label_differs_when_args_differ() {
        // #116 bug 2: two `command: onebrain` entries with different args
        // must land on DISTINCT plist labels (not silently collapse into a
        // false collision).
        let a = ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("onebrain".into()),
            args: Some(Args::List(vec!["qmd-reindex".into()])),
            ..Default::default()
        };
        let b = ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("onebrain".into()),
            args: Some(Args::List(vec!["backup".into()])),
            ..Default::default()
        };
        assert_ne!(label_for_entry(&a), label_for_entry(&b));
    }

    #[test]
    fn command_label_differs_when_cron_differs_and_args_absent() {
        // #116 bug 2: no args to discriminate on → fall back to the cron
        // expression so two same-binary entries on different schedules
        // still land on distinct labels.
        let a = ScheduleEntry {
            cron: Some("0 9 * * *".into()),
            command: Some("/usr/local/bin/backup".into()),
            ..Default::default()
        };
        let b = ScheduleEntry {
            cron: Some("0 18 * * *".into()),
            command: Some("/usr/local/bin/backup".into()),
            ..Default::default()
        };
        assert_ne!(label_for_entry(&a), label_for_entry(&b));
    }

    #[test]
    fn command_label_identical_when_command_args_cron_all_match() {
        // Genuine duplicate: identical command + args + cron must still
        // collapse to the same label so `detect_collisions` catches it.
        let a = ScheduleEntry {
            cron: Some("0 9 * * *".into()),
            command: Some("/usr/local/bin/backup".into()),
            args: Some(Args::List(vec!["--full".into()])),
            ..Default::default()
        };
        let b = a.clone();
        assert_eq!(label_for_entry(&a), label_for_entry(&b));
    }

    #[test]
    fn one_shot_command_wraps_in_self_delete_shell() {
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            command: Some("/opt/homebrew/bin/onebrain".into()),
            args: Some(Args::List(vec!["qmd-reindex".into()])),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("<string>/bin/sh</string>"));
        assert!(out.contains("&quot;/opt/homebrew/bin/onebrain&quot; &quot;qmd-reindex&quot;"));
        assert!(out.contains("launchctl bootout gui/501/com.onebrain.onebrain-qmd-reindex"));
        assert!(out.contains("rm -f"));
        // #263: onebrain one-shot command embeds --vault too (quoted).
        assert!(
            out.contains("&quot;--vault&quot; &quot;/Users/test/vault&quot;"),
            "onebrain one-shot argv must carry a quoted --vault, out:\n{out}"
        );
    }

    // ── #263 R2: --vault appended for onebrain ONLY, never generic binaries ──

    #[test]
    fn onebrain_recurring_command_with_no_args_gets_vault_appended() {
        // #263: an onebrain command with zero user-supplied args still gets
        // `--vault <path>` appended (it's not left a bare one-element argv).
        let e = ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("/opt/homebrew/bin/onebrain".into()),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("<string>/opt/homebrew/bin/onebrain</string>"));
        assert!(out.contains("<string>--vault</string>"));
        assert!(out.contains("<string>/Users/test/vault</string>"));
    }

    #[test]
    fn onebrain_bare_and_exe_command_names_detected() {
        // Detection is basename-based: bare `onebrain` and an `onebrain.exe`
        // (Windows binary name) both count as the onebrain binary → --vault
        // appended. Uses a forward-slash path for the `.exe` case so the
        // test is host-agnostic (std::path only splits on `\` on Windows).
        for cmd in ["onebrain", "/opt/homebrew/bin/onebrain.exe"] {
            let e = ScheduleEntry {
                cron: Some("0 3 * * 0".into()),
                command: Some(cmd.into()),
                args: Some(Args::List(vec!["search".into(), "reindex".into()])),
                ..Default::default()
            };
            let out = generate_plist(&e, &test_ctx());
            assert!(
                out.contains("<string>--vault</string>"),
                "expected --vault for onebrain command `{cmd}`, out:\n{out}"
            );
        }
    }

    #[test]
    fn recurring_generic_command_gets_no_vault() {
        // #263 R2 REGRESSION GUARD: command-mode supports generic binaries
        // (the documented `command: rsync, args: [-av, /vault, /backup]`
        // preset). Appending `--vault` there would make rsync fail on an
        // unknown flag every fire. A non-onebrain command must be UNCHANGED.
        let e = ScheduleEntry {
            cron: Some("0 5 * * *".into()),
            command: Some("/usr/bin/rsync".into()),
            args: Some(Args::List(vec![
                "-av".into(),
                "/vault".into(),
                "/backup".into(),
            ])),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("<string>/usr/bin/rsync</string>"));
        assert!(out.contains("<string>-av</string>"));
        assert!(
            !out.contains("<string>--vault</string>"),
            "generic rsync command must NOT get --vault appended, out:\n{out}"
        );
    }

    #[test]
    fn recurring_generic_command_with_no_args_gets_no_vault() {
        // A generic no-args command stays a bare one-element argv — no
        // spurious --vault, and no empty trailing string.
        let e = ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("/usr/bin/true".into()),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("<string>/usr/bin/true</string>"));
        assert!(!out.contains("<string>--vault</string>"));
    }

    #[test]
    fn one_shot_generic_command_gets_no_vault() {
        // #263 R2 REGRESSION GUARD, one-shot variant: a generic one-shot
        // command must not get --vault injected into its shell wrapper. Use
        // `/src /dst` (not `/vault /backup`) as the paths so the assertion
        // isn't confused by a `--vault` substring in the sanitized label
        // discriminator — we assert on the precise quoted flag form the
        // appended argv would take.
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            command: Some("/usr/bin/rsync".into()),
            args: Some(Args::List(vec!["-av".into(), "/src".into(), "/dst".into()])),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("&quot;/usr/bin/rsync&quot;"));
        assert!(
            !out.contains("&quot;--vault&quot;"),
            "generic one-shot rsync command must NOT get --vault, out:\n{out}"
        );
    }

    #[test]
    fn onebrain_recurring_command_with_explicit_vault_arg_not_doubled() {
        // #263 R2: respect a user's explicit --vault in args — don't append
        // a second one.
        let e = ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("onebrain".into()),
            args: Some(Args::List(vec![
                "search".into(),
                "reindex".into(),
                "--vault".into(),
                "/other/vault".into(),
            ])),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx());
        assert_eq!(
            out.matches("<string>--vault</string>").count(),
            1,
            "exactly one --vault (the user's), no appended duplicate, out:\n{out}"
        );
        // The appended ctx vault path must NOT be present — only the user's.
        assert!(!out.contains("<string>/Users/test/vault</string>"));
        assert!(out.contains("<string>/other/vault</string>"));
    }

    #[test]
    fn onebrain_command_with_explicit_vault_equals_form_not_doubled() {
        // The `--vault=<x>` and `--vault-dir` spellings are also honored.
        for arg in ["--vault=/other", "--vault-dir=/other", "--vault-dir"] {
            let e = ScheduleEntry {
                cron: Some("0 3 * * 0".into()),
                command: Some("onebrain".into()),
                args: Some(Args::List(vec!["reindex".into(), arg.into()])),
                ..Default::default()
            };
            let out = generate_plist(&e, &test_ctx());
            assert!(
                !out.contains("<string>/Users/test/vault</string>"),
                "explicit vault flag `{arg}` should suppress the appended \
                 ctx vault, out:\n{out}"
            );
        }
    }

    #[test]
    fn onebrain_recurring_command_vault_path_with_space_round_trips() {
        // #263 motivating scenario: a vault path containing a space must
        // round-trip. In recurring (hook-style) argv each element is its own
        // `<string>`, so the space needs no shell quoting — it lives intact
        // inside one `<string>` element.
        let e = ScheduleEntry {
            cron: Some("0 3 * * 0".into()),
            command: Some("onebrain".into()),
            args: Some(Args::List(vec!["reindex".into()])),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx_with_vault("/tmp/My Vault/x"));
        assert!(out.contains("<string>--vault</string>"));
        assert!(
            out.contains("<string>/tmp/My Vault/x</string>"),
            "space-containing vault path must be one intact <string>, out:\n{out}"
        );
    }

    #[test]
    fn onebrain_one_shot_command_vault_path_with_space_round_trips() {
        // One-shot wraps argv in a `/bin/sh -c` string, so a space in the
        // vault path must be shell-quoted (double-quoted) to survive as a
        // single argument.
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            command: Some("onebrain".into()),
            args: Some(Args::List(vec!["reindex".into()])),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx_with_vault("/tmp/My Vault/x"));
        // xml_escape turns the surrounding double-quotes into &quot; — the
        // space stays literal inside them.
        assert!(
            out.contains("&quot;--vault&quot; &quot;/tmp/My Vault/x&quot;"),
            "space-containing vault path must be quoted in the shell wrapper, out:\n{out}"
        );
    }

    #[test]
    fn command_mode_args_xml_escaped() {
        let e = ScheduleEntry {
            cron: Some("0 5 * * *".into()),
            command: Some("/usr/local/bin/rclone".into()),
            args: Some(Args::List(vec!["--exclude".into(), "a & b".into()])),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx());
        assert!(out.contains("<string>--exclude</string>"));
        assert!(out.contains("<string>a &amp; b</string>"));
        assert!(!out.contains("<string>a & b</string>"));
    }

    #[test]
    fn generate_plist_snapshot_recurring_skill() {
        let out = generate_plist(&skill_entry("/daily", "0 9 * * *"), &test_ctx());
        insta::assert_snapshot!(out);
    }

    // ── #116 bug 1: step/list/range → array-form StartCalendarInterval ────────

    #[test]
    fn calendar_block_single_combination_stays_plain_dict() {
        // A bare-value/wildcard cron (the common case) must still produce
        // a single `<dict>`, not a one-element `<array>` — byte parity
        // with the existing snapshot depends on this.
        let out = generate_plist(&skill_entry("/daily", "0 9 * * *"), &test_ctx());
        assert!(out.contains("<key>StartCalendarInterval</key>\n    <dict>"));
        assert!(!out.contains("<key>StartCalendarInterval</key>\n    <array>"));
    }

    #[test]
    fn calendar_block_step_hour_emits_array_of_dicts() {
        // `*/2` on hour → 12 combinations → array form, one <dict> per hour.
        let out = generate_plist(&skill_entry("/daily", "0 */2 * * *"), &test_ctx());
        assert!(out.contains("<key>StartCalendarInterval</key>\n    <array>"));
        assert!(out.contains("<key>Hour</key>\n            <integer>0</integer>"));
        assert!(out.contains("<key>Hour</key>\n            <integer>22</integer>"));
        // Every combination keeps Minute fixed at 0.
        let minute_count = out
            .matches("<key>Minute</key>\n            <integer>0</integer>")
            .count();
        assert_eq!(
            minute_count, 12,
            "expected 12 Minute keys (one per hour dict), out:\n{out}"
        );
    }

    #[test]
    fn calendar_block_list_weekday_emits_one_dict_per_value() {
        let out = generate_plist(&skill_entry("/daily", "0 9 * * 1,3,5"), &test_ctx());
        assert!(out.contains("<key>StartCalendarInterval</key>\n    <array>"));
        assert!(out.contains("<key>Weekday</key>\n            <integer>1</integer>"));
        assert!(out.contains("<key>Weekday</key>\n            <integer>3</integer>"));
        assert!(out.contains("<key>Weekday</key>\n            <integer>5</integer>"));
        let dict_count = out.matches("<dict>").count();
        // 1 top-level <dict> (the plist root) + 3 StartCalendarInterval dicts.
        assert_eq!(dict_count, 4, "out:\n{out}");
    }

    #[test]
    fn calendar_block_weekday_seven_emits_zero() {
        // Standard cron 7 = Sunday, but launchd's `Weekday` key only
        // understands 0-6 — must be normalized before it reaches the plist.
        let out = generate_plist(&skill_entry("/daily", "0 9 * * 7"), &test_ctx());
        assert!(out.contains("<key>Weekday</key>\n        <integer>0</integer>"));
        // Single value → stays plain <dict>, not <array>.
        assert!(out.contains("<key>StartCalendarInterval</key>\n    <dict>"));
    }

    #[test]
    fn calendar_block_range_weekday_emits_inclusive_dicts() {
        let out = generate_plist(&skill_entry("/daily", "0 9 * * 1-5"), &test_ctx());
        for w in 1..=5 {
            assert!(
                out.contains(&format!(
                    "<key>Weekday</key>\n            <integer>{w}</integer>"
                )),
                "missing weekday {w}, out:\n{out}"
            );
        }
    }

    #[test]
    fn generate_plist_snapshot_recurring_skill_array_form() {
        // New insta snapshot (#116 bug 1) covering the multi-value array
        // shape — distinct from the single-`<dict>` snapshot above.
        let out = generate_plist(&skill_entry("/daily", "0 */6 * * *"), &test_ctx());
        insta::assert_snapshot!(out);
    }

    // ── DOM/DOW: day-only restricted still works (weekday-only already
    // covered above by `calendar_block_list_weekday_emits_one_dict_per_value`
    // and `calendar_block_range_weekday_emits_inclusive_dicts`). The
    // both-restricted case is rejected upstream by
    // `cron_parse::validate_cron` — `generate_plist`/`calendar_block` assume
    // pre-validated input (see their doc comments), so a both-restricted
    // cron string never reaches this module in practice. ─────────────────

    #[test]
    fn calendar_block_day_only_restricted_emits_array_of_day_dicts() {
        let out = generate_plist(&skill_entry("/daily", "0 9 1,15 * *"), &test_ctx());
        assert!(out.contains("<key>StartCalendarInterval</key>\n    <array>"));
        assert!(out.contains("<key>Day</key>\n            <integer>1</integer>"));
        assert!(out.contains("<key>Day</key>\n            <integer>15</integer>"));
        // No Weekday key at all — day-of-month is the only restricted field.
        assert!(!out.contains("<key>Weekday</key>"));
    }
}
