//! Launchd plist emitter — the macOS renderer behind the scheduler backend
//! seam (`backend.rs`; Windows renders via `schtasks.rs`, Linux via
//! `systemd.rs`). The byte format originated as a byte-parity port of Bun
//! v2.3.3's `launchd.ts` and is now pinned by snapshot tests here — every
//! newline and indent space is contract.
//!
//! **Command-mode is a deliberate divergence from Bun (#263):** onebrain
//! command-mode entries now append `--vault <path>` to their argv (launchd
//! runs jobs with `cwd=/`, so the binary can't otherwise find the vault).
//! Bun v2.3.3 never emitted this, so the byte-parity claim above is scoped
//! to skill-mode; command-mode output intentionally differs.
//!
//! Implementation is **string templating, not `quick-xml`.** A round-trip
//! through `quick-xml` would re-format whitespace, breaking the byte
//! contract. XML escaping lives in [`crate::scheduler::xml`], shared with
//! the other renderers.

use crate::scheduler::context::SchedulerContext;
use crate::scheduler::cron_parse::{at_fields, cron_fields_expanded, CronFields};
use crate::scheduler::entry::{is_command_mode, is_one_shot};
use crate::scheduler::types::{Args, ScheduleEntry};
use crate::scheduler::xml::escape as xml_escape;
use std::path::{Path, PathBuf};

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
    let raw = raw_discriminator(entry)?;
    Some(sanitize_label(&bounded_discriminator(&raw)))
}

/// The unbounded source string a command-mode label discriminates on.
fn raw_discriminator(entry: &ScheduleEntry) -> Option<String> {
    let from_args = match &entry.args {
        Some(Args::List(argv)) if !argv.is_empty() => Some(argv.join("-")),
        _ => None,
    };
    from_args.or_else(|| entry.cron.clone().or_else(|| entry.at.clone()))
}

/// Bound the discriminator's length without letting two different inputs
/// land on the same label (#345).
///
/// A plain `take(40)` made entries whose args differ only after char 40
/// normalize identically — `detect_collisions` then refused the pair at
/// register time: fail-closed, but invisible until hit, and easy to hit with
/// path-bearing args. When the input would truncate, the tail becomes an
/// 8-hex digest of the FULL string instead.
///
/// The suffix is deliberately NOT applied to inputs that already fit: those
/// labels stay byte-identical, so an upgrade re-registers them onto the same
/// artifact and only the previously-at-risk entries change identity. The two
/// forms are also length-disjoint (≤ 40 vs exactly 41 chars), so a truncating
/// entry can never collide with a non-truncating one.
fn bounded_discriminator(raw: &str) -> String {
    const MAX_LEN: usize = 40;
    const PREFIX_LEN: usize = 32;
    if raw.chars().count() <= MAX_LEN {
        return raw.to_string();
    }
    let prefix: String = raw.chars().take(PREFIX_LEN).collect();
    format!("{prefix}-{}", short_hash(raw))
}

/// First 8 hex chars of sha256 — enough to keep same-prefix args distinct
/// without turning the label into something unreadable.
fn short_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(input.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The label this entry carried BEFORE v3.4.21's bounded discriminator (the
/// plain 40-char truncation), or `None` when its label is unchanged.
///
/// `register` uses this to remove the artifact the old label owned. Without
/// it a label change leaves the previous timer/task/plist installed and
/// firing, and `--remove` cannot reach it either, because removal derives
/// labels from the current config (v3.4.21 cold review, B3).
pub fn legacy_truncated_label(entry: &ScheduleEntry) -> Option<String> {
    if !is_command_mode(entry) {
        return None;
    }
    let raw = raw_discriminator(entry)?;
    if raw.chars().count() <= 40 {
        return None;
    }
    let cmd = entry.command.as_deref().unwrap_or("");
    let basename = Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd);
    let truncated: String = raw.chars().take(40).collect();
    Some(sanitize_label(&format!(
        "{basename}-{}",
        sanitize_label(&truncated)
    )))
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
fn recurring_skill_block(entry: &ScheduleEntry, ctx: &SchedulerContext) -> String {
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
    out.push_str("\n        <string>--harness</string>\n        <string>");
    out.push_str(entry.effective_harness().as_str());
    out.push_str("</string>");
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
pub(crate) fn strip_exe(name: &str) -> &str {
    name.strip_suffix(".exe").unwrap_or(name)
}

/// The basename of a path/command string, with any `.exe` suffix stripped.
/// Returns `None` when the path has no final component.
pub(crate) fn command_basename(cmd: &str) -> Option<&str> {
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
pub(crate) fn command_is_onebrain(cmd: &str, ctx: &SchedulerContext) -> bool {
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
pub(crate) fn args_have_explicit_vault(entry: &ScheduleEntry) -> bool {
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
pub(crate) fn should_append_vault(entry: &ScheduleEntry, ctx: &SchedulerContext) -> bool {
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
fn recurring_command_block(entry: &ScheduleEntry, ctx: &SchedulerContext) -> String {
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

/// Escape a value for safe interpolation inside a double-quoted `/bin/sh -c
/// "..."` argument (Finding #7). Order matters — backslash MUST go first,
/// otherwise the backslashes just added in front of `"` / `$` / `` ` ``
/// would themselves get doubled by a later `\` → `\\` pass:
///
/// 1. `\` → `\\` (escape the escape char itself)
/// 2. `"` → `\"` (would otherwise close the quoted string early)
/// 3. `$` → `\$` (would otherwise trigger parameter/command substitution)
/// 4. `` ` `` → `` \` `` (would otherwise trigger command substitution)
///
/// Applied to EVERY value interpolated into the one-shot blocks' shell
/// string — system/config-derived values (`ctx.vault_path`,
/// `ctx.skill_cli_path`, `entry.skill`, `entry.command`, the derived
/// `plist_file`) AND user-supplied `args:` map keys/values. A value
/// containing `"` would break the shell string; `$` / `` ` `` / `\` would
/// allow injection or corrupt the command launchd actually runs.
///
/// **This is the ONLY layer.** Until v3.4.21 a register-time ban
/// (`sanitize_args_for_one_shot`, plus a `validate_schedulable` scan) refused
/// these characters before they could reach here, and this docstring called
/// itself defense-in-depth. #344 deleted that ban deliberately — a `\` is a
/// path separator on Windows, so refusing it made legitimate configs
/// unregisterable — and there is no net beneath this function any more.
/// Saying otherwise is how the single-layer bug got written in the first
/// place, so it is worth being exact.
///
/// Treat any edit here as a security change:
/// `one_shot_skill_map_key_injection_neutralized_through_real_sh` and
/// `one_shot_command_list_arg_injection_neutralized_through_real_sh` run
/// their payloads through a real `/bin/sh` and assert a sentinel file is
/// never created. Keep both passing.
fn shell_escape_double_quoted(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

/// Build the `<ProgramArguments>` body for a one-shot skill-mode entry —
/// wraps a self-deleting shell command for launchctl bootout + rm.
fn one_shot_skill_block(entry: &ScheduleEntry, ctx: &SchedulerContext, label: &str) -> String {
    let plist_file = format!(
        "{}/Library/LaunchAgents/{label}.plist",
        ctx.homedir.to_string_lossy()
    );
    // BOTH key and value land inside the `/bin/sh -c "..."` wrapper's quoted
    // `--arg="{k}={v}"` fragment, so BOTH must be shell-escaped — escaping
    // only the value (or neither) leaves the key as an injection vector.
    let mut args_flags: String = match &entry.args {
        Some(Args::Map(map)) if !map.is_empty() => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    format!(
                        "--arg=\"{}={}\"",
                        shell_escape_double_quoted(k),
                        shell_escape_double_quoted(v)
                    )
                })
                .collect();
            format!(" {}", parts.join(" "))
        }
        _ => String::new(),
    };
    args_flags.push_str(&format!(
        " --harness=\"{}\"",
        entry.effective_harness().as_str()
    ));
    let shell = format!(
        "\"{}\" skill run --vault=\"{}\" --skill=\"{}\"{}; launchctl bootout gui/{}/{}; rm -f \"{}\"",
        shell_escape_double_quoted(&ctx.skill_cli_path),
        shell_escape_double_quoted(&ctx.vault_path.to_string_lossy()),
        shell_escape_double_quoted(entry.skill.as_deref().unwrap_or("")),
        args_flags,
        ctx.uid,
        label,
        shell_escape_double_quoted(&plist_file),
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
fn one_shot_command_block(entry: &ScheduleEntry, ctx: &SchedulerContext, label: &str) -> String {
    let plist_file = format!(
        "{}/Library/LaunchAgents/{label}.plist",
        ctx.homedir.to_string_lossy()
    );
    // Command-mode list args are escaped HERE, like every other value in
    // this block. They used to be interpolated raw on the theory that the
    // register-time character ban had already rejected anything dangerous —
    // which made this the ONE single-layer path in the file, with its only
    // layer in another crate behind a `cfg`. The v3.4.21 cold review found
    // it, and the injection PoC that was supposed to guard it actually
    // exercised the (already-escaped) skill-mode map-key path, so deleting
    // the ban would not have turned any test red. Escaping at the sink is
    // what makes the ban removable at all (#344).
    let mut parts: Vec<String> = match &entry.args {
        Some(Args::List(argv)) if !argv.is_empty() => argv
            .iter()
            .map(|a| format!("\"{}\"", shell_escape_double_quoted(a)))
            .collect(),
        _ => Vec::new(),
    };
    if should_append_vault(entry, ctx) {
        parts.push("\"--vault\"".to_string());
        parts.push(format!(
            "\"{}\"",
            shell_escape_double_quoted(&ctx.vault_path.to_string_lossy())
        ));
    }
    let quoted_args = if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" "))
    };
    let inner = format!(
        "\"{}\"{}",
        shell_escape_double_quoted(entry.command.as_deref().unwrap_or("")),
        quoted_args
    );
    let shell = format!(
        "{}; launchctl bootout gui/{}/{}; rm -f \"{}\"",
        inner,
        ctx.uid,
        label,
        shell_escape_double_quoted(&plist_file)
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
        let f = at_fields(entry.at.as_deref().unwrap());
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
        let set = cron_fields_expanded(entry.cron.as_deref().unwrap());
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
pub fn generate_plist(
    entry: &ScheduleEntry,
    ctx: &SchedulerContext,
) -> Result<String, crate::scheduler::error::SchedulerError> {
    // Refuse control characters BEFORE rendering. For this sink the reason is
    // XML — `&#1;` is itself illegal, so the character has no representation at
    // all and `plutil` / `launchctl bootstrap` would reject the document with an
    // opaque error naming nothing (#355). The rule itself is not XML's: it lives
    // in `entry` and every renderer applies it, so one config gets one verdict
    // on all three platforms. Fallible for the same reason
    // `generate_service_unit` became fallible in v3.4.21.
    crate::scheduler::entry::reject_control_chars_in_entry(entry)?;

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
    Ok(format!(
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
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn test_ctx() -> SchedulerContext {
        test_ctx_with_vault("/Users/test/vault")
    }

    /// Same as [`test_ctx`] but with an arbitrary vault path — used to
    /// exercise paths containing spaces (#263 motivating scenario).
    fn test_ctx_with_vault(vault: &str) -> SchedulerContext {
        SchedulerContext {
            vault_path: PathBuf::from(vault),
            skill_cli_path: "/opt/homebrew/bin/onebrain".into(),
            log_base_path: PathBuf::from("/Users/test/Library/Logs/onebrain"),
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
        let out = generate_plist(&skill_entry("/daily", "0 9 * * *"), &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
        assert!(out.contains("<string>--arg</string>"));
        assert!(
            out.contains("<string>topic=this-week</string>"),
            "out:\n{out}"
        );
    }

    #[test]
    fn recurring_codex_skill_forwards_harness() {
        let mut entry = skill_entry("/daily", "0 9 * * *");
        entry.harness = Some(crate::Harness::Codex);
        let out = generate_plist(&entry, &test_ctx()).unwrap();
        assert!(out.contains("<string>--harness</string>\n        <string>codex</string>"));
    }

    #[test]
    fn recurring_skill_escapes_xml_special_chars_in_arg_values() {
        let mut e = skill_entry("/echo", "0 9 * * *");
        let mut map = IndexMap::new();
        map.insert("msg".into(), "a & b < c".into());
        e.args = Some(Args::Map(map));
        let out = generate_plist(&e, &test_ctx()).unwrap();
        assert!(
            out.contains("<string>msg=a &amp; b &lt; c</string>"),
            "out:\n{out}"
        );
        // Raw chars must not appear in the output anywhere.
        assert!(!out.contains("msg=a & b"));
    }

    #[test]
    fn recurring_skill_no_blank_line_when_args_absent() {
        let out = generate_plist(&skill_entry("/daily", "0 9 * * *"), &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
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
        let out_bare = generate_plist(&bare, &test_ctx()).unwrap();
        let out_abs = generate_plist(&abs, &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
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
            let out = generate_plist(&e, &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
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
            let out = generate_plist(&e, &test_ctx()).unwrap();
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
        let out = generate_plist(&e, &test_ctx_with_vault("/tmp/My Vault/x")).unwrap();
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
        let out = generate_plist(&e, &test_ctx_with_vault("/tmp/My Vault/x")).unwrap();
        // xml_escape turns the surrounding double-quotes into &quot; — the
        // space stays literal inside them.
        assert!(
            out.contains("&quot;--vault&quot; &quot;/tmp/My Vault/x&quot;"),
            "space-containing vault path must be quoted in the shell wrapper, out:\n{out}"
        );
    }

    // ── Finding #7: shell escaping in one-shot `/bin/sh -c "..."` wrappers ──

    /// Direct proof the escaping is correct: for a battery of strings
    /// containing every character `shell_escape_double_quoted` handles
    /// (backslash, double-quote, `$`, backtick — plus combinations that
    /// would otherwise inject a subshell or corrupt the argument), wrap the
    /// escaped output inside a double-quoted `printf` argument and run it
    /// through a REAL `/bin/sh -c`. If escaping is correct, the shell must
    /// hand back exactly the original, un-escaped string as a single
    /// argument — no injected commands, no truncation, no corruption.
    ///
    /// `#[cfg(unix)]`: shells out to a real `/bin/sh`, which doesn't exist on
    /// Windows. The one-shot `/bin/sh -c` wrapper this guards is itself a
    /// unix/launchd concern; the pure-string escaping logic stays covered on
    /// all platforms by `shell_escape_double_quoted_escapes_all_four_chars_in_order`.
    #[cfg(unix)]
    #[test]
    fn shell_escape_double_quoted_round_trips_through_real_sh() {
        let cases = [
            "plain/path no specials",
            "has a \"quote\" inside",
            "has a $DOLLAR and $(subshell)",
            "has a `backtick` command",
            "has a back\\slash",
            "combo: \"$(rm -rf /)\" `whoami` \\end",
            "/tmp/weird\"$(id)\"vault",
        ];
        for raw in cases {
            let escaped = shell_escape_double_quoted(raw);
            let script = format!("printf '%s' \"{escaped}\"");
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .output()
                .expect("/bin/sh must be available to run this test");
            assert!(
                out.status.success(),
                "sh -c exited non-zero for {raw:?} · script: {script} · stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                raw,
                "round-trip mismatch for {raw:?} · script: {script}"
            );
        }
    }

    #[test]
    fn shell_escape_double_quoted_escapes_all_four_chars_in_order() {
        // Backslash must be escaped FIRST — otherwise the backslashes just
        // added in front of `"` / `$` / `` ` `` would get doubled by a
        // later backslash pass.
        assert_eq!(
            shell_escape_double_quoted(r#"a\b"c$d`e"#),
            r#"a\\b\"c\$d\`e"#
        );
    }

    #[test]
    fn one_shot_skill_vault_path_with_quote_and_dollar_escaped_in_wrapper() {
        // Copilot Finding #7: `ctx.vault_path` is interpolated into the
        // one-shot skill block's `/bin/sh -c "..."` string with NO
        // escaping. A path containing `"` breaks out of the quoted
        // `--vault="..."` segment; `$` allows command/parameter
        // substitution. Real vault paths only ever contain spaces (already
        // handled by the surrounding quotes) — this hardens the exotic-char
        // case.
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            skill: Some("/reminder".into()),
            ..Default::default()
        };
        let vault = "/tmp/weird\"$(id)\"vault";
        let out = generate_plist(&e, &test_ctx_with_vault(vault)).unwrap();

        // Compute the expected escaped-then-xml-escaped fragment using the
        // real helpers so this test can't silently drift from the emitter.
        let shell_fragment = format!("--vault=\"{}\"", shell_escape_double_quoted(vault));
        let expected_in_plist = xml_escape(&shell_fragment);
        assert!(
            out.contains(&expected_in_plist),
            "expected escaped vault path inside the shell wrapper: {expected_in_plist:?}, \
             out:\n{out}"
        );
        // Positive evidence the dollar sign was actually backslash-escaped
        // (not just coincidentally present) — `\$(id)` (backslash directly
        // before `$`) can only appear if escaping ran.
        assert!(
            out.contains("\\$(id)"),
            "expected the `$` in the vault path to be backslash-escaped, out:\n{out}"
        );
    }

    #[test]
    fn one_shot_command_entry_command_with_backtick_and_quote_escaped_in_wrapper() {
        // Same finding, `entry.command` this time (one-shot command-mode
        // block). A command path containing a backtick or quote must not
        // break out of the `"<command>"` segment or trigger command
        // substitution.
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            command: Some("/tmp/weird`whoami`\"cmd".into()),
            args: Some(Args::List(vec!["reindex".into()])),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx()).unwrap();

        let expected_in_plist = xml_escape(&format!(
            "\"{}\"",
            shell_escape_double_quoted("/tmp/weird`whoami`\"cmd")
        ));
        assert!(
            out.contains(&expected_in_plist),
            "expected escaped command inside the shell wrapper: {expected_in_plist:?}, \
             out:\n{out}"
        );
        assert!(
            out.contains("\\`whoami\\`"),
            "expected the backticks in the command to be backslash-escaped, out:\n{out}"
        );
    }

    #[test]
    fn one_shot_skill_plist_file_path_escaped_when_homedir_has_special_chars() {
        // `plist_file` (built from `ctx.homedir` + the sanitized label) is
        // also interpolated into the `rm -f "..."` segment of the shell
        // wrapper. The label itself is alphanumeric/dash-only
        // (`sanitize_label`), but `ctx.homedir` is not sanitized anywhere
        // upstream, so it must be escaped at the point of interpolation too.
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            skill: Some("/reminder".into()),
            ..Default::default()
        };
        let ctx = SchedulerContext {
            vault_path: PathBuf::from("/Users/test/vault"),
            skill_cli_path: "/opt/homebrew/bin/onebrain".into(),
            log_base_path: PathBuf::from("/Users/test/Library/Logs/onebrain"),
            homedir: PathBuf::from("/tmp/weird\"$home"),
            uid: 501,
        };
        let out = generate_plist(&e, &ctx).unwrap();
        let plist_file = "/tmp/weird\"$home/Library/LaunchAgents/com.onebrain.reminder.plist";
        let expected_in_plist = xml_escape(&format!(
            "rm -f \"{}\"",
            shell_escape_double_quoted(plist_file)
        ));
        assert!(
            out.contains(&expected_in_plist),
            "expected escaped plist_file inside the `rm -f` segment: {expected_in_plist:?}, \
             out:\n{out}"
        );
    }

    /// Reverse of [`xml_escape`] — recovers the raw text of a `<string>`
    /// payload. `&amp;` LAST so an already-decoded `&` can't be re-decoded.
    ///
    /// Not gated: `generate_plist` is a pure renderer that compiles and is
    /// tested on every host, so the escaping assertions that read its output
    /// back must run everywhere too. (This was `#[cfg(unix)]` while its only
    /// caller shelled out to `/bin/sh`; a Windows-path test now reads it back
    /// with no shell involved.)
    fn un_xml_escape(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&")
    }

    /// Pull the `/bin/sh -c` payload (the 3rd ProgramArguments `<string>`,
    /// right after `-c`) out of a generated one-shot plist and un-XML-escape
    /// it back to the raw shell command that launchd would actually run.
    ///
    /// Not gated, for the same reason as [`un_xml_escape`]: it only parses a
    /// generated plist string, and the Windows-path escaping test needs it.
    fn extract_one_shot_shell(plist: &str) -> String {
        let marker = "<string>-c</string>";
        let after = &plist[plist.find(marker).expect("no -c marker") + marker.len()..];
        let start = after.find("<string>").expect("no shell <string>") + "<string>".len();
        let rel_end = after[start..]
            .find("</string>")
            .expect("unterminated shell <string>");
        un_xml_escape(&after[start..start + rel_end])
    }

    // `#[cfg(unix)]`: shells out to a real `/bin/sh`, absent on Windows. The
    // one-shot `/bin/sh -c` wrapper it exercises is a unix/launchd construct.
    // The escaping primitive underneath it IS covered platform-independently,
    // by `shell_escape_double_quoted_escapes_all_four_chars_in_order`. (This
    // comment used to point at a test that does not exist and at the
    // register-time validator tests, which #344 deleted.)
    #[cfg(unix)]
    #[test]
    fn one_shot_skill_map_key_injection_neutralized_through_real_sh() {
        // SECURITY PoC (RED→GREEN). The live exploit used a map KEY of
        // `x"; touch <sentinel>; echo "` to break out of the quoted
        // `--arg="{k}={v}"` fragment inside the `/bin/sh -c "..."` wrapper
        // and run an arbitrary command. With key-escaping in place, running
        // the ACTUAL emitted shell string through a real `/bin/sh -c` must
        // NOT create the sentinel — the injected `touch` stays inert text
        // inside the quoted arg. On the pre-fix code the sentinel WAS
        // created (test fails), proving the guard catches the bug.
        let td = tempfile::tempdir().unwrap();
        let sentinel = td.path().join("onebrain_poc_pwned");
        assert!(!sentinel.exists());

        let mut map = IndexMap::new();
        map.insert(
            format!("x\"; touch {}; echo \"", sentinel.display()),
            "harmless".to_string(),
        );
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            skill: Some("/distill".into()),
            args: Some(Args::Map(map)),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx()).unwrap();
        let shell = extract_one_shot_shell(&out);

        // Run the real payload. The `onebrain` / `launchctl` commands inside
        // it are expected to fail (binary absent / no such job) — irrelevant.
        // We only assert the INJECTED `touch` never executes.
        let _ = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&shell)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run /bin/sh");
        assert!(
            !sentinel.exists(),
            "SHELL INJECTION: emitted one-shot payload executed the injected \
             command. shell string was:\n{shell}"
        );
    }

    /// SECURITY PoC for the path the OLD PoC did not cover (v3.4.21 cold
    /// review, B2): command-mode LIST args in a one-shot plist. The previous
    /// test exercised skill-mode map keys — already escaped — so it stayed
    /// green even with the register-time ban removed. This one goes red if
    /// `one_shot_command_block` ever stops escaping list elements.
    ///
    /// `#[cfg(unix)]`: it executes the payload through a real `/bin/sh`,
    /// which Windows does not have. The escaping itself is asserted on every
    /// host by `one_shot_command_list_arg_keeps_backslash_paths_intact`.
    #[cfg(unix)]
    #[test]
    fn one_shot_command_list_arg_injection_neutralized_through_real_sh() {
        let td = tempfile::tempdir().unwrap();
        let sentinel = td.path().join("onebrain_poc_list_arg_pwned");
        assert!(!sentinel.exists());

        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            command: Some("/usr/bin/true".into()),
            args: Some(Args::List(vec![format!(
                "x\"; touch {}; echo \"",
                sentinel.display()
            )])),
            ..Default::default()
        };
        let out = generate_plist(&e, &test_ctx()).unwrap();
        let shell = extract_one_shot_shell(&out);

        let _ = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&shell)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run /bin/sh");
        assert!(
            !sentinel.exists(),
            "SHELL INJECTION via command-mode list arg. shell string was:\n{shell}"
        );
    }

    /// #344: a Windows-style absolute path in a one-shot list arg must
    /// survive escaping intact — the backslashes are data, and the whole
    /// point of escaping at the sink is that the register-time ban can go.
    #[test]
    fn one_shot_command_list_arg_keeps_backslash_paths_intact() {
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            command: Some("/usr/bin/true".into()),
            args: Some(Args::List(vec![r"C:\ob test\out.txt".to_string()])),
            ..Default::default()
        };
        let shell = extract_one_shot_shell(&generate_plist(&e, &test_ctx()).unwrap());
        // Escaped form inside the double-quoted shell string.
        assert!(
            shell.contains(r"C:\\ob test\\out.txt"),
            "path must be present, backslash-escaped: {shell}"
        );
    }

    #[test]
    fn one_shot_command_plist_file_path_escaped_when_homedir_has_special_chars() {
        // Structural companion to the skill-mode plist_file test: the
        // one-shot COMMAND block also interpolates `plist_file` (derived from
        // `ctx.homedir`) into its `rm -f "..."` segment and must escape it
        // identically. `/usr/bin/true` is a generic command (no --vault
        // append) so this isolates the plist_file path.
        let e = ScheduleEntry {
            at: Some("2026-05-13 14:30".into()),
            command: Some("/usr/bin/true".into()),
            ..Default::default()
        };
        let ctx = SchedulerContext {
            vault_path: PathBuf::from("/Users/test/vault"),
            skill_cli_path: "/opt/homebrew/bin/onebrain".into(),
            log_base_path: PathBuf::from("/Users/test/Library/Logs/onebrain"),
            homedir: PathBuf::from("/tmp/weird\"$home"),
            uid: 501,
        };
        let out = generate_plist(&e, &ctx).unwrap();
        let label = format!("com.onebrain.{}", label_for_entry(&e));
        let plist_file = format!("/tmp/weird\"$home/Library/LaunchAgents/{label}.plist");
        let expected_in_plist = xml_escape(&format!(
            "rm -f \"{}\"",
            shell_escape_double_quoted(&plist_file)
        ));
        assert!(
            out.contains(&expected_in_plist),
            "expected escaped plist_file inside the command-mode `rm -f` segment: \
             {expected_in_plist:?}, out:\n{out}"
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
        let out = generate_plist(&e, &test_ctx()).unwrap();
        assert!(out.contains("<string>--exclude</string>"));
        assert!(out.contains("<string>a &amp; b</string>"));
        assert!(!out.contains("<string>a & b</string>"));
    }

    #[test]
    fn generate_plist_snapshot_recurring_skill() {
        let out = generate_plist(&skill_entry("/daily", "0 9 * * *"), &test_ctx()).unwrap();
        insta::assert_snapshot!(out);
    }

    // ── #116 bug 1: step/list/range → array-form StartCalendarInterval ────────

    #[test]
    fn calendar_block_single_combination_stays_plain_dict() {
        // A bare-value/wildcard cron (the common case) must still produce
        // a single `<dict>`, not a one-element `<array>` — byte parity
        // with the existing snapshot depends on this.
        let out = generate_plist(&skill_entry("/daily", "0 9 * * *"), &test_ctx()).unwrap();
        assert!(out.contains("<key>StartCalendarInterval</key>\n    <dict>"));
        assert!(!out.contains("<key>StartCalendarInterval</key>\n    <array>"));
    }

    #[test]
    fn calendar_block_step_hour_emits_array_of_dicts() {
        // `*/2` on hour → 12 combinations → array form, one <dict> per hour.
        let out = generate_plist(&skill_entry("/daily", "0 */2 * * *"), &test_ctx()).unwrap();
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
        let out = generate_plist(&skill_entry("/daily", "0 9 * * 1,3,5"), &test_ctx()).unwrap();
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
        let out = generate_plist(&skill_entry("/daily", "0 9 * * 7"), &test_ctx()).unwrap();
        assert!(out.contains("<key>Weekday</key>\n        <integer>0</integer>"));
        // Single value → stays plain <dict>, not <array>.
        assert!(out.contains("<key>StartCalendarInterval</key>\n    <dict>"));
    }

    #[test]
    fn calendar_block_range_weekday_emits_inclusive_dicts() {
        let out = generate_plist(&skill_entry("/daily", "0 9 * * 1-5"), &test_ctx()).unwrap();
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
        let out = generate_plist(&skill_entry("/daily", "0 */6 * * *"), &test_ctx()).unwrap();
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
        let out = generate_plist(&skill_entry("/daily", "0 9 1,15 * *"), &test_ctx()).unwrap();
        assert!(out.contains("<key>StartCalendarInterval</key>\n    <array>"));
        assert!(out.contains("<key>Day</key>\n            <integer>1</integer>"));
        assert!(out.contains("<key>Day</key>\n            <integer>15</integer>"));
        // No Weekday key at all — day-of-month is the only restricted field.
        assert!(!out.contains("<key>Weekday</key>"));
    }
}
