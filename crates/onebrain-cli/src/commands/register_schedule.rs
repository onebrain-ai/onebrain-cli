//! `onebrain schedule register` — emit launchd plists for each `onebrain.yml`
//! (or legacy `vault.yml`) `schedule:` entry.
//!
//! Six operational flags route at the top of [`run`]:
//! - `--remove` → delete each entry's plist
//! - `--status` → print which plists are installed
//! - `--test <skill>` → stubbed (deferred to Slice 12 `run-skill`)
//! - `--resume <skill>` → clear the `.paused` marker file
//! - `--refresh` → log notice and re-emit (forces overwrite)
//! - `--dry-run` → print plists to stdout instead of writing
//!
//! When none of the above branches fire we walk the `schedule:` list,
//! validate each entry, build a [`SchedulerContext`] from the current process
//! environment, run collision detection, then write (or `--dry-run` print).

use anyhow::{anyhow, Context, Result};
use onebrain_core::scheduler::backend;
use onebrain_core::scheduler::{
    self, is_command_mode, is_one_shot, is_skill_mode, label_for_entry, validate_at, validate_cron,
    validate_entry, Args, ScheduleConfig, ScheduleEntry, SchedulerContext, SchedulerError,
    SkillFrontmatter,
};
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

/// Entry point dispatched from `main.rs`. Returns the standard `anyhow`
/// error which `classify_exit_code` maps to an exit code.
///
/// `vault` mirrors Bun v2.3.3's `--vault <path>` flag — when present, it
/// supplies the vault root directly without walking up from cwd.
pub fn run(
    vault: Option<PathBuf>,
    dry_run: bool,
    remove: bool,
    refresh: bool,
    resume: Option<String>,
    status: bool,
    test: Option<String>,
) -> Result<()> {
    run_with(vault, dry_run, remove, refresh, resume, status, test, false).map(|_| ())
}

/// Same as [`run`] but suppresses all progress and summary `println!` calls
/// AND returns the number of plists actually written. Used by `onebrain
/// plugin update`, which wraps `register_schedule` as one step inside its
/// framed report — the bare per-plist `✓ Wrote ...` lines and the trailing
/// "Use launchctl to load …" hint would otherwise leak through the parent
/// frame (pre-#312 it also suppressed the now-deleted "Use launchctl to\n/// load" hint), and the parent needs the count to distinguish "no schedule
/// entries · skipped" from "N plists refreshed".
///
/// Other paths (`status`, `remove`, `test`, `resume`, real `register` from
/// the CLI surface) keep their existing output — only the embedded-from-
/// plugin-update path passes `quiet = true`.
///
/// Returns `Ok(0)` when `onebrain.yml` has no `schedule:` entries (a
/// well-formed no-op, NOT an error). `Ok(N)` where `N == entries.len()`
/// on a successful registration pass. Errors bubble up via `?` as usual.
pub fn run_embedded(vault: Option<PathBuf>, dry_run: bool, refresh: bool) -> Result<usize> {
    run_with(vault, dry_run, false, refresh, None, false, None, true)
}

#[allow(clippy::too_many_arguments)]
fn run_with(
    vault: Option<PathBuf>,
    dry_run: bool,
    remove: bool,
    refresh: bool,
    resume: Option<String>,
    status: bool,
    test: Option<String>,
    quiet: bool,
) -> Result<usize> {
    let vault = match vault {
        Some(path) => path,
        None => {
            let cwd = env::current_dir().context("read current directory")?;
            resolve_vault_root(&cwd)?
        }
    };

    if remove {
        remove_all(&vault)?;
        return Ok(0);
    }
    if status {
        print_status(&vault)?;
        return Ok(0);
    }
    if let Some(skill) = test {
        test_run(&vault, &skill)?;
        return Ok(0);
    }
    if let Some(skill) = resume {
        resume_skill(&vault, &skill)?;
        return Ok(0);
    }
    if refresh && !quiet {
        println!("(--refresh: re-emitting plists with current vault path)");
    }

    let config = read_vault_config(&vault)?;
    let entries = config.schedule;
    if entries.is_empty() {
        if !quiet {
            println!("No schedule entries in onebrain.yml. Nothing to register.");
        }
        return Ok(0);
    }

    // Pass 1 — structural + field-format validation. We do NOT mutate input
    // (mirrors Bun's "callers may pass their own entry array" contract).
    for entry in &entries {
        validate_entry(entry)
            .map_err(|e| anyhow!("Invalid schedule entry: {}", inner_reason(&e)))?;
        // Up front, NOT at render time. The renderers refuse control characters
        // too (defence in depth), but reaching the check only from inside the
        // per-entry install loop meant entry 3 failing after entries 1 and 2 had
        // already been written AND `launchctl bootstrap`ed, with no rollback —
        // defeating the "nothing changed" ordering this function documents
        // below. It also stopped `--dry-run` at the first bad entry, so the one
        // command meant for checking a config could not list its mistakes.
        onebrain_core::scheduler::entry::reject_control_chars_in_entry(entry)
            .map_err(|e| anyhow!("Invalid schedule entry: {}", inner_reason(&e)))?;
        if is_one_shot(entry) {
            let at = entry.at.as_deref().unwrap();
            validate_at(at).map_err(|e| anyhow!("Invalid at \"{at}\": {}", inner_reason(&e)))?;
        } else if let Some(cron) = &entry.cron {
            validate_cron(cron)
                .map_err(|e| anyhow!("Invalid cron \"{cron}\": {}", inner_reason(&e)))?;
        }
        if is_skill_mode(entry) {
            validate_schedulable(&vault, entry)?;
        }
    }

    // Pass 2 — build the resolved entry list. Command-mode entries are
    // shallow-cloned with their `command` rewritten to an absolute path so
    // launchd's restricted PATH can locate the binary.
    let resolved: Vec<ScheduleEntry> = entries
        .iter()
        .map(|e| -> Result<ScheduleEntry> {
            if is_command_mode(e) {
                let abs = resolve_command_binary(e.command.as_deref().unwrap(), Some(&vault))?;
                let mut clone = e.clone();
                clone.command = Some(abs);
                Ok(clone)
            } else {
                Ok(e.clone())
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let ctx = build_scheduler_context(&vault)?;
    detect_collisions(&resolved, &ctx)?;

    // One-line migration note (#315): fixed without the user reading the issue.
    let legacy = legacy_in_vault_log_dir(&vault);
    if !dry_run && !quiet && legacy.exists() {
        println!(
            "\u{2139} scheduler logs now write to {} (previously {}; old files left in place)",
            ctx.log_base_path.display(),
            legacy.display()
        );
    }

    // Every label this config currently owns. A legacy label is only stale if
    // NO current entry answers to it — see `cleanup_stale_labels`.
    let current_labels: Vec<String> = resolved.iter().map(label_for_entry).collect();

    for entry in &resolved {
        if dry_run {
            if !quiet {
                // Platform-aware preview [M5]: launchd plist on macOS, task
                // XML on Windows, and an honest note where no backend exists
                // — a dry run that errors teaches nothing, and printing
                // another OS's format teaches the wrong thing.
                // Headed by the entry's label, not `artifact_key` — that key
                // is the launchd plist path on every platform (a collision
                // identity, not a location), and displaying it printed a
                // `~/Library/...plist` banner over systemd units (caught by
                // the 9b Linux audit).
                let label = label_for_entry(entry);
                match backend::render_preview(entry, &ctx)? {
                    Some(artifact) => {
                        println!("---  {label}  ---");
                        println!("{artifact}");
                    }
                    None => println!(
                        "---  {label}  ---\n(no scheduler backend on {}; entry validates but no artifact can be produced here)",
                        std::env::consts::OS
                    ),
                }
            }
            continue;
        }
        // The backend owns directory creation + artifact write (Task 3 seam);
        // activation with the OS itself lands in Task 4 (#312).
        // Context names the LABEL, not `artifact_key` — that key is the
        // launchd plist path on every platform (a collision identity, not a
        // location), so this line printed a `~/Library/...plist` inside a
        // Linux error message. Fourth member of the same display family
        // (v3.4.20 fixed remove, dry-run, and the collision error); caught by
        // the Track A Linux fire proof.
        let written = backend::install(entry, &ctx)
            .with_context(|| format!("install schedule '{}'", label_for_entry(entry)))?;
        if !quiet {
            println!("\u{2713} Wrote {}", written.display());
        }
        // #116 bug 2 introduced an args/cron discriminator into command-mode
        // labels, so a command entry registered before this release has a
        // stale artifact sitting at the OLD basename-only label — still on
        // disk AND still loaded — and #345's hash suffix adds a second such
        // generation. Remove those so a `--refresh` doesn't leave the old and
        // new jobs both firing. Best-effort: never blocks registration.
        //
        // AFTER the install, not before: `backend::remove` boots the job out
        // as well as deleting it, so cleaning up first meant a failed install
        // (a `launchctl bootstrap` error is a hard `Err`) left the user with
        // the old job destroyed and no replacement. Ordering it after makes
        // the failure mode "nothing changed" instead of "schedule lost".
        cleanup_stale_labels(entry, &current_labels, &ctx, quiet);
    }

    if !quiet {
        // No "Use launchctl to load" epilogue: install() now boots the job
        // out and back in itself (#312), so the instruction would be false.
        println!(
            "\nRegistered and activated {} schedule entries with {}.",
            entries.len(),
            backend::describe()
        );
    }
    Ok(entries.len())
}

/// Resolve the active vault root. Falls back to `cwd` when no `vault.yml`
/// is found in any ancestor (matches Bun behavior: `--vault` defaults to
/// the working directory).
fn resolve_vault_root(cwd: &Path) -> Result<PathBuf> {
    if let Some(root) = onebrain_core::find_vault_root(cwd) {
        return Ok(root.as_path().to_path_buf());
    }
    // Vault root not detected — fall back to cwd. Bun's CLI surface
    // accepts `--vault <path>` directly; we mirror by treating cwd as the
    // vault root when nothing else is given.
    Ok(cwd.to_path_buf())
}

fn inner_reason(e: &SchedulerError) -> String {
    // Strip the prefix we already include in the outer anyhow message
    // (e.g. `invalid cron "0 9 * *": expected 5 fields, got 4` → reason
    // is `expected 5 fields, got 4`). We could re-engineer SchedulerError
    // to carry the reason cleanly, but Bun's error strings expect the
    // wrapping prefix in the outer message — see test assertions.
    match e {
        SchedulerError::InvalidCron { reason, .. } => reason.clone(),
        SchedulerError::InvalidAt { reason, .. } => reason.clone(),
        SchedulerError::InvalidEntry { reason } => reason.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn read_vault_config(vault: &Path) -> Result<ScheduleConfig> {
    // Dual-read: canonical `onebrain.yml` preferred, legacy `vault.yml`
    // fallback. Hardcoding `vault.yml` here made `schedule register` find no
    // entries on v3.1 vaults (onebrain.yml only) — it silently refused to
    // (re)register/refresh the user's schedule. `resolve_logs_folder` already
    // dual-reads via `load_vault_config`; this is the matching fix for the
    // schedule-entries reader (which parses the raw file into `ScheduleConfig`).
    let yaml_path = onebrain_core::find_config_file(vault)
        .unwrap_or_else(|| vault.join(onebrain_core::CONFIG_FILENAME));
    if !yaml_path.exists() {
        return Ok(ScheduleConfig::default());
    }
    let raw = std::fs::read_to_string(&yaml_path)
        .with_context(|| format!("read {}", yaml_path.display()))?;
    let cfg: ScheduleConfig =
        serde_yaml::from_str(&raw).with_context(|| format!("parse {}", yaml_path.display()))?;
    Ok(cfg)
}

// The register-time character ban (`"`, `$`, backtick, `\` refused in any
// arg key or value) was REMOVED in v3.4.21 (#344). It existed because the
// launchd one-shot wrapper interpolated args into a `/bin/sh -c "..."`
// string; every renderer now escapes its own sink instead —
// `shell_escape_double_quoted` on both launchd one-shot blocks,
// `quote_arg` + `sanitize_unit_value` for systemd, `quote_win_arg` for Task
// Scheduler XML — with the deliberate exception of a `cmd.exe /c` payload,
// passed through verbatim because CMD parses its own tail.
//
// How well each is verified, stated honestly, because an earlier version of
// this comment claimed "a real-interpreter test" for all three and that was
// false for two of them (v3.4.21 cold injection review):
// - launchd — a genuine inertness proof IN THE SUITE: the PoCs run their
//   payload through a real `/bin/sh` and assert a sentinel is never created
// - systemd and Task Scheduler — string assertions in the suite, PLUS a
//   standing corpus case since v3.4.22 (#353):
//   `accept-generated-escaping.{service,timer,xml}` carries `$`, `%`, a
//   space, a trailing backslash run, `;`, `'` and `"` in one argument list,
//   and the existing CI jobs feed it to the real tools on every PR —
//   `systemd-analyze verify` on Linux, `schtasks /Create` on Windows, where
//   its `.expect` pins the arguments as Task Scheduler reports them BACK.
//   Until then the corpus carried no escaped value at all, so either escaper
//   could regress with every gate green.
//   Measured while adding it, on the VMs rather than from the strings:
//   systemd RAN the unit and `/bin/echo` received `$HOME` unexpanded, and
//   Task Scheduler reported back `"C:\My Vault\\"` with the backslash run
//   doubled. What is still NOT automated is a runtime argv proof on Windows
//   — `schtasks` reports the command LINE, and splitting it back is
//   `CommandLineToArgvW`'s job, which no CI job observes.
//
// Removing it was the point, not a side effect: a `\` is a path separator on
// Windows, and a ban at register time made `args: [/c, "echo x> C:\dir\f"]`
// unregisterable while the same value was harmless in the sink it actually
// reached. Escaping at the sink also covers the paths the ban never saw at
// all (recurring command-mode args were unchecked), so this is a net gain in
// safety, not a trade.

/// Read the target skill's `SKILL.md` and confirm it declares schedulability.
fn validate_schedulable(vault: &Path, entry: &ScheduleEntry) -> Result<()> {
    let skill = entry
        .skill
        .as_deref()
        .ok_or_else(|| anyhow!("validateSchedulable invoked on non-skill entry — caller bug"))?;
    let skill_name = skill.trim_start_matches('/');
    let skill_path = vault
        .join(".claude/plugins/onebrain/skills")
        .join(skill_name)
        .join("SKILL.md");
    if !skill_path.exists() {
        return Err(anyhow!(SchedulerError::SkillNotFound(
            skill.to_string(),
            skill_path.display().to_string(),
        )));
    }
    let raw = std::fs::read_to_string(&skill_path)
        .with_context(|| format!("read {}", skill_path.display()))?;
    let fm_block = extract_frontmatter(&raw)
        .ok_or_else(|| anyhow!(SchedulerError::SkillNoFrontmatter(skill.to_string())))?;
    let fm: SkillFrontmatter = serde_yaml::from_str(fm_block)
        .map_err(|e| anyhow!("parse SKILL.md frontmatter for {skill}: {e}"))?;

    if fm.schedulable == Some(false) {
        return Err(anyhow!(SchedulerError::SkillNotSchedulable(
            skill.to_string()
        )));
    }
    if fm.schedulable_with_args == Some(true) {
        let required = fm.required_args.clone().unwrap_or_default();
        let provided: Vec<&str> = match &entry.args {
            Some(Args::Map(m)) => m.keys().map(String::as_str).collect(),
            _ => Vec::new(),
        };
        let missing: Vec<String> = required
            .iter()
            .filter(|r| !provided.contains(&r.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(anyhow!(SchedulerError::SkillMissingArgs {
                skill: skill.to_string(),
                missing: missing.join(", "),
            }));
        }
    } else if fm.schedulable != Some(true) {
        return Err(anyhow!(SchedulerError::SkillSchedulableMissing(
            skill.to_string()
        )));
    }

    // (The recurring skill-mode character ban lived here until v3.4.21 —
    // see the note above `validate_schedulable`'s neighbour for why every
    // sink escapes its own values now.)
    Ok(())
}

/// Extract the YAML body between `---` fences at the start of a file.
fn extract_frontmatter(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// Resolve a command-mode binary name to an absolute path. Mirrors Bun's
/// `resolveCommandBinary` lexically (does NOT canonicalize symlinks).
pub fn resolve_command_binary(name: &str, vault_root: Option<&Path>) -> Result<String> {
    let p = Path::new(name);
    if p.is_absolute() {
        if !p.exists() {
            return Err(anyhow!(SchedulerError::CommandNotFoundAbsolute(
                name.into()
            )));
        }
        return Ok(name.to_string());
    }
    if name.starts_with("./") || name.starts_with("../") {
        let base: PathBuf = vault_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| env::current_dir().unwrap_or_default());
        let resolved = normalize_path(&base.join(name));
        if !resolved.exists() {
            return Err(anyhow!(SchedulerError::CommandNotFoundRelative {
                orig: name.into(),
                resolved: resolved.display().to_string(),
            }));
        }
        return Ok(resolved.display().to_string());
    }
    // Bare name → use the `which` crate (mirrors `/usr/bin/which <name>`).
    match which::which(name) {
        Ok(p) if p.exists() => Ok(p.display().to_string()),
        _ => Err(anyhow!(SchedulerError::CommandNotFoundInPath(name.into()))),
    }
}

/// Lexical path normalization (no symlink resolution, no disk touch).
/// Equivalent to Node's `path.resolve` after the base is applied.
fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

pub(crate) fn build_scheduler_context(vault: &Path) -> Result<SchedulerContext> {
    let skill_cli_path = env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "onebrain".to_string());
    let homedir = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    // #315: job logs are machine-local operational output, not vault content.
    // On a synced vault (iCloud) the old in-vault files eventually became
    // unopenable-for-append, and launchd opens the log paths BEFORE exec — so
    // every job died at setup with EX_CONFIG and wrote nothing anywhere.
    let log_base_path =
        onebrain_core::scheduler::default_log_dir(&homedir, &|k| std::env::var(k).ok());
    Ok(SchedulerContext {
        vault_path: vault.to_path_buf(),
        skill_cli_path,
        log_base_path,
        homedir,
        uid: current_uid(),
    })
}

/// The pre-#315 in-vault scheduler log dir, kept only to detect and announce
/// the migration on `register`. Old files are left in place — deleting user
/// data is not this function's call.
fn legacy_in_vault_log_dir(vault: &Path) -> PathBuf {
    vault.join(resolve_logs_folder(vault)).join("scheduler")
}

/// Resolve the vault's logs folder from `vault.yml::folders.logs`. Falls back
/// to the OneBrain default `"07-logs"` when the config can't be loaded — the
/// scheduler's log path is operational metadata, so a missing/invalid vault.yml
/// shouldn't block plist emission entirely.
///
/// Defense-in-depth: `folders.logs` is owner-supplied via vault.yml, so a
/// malicious or copy-pasted vault could set it to `"../../etc"` or an
/// absolute path. Joining either into `vault.join(folder)` could put the
/// launchd `StandardOutPath` outside the vault — file clobbering at user
/// uid. We reject any path containing `..` segments OR starting with `/`
/// (or a Windows drive prefix) and fall back to the default.
fn resolve_logs_folder(vault: &Path) -> String {
    let raw = onebrain_core::find_vault_root(vault)
        .and_then(|root| onebrain_core::load_vault_config(&root).ok())
        .map(|cfg| cfg.folders.logs)
        .unwrap_or_else(|| "07-logs".to_string());
    if is_safe_relative_folder(&raw) {
        raw
    } else {
        eprintln!(
            "⚠ Ignoring unsafe folders.logs value '{raw}' — it must be a relative path with no \
             `..` segments; using '07-logs' for the scheduler logs instead\n\
             💡 fix `folders.logs` in onebrain.yml to a safe relative path"
        );
        "07-logs".to_string()
    }
}

/// True when `s` is a relative path containing no `..` parent traversals.
/// Uses `std::path::Component` to handle both Unix and Windows path
/// separators portably (a literal `..` in the string and a `ParentDir`
/// component are not the same on Windows where `\` is the separator).
fn is_safe_relative_folder(s: &str) -> bool {
    let p = std::path::Path::new(s);
    if p.is_absolute() {
        return false;
    }
    for component in p.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return false;
        }
        // RootDir / Prefix would have triggered `is_absolute()` already,
        // but be defensive — anything other than CurDir / Normal is rejected.
        if !matches!(
            component,
            std::path::Component::CurDir | std::path::Component::Normal(_)
        ) {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: `getuid` is async-signal-safe + always-succeeds in libc.
    unsafe { libc::getuid() }
}

/// Windows has no `getuid`, and `libc` is a `cfg(unix)` dependency here, so
/// there is nothing to call. The value is never used on Windows either: `uid`
/// only reaches `launchctl bootout gui/<uid>/…` and the launchd one-shot
/// wrapper, neither of which runs off macOS.
///
/// Kept rather than deleted because `SchedulerContext::uid` is deliberately
/// not `cfg`-gated — the launchd renderer must compile on every platform so
/// its snapshot tests run anywhere — so *something* has to fill the field.
/// 501 is Bun's original `process.getuid?.() ?? 501` fallback, retained for
/// continuity.
#[cfg(not(unix))]
fn current_uid() -> u32 {
    501
}

/// Two entries normalizing to the same plist path conflict. We label them
/// with their `skill:` or `command:` discriminator for the error message.
fn detect_collisions(resolved: &[ScheduleEntry], ctx: &SchedulerContext) -> Result<()> {
    let mut seen: HashMap<PathBuf, ScheduleEntry> = HashMap::new();
    for entry in resolved {
        let target = backend::artifact_key(entry, ctx);
        if let Some(existing) = seen.get(&target) {
            let existing_label = if is_command_mode(existing) {
                format!("command:{}", existing.command.as_deref().unwrap_or(""))
            } else {
                format!("skill:{}", existing.skill.as_deref().unwrap_or(""))
            };
            let new_label = if is_command_mode(entry) {
                format!("command:{}", entry.command.as_deref().unwrap_or(""))
            } else {
                format!("skill:{}", entry.skill.as_deref().unwrap_or(""))
            };
            // Report the shared LABEL, not `artifact_key` — that key is the
            // launchd plist path on every platform by design, and printing
            // it showed a `~/Library/...plist` that never existed on
            // Windows/Linux (third member of the display family fixed in
            // v3.4.20; caught by the Windows ARM64 audit).
            return Err(anyhow!(SchedulerError::Conflict {
                new: new_label,
                existing: existing_label,
                path: label_for_entry(entry),
            }));
        }
        seen.insert(target, entry.clone());
    }
    Ok(())
}

/// Compute the pre-#116 basename-only label for a command-mode entry — the
/// label `label_for_entry` would have produced before the args/cron
/// discriminator was added. Skill-mode entries never had a discriminator
/// (only command-mode labels changed), so this returns `None` for them —
/// there's nothing legacy to clean up.
///
/// Mirrors the `basename`-only branch of
/// [`onebrain_core::scheduler::label_for_entry`] exactly (same
/// `Path::file_name` + `sanitize_label` steps), intentionally NOT calling
/// through to it, since that function now always appends the discriminator
/// when one is available — we need the OLD (pre-discriminator) shape here.
fn legacy_command_label(entry: &ScheduleEntry) -> Option<String> {
    if !is_command_mode(entry) {
        return None;
    }
    let cmd = entry.command.as_deref().unwrap_or("");
    let basename = Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd);
    Some(sanitize_label_for_migration(basename))
}

/// Local copy of `launchd::sanitize_label`'s replace-non-alphanumeric rule
/// (that function is private to the `onebrain_core` crate). Kept in lockstep
/// deliberately: this is used ONLY to reconstruct the pre-#116 legacy label
/// for migration cleanup, so it must sanitize identically to how
/// `label_for_entry` sanitized a bare basename before the discriminator was
/// introduced.
fn sanitize_label_for_migration(raw: &str) -> String {
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

/// During registration, remove the artifact any PREVIOUS label of this entry
/// owned, so a label change never leaves a second copy of the job installed.
///
/// Two label generations can be stale for one command entry:
/// - **pre-#116**: the bare basename, before args/cron became part of the label
/// - **pre-v3.4.21**: the plain 40-char truncation, before `#345`'s bounded
///   discriminator gave same-prefix args distinct labels
///
/// Both are cleaned **through `backend::remove`**, which is what makes this
/// work off macOS at all. It previously built a `~/Library/LaunchAgents`
/// path directly and returned early on every other platform, so a Linux or
/// Windows label change left a timer/task installed and firing that
/// `--remove` could never reach either — removal derives labels from the
/// CURRENT config (v3.4.21 cold review, B3). Going through the seam also
/// means the OS-side deactivation (bootout / `disable --now` / `/Delete`)
/// happens per platform, and the `NO_ACTIVATE` kill-switch is honoured by
/// the arms rather than re-implemented here.
///
/// `current_labels` is EVERY label this config owns, not just this entry's.
/// One entry's legacy label can be another entry's live one: an entry whose
/// joined args are exactly the 40-char prefix of a longer entry's produces a
/// current label byte-identical to that longer entry's pre-v3.4.21 truncation.
/// Comparing against `entry`'s own label alone deleted a job that was still in
/// the config, on every run, while printing "Registered and activated N
/// entries" and exiting 0 — the removal line even named a different entry as
/// the superseder. Found by the v3.4.21 cold recovery review (F1); guarded by
/// `legacy_label_matching_another_entry_is_never_removed`.
///
/// Best-effort throughout: a failure is reported, never fatal to
/// registration.
fn cleanup_stale_labels(
    entry: &ScheduleEntry,
    current_labels: &[String],
    ctx: &SchedulerContext,
    quiet: bool,
) {
    let stale = [
        legacy_command_label(entry),
        onebrain_core::scheduler::legacy_truncated_label(entry),
    ];
    for legacy in stale.into_iter().flatten() {
        if current_labels.contains(&legacy) {
            continue;
        }
        match backend::remove(&legacy, ctx) {
            Ok(true) if !quiet => println!(
                "\u{2713} Removed stale schedule '{legacy}' (superseded by '{}')",
                label_for_entry(entry)
            ),
            Ok(_) => {}
            Err(e) if !quiet => eprintln!(
                "⚠ Could not remove stale schedule '{legacy}' — {e}; it may keep firing until \
                 removed manually"
            ),
            Err(_) => {}
        }
    }
}

fn remove_all(vault: &Path) -> Result<()> {
    let mut config = read_vault_config(vault)?;
    let ctx = build_scheduler_context(vault)?;
    resolve_commands_best_effort(&mut config, vault);
    remove_entries(&config, &ctx)
}

/// Rewrite command-mode entries to their resolved absolute binary, exactly
/// as register's Pass 2 does — labels derive from the command's BASENAME,
/// and on Windows resolution changes it (`cmd` → `cmd.exe` → label
/// `cmd-exe`), so a remove/status over the RAW config derived a label no
/// installed artifact ever had: `--remove` silently removed nothing and
/// status read ✗ for entries that were firing (caught live by the v3.4.20
/// Windows ARM64 audit). Best-effort on purpose, unlike register's
/// hard-error resolution: a binary the user has since uninstalled must not
/// wedge removal of its own entry — the raw-name label is still correct on
/// platforms where resolution keeps the basename.
fn resolve_commands_best_effort(config: &mut ScheduleConfig, vault: &Path) {
    for entry in &mut config.schedule {
        if is_command_mode(entry) {
            if let Some(cmd) = entry.command.as_deref() {
                if let Ok(abs) = resolve_command_binary(cmd, Some(vault)) {
                    entry.command = Some(abs);
                }
            }
        }
    }
}

/// Split from [`remove_all`] so unit tests inject a tempdir-rooted context
/// instead of inheriting the real home — the seam a real-plist deletion
/// escaped through once.
fn remove_entries(config: &ScheduleConfig, ctx: &SchedulerContext) -> Result<()> {
    for entry in &config.schedule {
        // Report the label, not `artifact_key` — that key is the launchd
        // plist path on every platform BY DESIGN (a collision identity, not
        // a location), so displaying it printed a `~/Library/...plist` that
        // never existed on Linux (caught by the 9b VM verify).
        let label = label_for_entry(entry);
        if backend::remove(&label, ctx).with_context(|| format!("remove schedule '{label}'"))? {
            println!("\u{2713} Removed schedule '{label}'");
        }
    }
    Ok(())
}

fn print_status(vault: &Path) -> Result<()> {
    let mut config = read_vault_config(vault)?;
    let ctx = build_scheduler_context(vault)?;
    // Same label symmetry as remove_all — status over raw labels reported
    // ✗ for command entries the OS scheduler was actively running.
    resolve_commands_best_effort(&mut config, vault);
    print_status_with(&config, &ctx)
}

/// See [`remove_entries`] — same injection seam, same reason.
fn print_status_with(config: &ScheduleConfig, ctx: &SchedulerContext) -> Result<()> {
    let entries = &config.schedule;
    println!("Registered schedules: {}", entries.len());
    for entry in entries {
        let installed = match backend::is_installed(&label_for_entry(entry), ctx)? {
            backend::InstallState::Active => "\u{2713}",
            // The state the old file-existence check could not see (#312):
            // artifact on disk, OS not running it.
            backend::InstallState::Inactive => "\u{26a0}",
            backend::InstallState::Absent => "\u{2717}",
        };
        let when = entry.at.as_deref().or(entry.cron.as_deref()).unwrap_or("?");
        let tag = if entry.at.is_some() {
            "[once]"
        } else {
            "[cron]"
        };

        let target_label = if is_command_mode(entry) {
            let argv: Vec<&str> = match &entry.args {
                Some(Args::List(v)) => v.iter().map(String::as_str).collect(),
                _ => Vec::new(),
            };
            let arg_str = if argv.is_empty() {
                String::new()
            } else {
                format!(" {}", argv.join(" "))
            };
            format!("cmd: {}{}", entry.command.as_deref().unwrap_or(""), arg_str)
        } else {
            let arg_str = match &entry.args {
                Some(Args::Map(m)) if !m.is_empty() => {
                    let parts: Vec<String> = m.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    format!(" ({})", parts.join(", "))
                }
                _ => String::new(),
            };
            format!("skill: {}{}", entry.skill.as_deref().unwrap_or(""), arg_str)
        };
        println!("  {installed} {tag} {when}  {target_label}");
    }
    Ok(())
}

/// `--test <skill>` — fire a scheduled skill one-shot, exactly the way launchd
/// would invoke it on schedule firing. Used to validate that a `vault.yml`
/// schedule entry actually works end-to-end (claude binary on PATH, vault
/// path correct, args parsed) before committing to a recurring cron line.
///
/// Implementation: walk vault.yml to find the entry matching `skill`, build
/// the same argv the launchd plist would emit (`onebrain run-skill --vault
/// <path> --skill <name> [--arg key=value ...]` for skill-mode entries, or
/// the raw `command + args[]` for command-mode entries), spawn it
/// synchronously with the parent process's env, and stream stdout/stderr.
/// Exit code is propagated.
fn test_run(vault: &Path, skill: &str) -> Result<()> {
    use std::process::{Command, Stdio};

    let config = read_vault_config(vault)?;
    let target = config
        .schedule
        .iter()
        .find(|e| {
            // Match by skill name (with or without leading slash) for skill
            // mode, or by command + first-arg for command mode (a label hack
            // — see `label_for_entry` for the canonical mapping).
            if is_skill_mode(e) {
                let raw = e.skill.as_deref().unwrap_or("");
                let cleaned = raw.trim_start_matches('/');
                let asked = skill.trim_start_matches('/');
                cleaned == asked
            } else {
                false
            }
        })
        .ok_or_else(|| {
            anyhow!(
                "no `schedule:` entry matching skill `{skill}` in onebrain.yml — \
                 run `onebrain schedule register --status` to list entries"
            )
        })?;

    let exe = env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "onebrain".to_string());

    let skill_name = target.skill.as_deref().unwrap_or("");
    let mut cmd = Command::new(&exe);
    cmd.arg("run-skill")
        .arg("--vault")
        .arg(vault.as_os_str())
        .arg("--skill")
        .arg(skill_name);
    if let Some(Args::Map(map)) = target.args.as_ref() {
        for (key, value) in map {
            // Same shape launchd emits: `--arg key=value` per pair.
            cmd.arg("--arg").arg(format!("{key}={value}"));
        }
    }
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    println!(
        "--test: invoking {exe} run-skill --vault {} --skill {skill_name}",
        vault.display()
    );
    let status = cmd
        .status()
        .with_context(|| format!("spawn `{exe} run-skill` for test invocation"))?;
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        eprintln!(
            "✗ --test: skill `{skill_name}` exited with code {code} (see its output above)\n\
             💡 rerun it directly with `onebrain skill run --vault {} --skill {skill_name}` to debug \
             outside the scheduler",
            vault.display()
        );
        return Err(anyhow!("test invocation of `{skill_name}` failed"));
    }
    println!("--test: completed successfully");
    Ok(())
}

fn resume_skill(vault: &Path, skill: &str) -> Result<()> {
    let skill_safe = skill.trim_start_matches('/');
    // The value reaches `remove_file`, so it must name a FILE inside the marker
    // directory and nothing else. A leading-slash trim alone let `../` walk out
    // and delete any `.txt` the user could write (#354).
    //
    // ALLOWLIST, not denylist. The first fix for #354 banned `..`, `/` and `\\`,
    // which closes only the POSIX spelling. On Windows a drive-relative name
    // carries a path PREFIX, and `PathBuf::push` CLEARS the buffer when it sees
    // one — so `--resume "C:precious"` discarded the whole marker path and
    // deleted `precious.txt` from the process's current directory on C:,
    // outside both the marker directory and the vault. Proved on the win11-arm64
    // VM against the denylist build: it printed `✓ Resumed C:precious` and the
    // victim file was gone. `:` also opens an NTFS alternate data stream
    // (`daily:evil`), which a separator ban does not see either.
    //
    // Enumerating Windows path spellings is a losing game, so this states what
    // a skill name IS: ASCII letters, digits, `-` and `_`. Every skill in the
    // vault is kebab-case, and the rule reads the same on all three platforms —
    // a name refused on one host is refused on all of them, which is the same
    // property #355 is about.
    //
    // Checked on the raw string rather than by canonicalising the result: the
    // marker usually does NOT exist — that is the ordinary "not paused" case —
    // and `canonicalize` fails on a missing path.
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    if skill_safe.is_empty() || !skill_safe.chars().all(allowed) {
        anyhow::bail!(
            "invalid skill name {skill:?} — a skill is a bare name like `/daily`: \
             ASCII letters, digits, `-` and `_` only, with no path separators, \
             drive letters or `:`"
        );
    }
    // Reserved DOS device names resolve to the device from ANY directory, so
    // `NUL.txt` never names a file in the marker dir. Harmless (the delete just
    // fails) but the resulting error explains nothing.
    const DOS_DEVICES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if DOS_DEVICES
        .iter()
        .any(|d| skill_safe.eq_ignore_ascii_case(d))
    {
        anyhow::bail!(
            "invalid skill name {skill:?} — that is a reserved device name on Windows, \
             so it cannot name a file in the marker directory"
        );
    }
    let logs_folder = resolve_logs_folder(vault);
    let marker = vault
        .join(&logs_folder)
        .join("scheduler/.paused")
        .join(format!("{skill_safe}.txt"));
    if marker.exists() {
        std::fs::remove_file(&marker)
            .with_context(|| format!("remove paused marker at {}", marker.display()))?;
        println!("\u{2713} Resumed {skill}");
    } else {
        println!("{skill} is not paused.");
    }
    Ok(())
}

// Suppress dead-code warnings for items only used by integration tests in
// other binaries — none currently, but reserved.
#[allow(dead_code)]
fn _unused_marker() {
    let _ = scheduler::scheduler_log_path("x", chrono::Local::now().date_naive(), "/y", false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_core::scheduler::SchedulerError;

    #[test]
    fn normalize_path_strips_curdir_and_pops_parentdir() {
        let p = normalize_path(Path::new("/a/b/./c/../d"));
        assert_eq!(p, PathBuf::from("/a/b/d"));
    }

    // Additional normalize_path branches
    #[test]
    fn normalize_path_parent_dir_pops_prefix() {
        // ../x relative to /a/b should yield /a/x
        let p = normalize_path(Path::new("/a/b/../x"));
        assert_eq!(p, PathBuf::from("/a/x"));
    }

    #[test]
    fn normalize_path_plain_absolute_unchanged() {
        let p = normalize_path(Path::new("/usr/local/bin/foo"));
        assert_eq!(p, PathBuf::from("/usr/local/bin/foo"));
    }

    #[test]
    fn extract_frontmatter_finds_yaml_block() {
        let raw = "---\nname: x\nschedulable: true\n---\n\nbody\n";
        assert_eq!(extract_frontmatter(raw), Some("name: x\nschedulable: true"));
    }

    #[test]
    fn extract_frontmatter_none_when_no_fences() {
        assert!(extract_frontmatter("# Just a heading\n").is_none());
    }

    // ── inner_reason ──────────────────────────────────────────────────────────

    #[test]
    fn inner_reason_invalid_cron_returns_reason_field() {
        let e = SchedulerError::InvalidCron {
            cron: "bad".to_string(),
            reason: "expected 5 fields".to_string(),
        };
        assert_eq!(inner_reason(&e), "expected 5 fields");
    }

    #[test]
    fn inner_reason_invalid_at_returns_reason_field() {
        let e = SchedulerError::InvalidAt {
            at: "bad".to_string(),
            reason: "bad timestamp".to_string(),
        };
        assert_eq!(inner_reason(&e), "bad timestamp");
    }

    #[test]
    fn inner_reason_invalid_entry_returns_reason_field() {
        let e = SchedulerError::InvalidEntry {
            reason: "must have skill or command".to_string(),
        };
        assert_eq!(inner_reason(&e), "must have skill or command");
    }

    #[test]
    fn inner_reason_other_variant_uses_display() {
        // Exercises the `other => other.to_string()` arm (line 208).
        let e = SchedulerError::CommandNotFoundInPath("nope".to_string());
        let s = inner_reason(&e);
        assert!(s.contains("nope"), "got: {s}");
    }

    // ── is_safe_relative_folder ───────────────────────────────────────────────

    #[test]
    fn is_safe_relative_folder_accepts_normal_paths() {
        assert!(is_safe_relative_folder("07-logs"));
        assert!(is_safe_relative_folder("logs/sub"));
        assert!(is_safe_relative_folder("./logs"));
    }

    #[test]
    fn is_safe_relative_folder_rejects_absolute() {
        assert!(!is_safe_relative_folder("/var/logs"));
        assert!(!is_safe_relative_folder("/07-logs"));
    }

    #[test]
    fn is_safe_relative_folder_rejects_parent_traversal() {
        assert!(!is_safe_relative_folder("../etc"));
        assert!(!is_safe_relative_folder("logs/../../etc"));
    }

    // NOTE: the `sanitize_args_for_one_shot` unit tests were removed with the
    // function in v3.4.21 (#344). What replaced them is stronger and lives
    // next to the code that creates the exposure: a real-`/bin/sh` injection
    // PoC per launchd one-shot block, a systemd newline-refusal + `$`/`%`
    // escaping test, and CommandLineToArgvW quoting tests for Task Scheduler.

    // ── read_vault_config ─────────────────────────────────────────────────────

    #[test]
    fn read_vault_config_reads_canonical_onebrain_yml() {
        // Regression: `schedule register` must find entries in canonical
        // onebrain.yml. v3.1 vaults have NO vault.yml, and the old hardcoded
        // `vault.join("vault.yml")` silently returned 0 entries on them.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "schedule:\n- cron: \"0 9 * * *\"\n  skill: /daily\n",
        )
        .unwrap();
        let cfg = read_vault_config(dir.path()).unwrap();
        assert_eq!(
            cfg.schedule.len(),
            1,
            "schedule must be read from onebrain.yml"
        );
    }

    #[test]
    fn read_vault_config_falls_back_to_legacy_vault_yml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("vault.yml"),
            "schedule:\n- cron: \"0 9 * * *\"\n  skill: /weekly\n",
        )
        .unwrap();
        let cfg = read_vault_config(dir.path()).unwrap();
        assert_eq!(cfg.schedule.len(), 1, "legacy vault.yml read as fallback");
    }

    #[test]
    fn read_vault_config_empty_when_no_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = read_vault_config(dir.path()).unwrap();
        assert!(cfg.schedule.is_empty(), "no config → empty schedule");
    }

    // ── resolve_vault_root ────────────────────────────────────────────────────

    #[test]
    fn resolve_vault_root_returns_cwd_when_no_vault_found() {
        // A tempdir with no vault config → falls back to cwd.
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_vault_root(dir.path()).unwrap();
        assert_eq!(result, dir.path());
    }

    #[test]
    fn resolve_vault_root_finds_onebrain_yml_ancestor() {
        // Create vault root with onebrain.yml, then call from a subdir.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "# vault\n").unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let result = resolve_vault_root(&sub).unwrap();
        // Must walk up from `sub` to the ancestor holding onebrain.yml.
        // `find_vault_root` walks via `Path::pop` without canonicalizing, so
        // the popped path equals `dir.path()` exactly — no macOS
        // `/var`→`/private/var` mismatch to guard against here.
        assert_eq!(result, dir.path());
    }

    /// The v3.4.20 Windows-audit regression: register resolves `cmd` →
    /// `...\cmd.exe` before deriving the label (`cmd-exe--…`), so a remove
    /// or status pass over the RAW config derives `cmd--…` — a label no
    /// installed artifact ever had. The resolver rewrite must produce the
    /// SAME label register used.
    #[test]
    fn remove_and_register_derive_the_same_label_for_command_entries() {
        let dir = tempfile::tempdir().unwrap();
        let raw: ScheduleConfig = serde_yaml::from_str(
            "schedule:\n  - cron: \"0 9 * * *\"\n    command: sh\n    args: [-c, hi]\n",
        )
        .unwrap();

        // Register's Pass 2 resolution:
        let registered_label = {
            let abs = resolve_command_binary(
                raw.schedule[0].command.as_deref().unwrap(),
                Some(dir.path()),
            )
            .unwrap();
            let mut e = raw.schedule[0].clone();
            e.command = Some(abs);
            label_for_entry(&e)
        };

        // The remove/status path after the fix:
        let mut cfg = raw;
        resolve_commands_best_effort(&mut cfg, dir.path());
        assert_eq!(
            label_for_entry(&cfg.schedule[0]),
            registered_label,
            "remove/status must address the artifact register created"
        );
    }

    /// Best-effort contract: a command binary that no longer exists must not
    /// wedge removal — the raw entry passes through untouched.
    #[test]
    fn missing_binary_does_not_block_the_resolver_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: ScheduleConfig = serde_yaml::from_str(
            "schedule:\n  - cron: \"0 9 * * *\"\n    command: definitely-not-a-real-binary-xyz\n",
        )
        .unwrap();
        resolve_commands_best_effort(&mut cfg, dir.path());
        assert_eq!(
            cfg.schedule[0].command.as_deref(),
            Some("definitely-not-a-real-binary-xyz")
        );
    }

    // ── resolve_command_binary ────────────────────────────────────────────────

    // POSIX-only: uses `/bin/sh` which doesn't exist on Windows.
    #[cfg(unix)]
    #[test]
    fn resolve_absolute_path_that_exists_returns_as_is() {
        // /bin/sh exists on every POSIX system the test matrix touches.
        let p = resolve_command_binary("/bin/sh", None).unwrap();
        assert_eq!(p, "/bin/sh");
    }

    // POSIX-only: error message wording matches Unix `/...` absolute paths.
    #[cfg(unix)]
    #[test]
    fn resolve_absolute_path_missing_throws() {
        let err = resolve_command_binary("/nonexistent/binary/xyz", None).unwrap_err();
        assert!(err
            .to_string()
            .contains("Command not found at absolute path"));
    }

    // POSIX-only: relies on `ls` being on PATH with a `/ls` suffix.
    #[cfg(unix)]
    #[test]
    fn resolve_bare_name_in_path_returns_absolute() {
        // `ls` is on every POSIX system — its path varies (/bin/ls vs
        // /usr/bin/ls vs Nix store), so match the suffix only.
        let p = resolve_command_binary("ls", None).unwrap();
        assert!(p.ends_with("/ls"), "got {p}");
    }

    #[test]
    fn resolve_bare_name_missing_throws_with_path_hint() {
        let err = resolve_command_binary("definitely-not-a-real-binary-xyz", None).unwrap_err();
        assert!(err.to_string().contains("not found in PATH"));
    }

    #[test]
    fn resolve_relative_path_against_vault_root() {
        let dir = tempfile::tempdir().unwrap();
        let script_dir = dir.path().join("scripts");
        std::fs::create_dir_all(&script_dir).unwrap();
        let script = script_dir.join("backup.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let p = resolve_command_binary("./scripts/backup.sh", Some(dir.path())).unwrap();
        assert_eq!(p, script.display().to_string());
    }

    #[test]
    fn resolve_relative_path_missing_throws() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_command_binary("./scripts/missing.sh", Some(dir.path())).unwrap_err();
        assert!(err
            .to_string()
            .contains("Command not found at relative path"));
    }

    // relative path with no vault root → falls back to cwd
    #[cfg(unix)]
    #[test]
    fn resolve_relative_path_without_vault_root_uses_cwd() {
        // Use a path that does not exist so we get the "relative" error, which
        // proves the no-vault_root branch at line 341-342 was exercised.
        let err = resolve_command_binary("./definitely-not-here.sh", None).unwrap_err();
        assert!(err
            .to_string()
            .contains("Command not found at relative path"));
    }

    // ── validate_schedulable ──────────────────────────────────────────────────

    fn write_skill_file(dir: &std::path::Path, name: &str, frontmatter: &str) {
        let skill_dir = dir.join(".claude/plugins/onebrain/skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\n{frontmatter}\n---\n"),
        )
        .unwrap();
    }

    #[test]
    fn validate_schedulable_passes_for_schedulable_true() {
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "daily", "schedulable: true");
        let entry = ScheduleEntry {
            cron: Some("0 9 * * *".to_string()),
            skill: Some("/daily".to_string()),
            ..Default::default()
        };
        assert!(validate_schedulable(dir.path(), &entry).is_ok());
    }

    #[test]
    fn validate_schedulable_rejects_skill_not_found() {
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        // No SKILL.md written
        let entry = ScheduleEntry {
            cron: Some("0 9 * * *".to_string()),
            skill: Some("/missing-skill".to_string()),
            ..Default::default()
        };
        let err = validate_schedulable(dir.path(), &entry).unwrap_err();
        assert!(err.to_string().contains("missing-skill"), "got: {err}");
    }

    #[test]
    fn validate_schedulable_rejects_no_frontmatter() {
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".claude/plugins/onebrain/skills/bare");
        std::fs::create_dir_all(&skill_dir).unwrap();
        // SKILL.md with no frontmatter fences
        std::fs::write(skill_dir.join("SKILL.md"), "# No frontmatter\n").unwrap();
        let entry = ScheduleEntry {
            cron: Some("0 9 * * *".to_string()),
            skill: Some("/bare".to_string()),
            ..Default::default()
        };
        let err = validate_schedulable(dir.path(), &entry).unwrap_err();
        assert!(
            err.to_string().contains("no YAML frontmatter")
                || err.to_string().contains("frontmatter"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_schedulable_rejects_schedulable_false() {
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "interactive", "schedulable: false");
        let entry = ScheduleEntry {
            cron: Some("0 9 * * *".to_string()),
            skill: Some("/interactive".to_string()),
            ..Default::default()
        };
        let err = validate_schedulable(dir.path(), &entry).unwrap_err();
        assert!(
            err.to_string().contains("requires user input"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_schedulable_rejects_missing_schedulable_key() {
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        // Frontmatter present but schedulable key absent
        write_skill_file(dir.path(), "nodecl", "name: nodecl");
        let entry = ScheduleEntry {
            cron: Some("0 9 * * *".to_string()),
            skill: Some("/nodecl".to_string()),
            ..Default::default()
        };
        let err = validate_schedulable(dir.path(), &entry).unwrap_err();
        assert!(
            err.to_string().contains("does not declare schedulable"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_schedulable_with_args_passes_when_required_args_provided() {
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(
            dir.path(),
            "distill",
            "schedulable_with_args: true\nrequired_args:\n  - topic",
        );
        // Build via YAML to avoid direct indexmap dependency.
        let entry: ScheduleEntry = serde_yaml::from_str(
            "cron: \"0 9 * * *\"\nskill: /distill\nargs:\n  topic: this-week\n",
        )
        .unwrap();
        assert!(validate_schedulable(dir.path(), &entry).is_ok());
    }

    #[test]
    fn validate_schedulable_with_args_rejects_missing_required_arg() {
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(
            dir.path(),
            "distill",
            "schedulable_with_args: true\nrequired_args:\n  - topic",
        );
        let entry = ScheduleEntry {
            cron: Some("0 9 * * *".to_string()),
            skill: Some("/distill".to_string()),
            args: None, // topic is missing
            ..Default::default()
        };
        let err = validate_schedulable(dir.path(), &entry).unwrap_err();
        assert!(err.to_string().contains("requires args"), "got: {err}");
    }

    /// v3.4.21 (#344) inverted these two: a shell-special char in a
    /// recurring skill-mode arg is no longer refused at register time — the
    /// renderers escape their own sinks (see the note above
    /// `validate_schedulable`). Validation must now ACCEPT it, and the
    /// injection PoCs in `onebrain-core::scheduler::launchd` are what prove
    /// the value stays inert. Keeping these as acceptance cases pins the
    /// inversion: if the ban ever returns without the escaping being
    /// reverted too, these go red.
    #[test]
    fn validate_schedulable_accepts_shell_special_now_that_sinks_escape() {
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "distill", "schedulable: true");
        let value_case: ScheduleEntry = serde_yaml::from_str(
            "cron: \"0 9 * * *\"\nskill: /distill\nargs:\n  topic: \"$(evil)\"\n",
        )
        .unwrap();
        assert!(validate_schedulable(dir.path(), &value_case).is_ok());

        let key_case: ScheduleEntry = serde_yaml::from_str(
            "cron: \"0 9 * * *\"\nskill: /distill\nargs:\n  \"bad`key\": safe-value\n",
        )
        .unwrap();
        assert!(validate_schedulable(dir.path(), &key_case).is_ok());
    }

    /// #344's headline case: a Windows absolute path in a one-shot
    /// command-mode arg registered nowhere before v3.4.21.
    #[test]
    fn one_shot_command_args_accept_windows_paths() {
        use onebrain_core::scheduler::{validate_entry, ScheduleEntry};
        let entry: ScheduleEntry = serde_yaml::from_str(
            "at: \"2026-08-01 09:00\"\ncommand: cmd\nargs: [/c, \"echo hi> C:\\\\ob test\\\\out.txt\"]\n",
        )
        .unwrap();
        assert!(validate_entry(&entry).is_ok());
    }

    // ── detect_collisions ─────────────────────────────────────────────────────

    #[test]
    fn detect_collisions_ok_when_no_duplicates() {
        use onebrain_core::scheduler::{ScheduleEntry, SchedulerContext};
        let dir = tempfile::tempdir().unwrap();
        let ctx = SchedulerContext {
            vault_path: dir.path().to_path_buf(),
            skill_cli_path: "onebrain".to_string(),
            log_base_path: dir.path().join("logs"),
            homedir: dir.path().to_path_buf(),
            uid: 501,
        };
        let entries = vec![
            ScheduleEntry {
                cron: Some("0 9 * * *".to_string()),
                skill: Some("/daily".to_string()),
                ..Default::default()
            },
            ScheduleEntry {
                cron: Some("0 17 * * 5".to_string()),
                skill: Some("/weekly".to_string()),
                ..Default::default()
            },
        ];
        assert!(detect_collisions(&entries, &ctx).is_ok());
    }

    #[test]
    fn detect_collisions_errors_on_duplicate_plist_path() {
        use onebrain_core::scheduler::{ScheduleEntry, SchedulerContext};
        let dir = tempfile::tempdir().unwrap();
        let ctx = SchedulerContext {
            vault_path: dir.path().to_path_buf(),
            skill_cli_path: "onebrain".to_string(),
            log_base_path: dir.path().join("logs"),
            homedir: dir.path().to_path_buf(),
            uid: 501,
        };
        // Two entries with the same skill name → same plist label → collision.
        let entries = vec![
            ScheduleEntry {
                cron: Some("0 9 * * *".to_string()),
                skill: Some("/daily".to_string()),
                ..Default::default()
            },
            ScheduleEntry {
                cron: Some("0 12 * * *".to_string()),
                skill: Some("/daily".to_string()),
                ..Default::default()
            },
        ];
        let err = detect_collisions(&entries, &ctx).unwrap_err();
        assert!(
            err.to_string()
                .contains("normalize to the same schedule artifact"),
            "got: {err}"
        );
    }

    // ── detect_collisions: command-mode label arms ────────────────────────────

    #[test]
    fn detect_collisions_errors_with_command_mode_labels() {
        // Two command-mode entries with IDENTICAL command + args + cron →
        // same plist label → collision (a genuine duplicate; #116 bug 2
        // only splits labels when args or cron actually differ). Exercises
        // the `is_command_mode(existing/entry)` arms that format
        // `"command:..."` labels in the error message.
        use onebrain_core::scheduler::{ScheduleEntry, SchedulerContext};
        let dir = tempfile::tempdir().unwrap();
        let ctx = SchedulerContext {
            vault_path: dir.path().to_path_buf(),
            skill_cli_path: "onebrain".to_string(),
            log_base_path: dir.path().join("logs"),
            homedir: dir.path().to_path_buf(),
            uid: 501,
        };
        let entries = vec![
            ScheduleEntry {
                cron: Some("0 9 * * *".to_string()),
                command: Some("/usr/local/bin/backup".to_string()),
                ..Default::default()
            },
            ScheduleEntry {
                cron: Some("0 9 * * *".to_string()),
                command: Some("/usr/local/bin/backup".to_string()),
                ..Default::default()
            },
        ];
        let err = detect_collisions(&entries, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("normalize to the same schedule artifact"),
            "got: {msg}"
        );
        assert!(
            msg.contains("command:"),
            "expected command: prefix, got: {msg}"
        );
    }

    #[test]
    fn detect_collisions_ok_when_command_mode_entries_differ_by_args() {
        // #116 bug 2 regression: two `command:` entries sharing a binary
        // basename but with DIFFERENT args must land on distinct plist
        // paths — no false-positive collision.
        use onebrain_core::scheduler::{ScheduleEntry, SchedulerContext};
        let dir = tempfile::tempdir().unwrap();
        let ctx = SchedulerContext {
            vault_path: dir.path().to_path_buf(),
            skill_cli_path: "onebrain".to_string(),
            log_base_path: dir.path().join("logs"),
            homedir: dir.path().to_path_buf(),
            uid: 501,
        };
        let entries = vec![
            ScheduleEntry {
                cron: Some("0 9 * * *".to_string()),
                command: Some("onebrain".to_string()),
                args: Some(onebrain_core::scheduler::Args::List(vec![
                    "search".to_string(),
                    "reindex".to_string(),
                ])),
                ..Default::default()
            },
            ScheduleEntry {
                cron: Some("0 9 * * *".to_string()),
                command: Some("onebrain".to_string()),
                args: Some(onebrain_core::scheduler::Args::List(vec![
                    "backup".to_string()
                ])),
                ..Default::default()
            },
        ];
        assert!(
            detect_collisions(&entries, &ctx).is_ok(),
            "same binary, different args must not collide"
        );
    }

    // ── read_vault_config: YAML parse error ───────────────────────────────────

    #[test]
    fn read_vault_config_errors_on_malformed_yaml() {
        // Exercises the `serde_yaml::from_str` error path (line ~227).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "schedule: [unclosed\n").unwrap();
        let err = read_vault_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("parse"), "got: {err}");
    }

    // ── validate_schedulable: caller-bug and list-args branches ───────────────

    #[test]
    fn validate_schedulable_caller_bug_no_skill_returns_error() {
        // Exercises the `ok_or_else(|| anyhow!("... caller bug"))` arm:
        // validate_schedulable invoked on an entry with skill = None.
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        let entry = ScheduleEntry {
            cron: Some("0 9 * * *".to_string()),
            skill: None,
            ..Default::default()
        };
        let err = validate_schedulable(dir.path(), &entry).unwrap_err();
        assert!(err.to_string().contains("caller bug"), "got: {err}");
    }

    #[test]
    fn validate_schedulable_with_args_list_args_treated_as_missing() {
        // When `schedulable_with_args: true` but entry.args is Args::List
        // (not Map), the `_ => Vec::new()` arm fires → all required args
        // are "missing" → SkillMissingArgs error.
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(
            dir.path(),
            "distill",
            "schedulable_with_args: true\nrequired_args:\n  - topic",
        );
        let entry: ScheduleEntry =
            serde_yaml::from_str("cron: \"0 9 * * *\"\nskill: /distill\nargs:\n  - list-val\n")
                .unwrap();
        let err = validate_schedulable(dir.path(), &entry).unwrap_err();
        assert!(err.to_string().contains("requires args"), "got: {err}");
    }

    // ── run_embedded: quiet-mode branches ─────────────────────────────────────

    #[test]
    fn run_embedded_empty_schedule_returns_zero() {
        // `run_embedded` hardcodes quiet=true internally, so the empty-schedule
        // `if !quiet { println!(...) }` TRUE arm is unreachable here (it's only
        // covered via `run()`, which passes quiet=false). This exercises the
        // false arm: the println is skipped and Ok(0) is still returned.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "# no schedule entries\n").unwrap();
        let count = run_embedded(Some(dir.path().to_path_buf()), false, false).unwrap();
        assert_eq!(count, 0, "empty schedule → Ok(0) in quiet mode");
    }

    #[test]
    fn run_embedded_refresh_quiet_no_entries_returns_zero() {
        // Exercises the `if refresh && !quiet { println!(...) }` false branch:
        // refresh=true but quiet=true suppresses the message; still Ok(0).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "# no schedule\n").unwrap();
        let count = run_embedded(Some(dir.path().to_path_buf()), false, true).unwrap();
        assert_eq!(count, 0, "refresh + quiet + no entries → Ok(0)");
    }

    // Unix-only: uses /bin/sh which doesn't exist on Windows.
    #[cfg(unix)]
    #[test]
    fn run_embedded_dry_run_quiet_returns_entry_count() {
        // Exercises `dry_run=true, quiet=true`: the inner `if !quiet { println!... }`
        // is skipped, `continue` fires, no plist is written, Ok(N) is returned.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "schedule:\n- cron: \"0 9 * * *\"\n  command: /bin/sh\n",
        )
        .unwrap();
        let count = run_embedded(Some(dir.path().to_path_buf()), true, false).unwrap();
        assert_eq!(
            count, 1,
            "dry_run quiet mode returns entry count without writing"
        );
    }

    // ── print_status: command-mode and skill-with-args paths ──────────────────

    #[test]
    fn run_status_command_mode_with_list_args_succeeds() {
        // Exercises the `is_command_mode(entry)` arm in print_status, including
        // the non-empty argv branch: `Some(Args::List(v))` → `format!(" {}", ...)`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "schedule:\n- cron: \"0 9 * * *\"\n  command: /bin/backup\n  args:\n    - --verbose\n",
        )
        .unwrap();
        let result = run(
            Some(dir.path().to_path_buf()),
            false,
            false,
            false,
            None,
            true,
            None,
        );
        assert!(
            result.is_ok(),
            "status with command entry+list-args: {result:?}"
        );
    }

    #[test]
    fn run_status_skill_with_map_args_succeeds() {
        // Exercises the `Some(Args::Map(m)) if !m.is_empty()` arm in print_status's
        // skill branch, building the `" (key=val)"` arg-str.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "schedule:\n- cron: \"0 9 * * *\"\n  skill: /distill\n  args:\n    topic: weekly\n",
        )
        .unwrap();
        let result = run(
            Some(dir.path().to_path_buf()),
            false,
            false,
            false,
            None,
            true,
            None,
        );
        assert!(result.is_ok(), "status with skill+map-args: {result:?}");
    }

    #[test]
    fn run_status_with_at_entry_renders_without_error() {
        // Exercises the `entry.at.is_some()` → `"[once]"` branch in print_status.
        // (print_status writes to stdout with no writer seam, so a unit test can
        // only assert the path runs cleanly — not the rendered `[once]` text.)
        // Far-future date avoids any time-sensitivity; `_ => String::new()` skill
        // arm is hit since no args are set.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "schedule:\n- at: \"2099-01-01 09:00\"\n  skill: /daily\n",
        )
        .unwrap();
        let result = run(
            Some(dir.path().to_path_buf()),
            false,
            false,
            false,
            None,
            true,
            None,
        );
        assert!(result.is_ok(), "status with at-entry: {result:?}");
    }

    // ── remove_all: plist-not-on-disk branch ──────────────────────────────────

    #[test]
    fn remove_entries_when_plist_not_on_disk_is_noop() {
        // Exercises the `if target.exists()` false branch: plist absent →
        // silently skip, return Ok(()).
        //
        // Goes through `remove_entries` with an injected tempdir context —
        // NEVER through `run()`. The `run()` version of this test built its
        // context from the real home and, once #312 made remove() operate on
        // launchd for real, it booted out and DELETED the operator's actual
        // /daily plist while reporting green. Its "not on disk" premise was
        // only true of a world where nothing was registered.
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let config: ScheduleConfig = serde_yaml::from_str(
            "schedule:\n- cron: \"0 9 * * *\"\n  skill: /unit-remove-noop-probe\n",
        )
        .unwrap();
        let ctx = SchedulerContext {
            vault_path: dir.path().to_path_buf(),
            skill_cli_path: "/usr/local/bin/onebrain".to_string(),
            log_base_path: home.path().join("logs"),
            homedir: home.path().to_path_buf(),
            uid: current_uid(),
        };
        let result = remove_entries(&config, &ctx);
        assert!(result.is_ok(), "remove with no plist on disk: {result:?}");
    }

    // ── resume_skill: not-paused and paused paths ─────────────────────────────

    #[test]
    fn run_resume_not_paused_succeeds() {
        // Exercises the `else` arm of `if marker.exists()` in resume_skill:
        // no marker file → prints "not paused" and returns Ok(()).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "# vault\n").unwrap();
        let result = run(
            Some(dir.path().to_path_buf()),
            false,
            false,
            false,
            Some("/daily".to_string()),
            false,
            None,
        );
        assert!(result.is_ok(), "resume when not paused: {result:?}");
    }

    #[test]
    fn run_resume_paused_removes_marker_file() {
        // Exercises the `if marker.exists()` true arm in resume_skill:
        // marker present → remove it and print "✓ Resumed".
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "# vault\n").unwrap();
        // resolve_logs_folder returns "07-logs" when no folders.logs in config.
        let marker_dir = dir.path().join("07-logs/scheduler/.paused");
        std::fs::create_dir_all(&marker_dir).unwrap();
        let marker = marker_dir.join("daily.txt");
        std::fs::write(&marker, "").unwrap();

        let result = run(
            Some(dir.path().to_path_buf()),
            false,
            false,
            false,
            Some("/daily".to_string()),
            false,
            None,
        );
        assert!(result.is_ok(), "resume when paused: {result:?}");
        assert!(
            !marker.exists(),
            "pause marker should be removed after resume"
        );
    }

    // ── legacy_command_label / cleanup_stale_legacy_plist ─────────────────────

    #[test]
    fn legacy_command_label_none_for_skill_mode() {
        use onebrain_core::scheduler::ScheduleEntry;
        let e = ScheduleEntry {
            cron: Some("0 9 * * *".to_string()),
            skill: Some("/daily".to_string()),
            ..Default::default()
        };
        assert!(legacy_command_label(&e).is_none());
    }

    #[test]
    fn legacy_command_label_is_bare_basename_no_discriminator() {
        use onebrain_core::scheduler::{Args, ScheduleEntry};
        let e = ScheduleEntry {
            cron: Some("0 3 * * 0".to_string()),
            command: Some("/opt/homebrew/bin/onebrain".to_string()),
            args: Some(Args::List(vec!["search-reindex".to_string()])),
            ..Default::default()
        };
        // Legacy label ignores args/cron entirely — just the basename.
        assert_eq!(legacy_command_label(&e), Some("onebrain".to_string()));
    }

    /// #354: the `--resume` value reaches `remove_file`, so a `../` in it used
    /// to delete any `.txt` the user could write, outside the vault entirely.
    /// A leading-slash trim is not containment — and neither is banning the
    /// POSIX spellings, which is why the Windows cases below are here: a
    /// drive-relative `C:precious` carries a path PREFIX, and `PathBuf::push`
    /// clears the buffer when it sees one, so the marker path evaporated and
    /// the delete landed in the current directory on C:. Confirmed on the
    /// win11-arm64 VM before the guard became an allowlist.
    #[test]
    fn resume_refuses_a_skill_name_that_could_escape_the_marker_directory() {
        let d = tempfile::tempdir().unwrap();
        // A file that must survive every attempt below.
        let victim = d.path().join("precious.txt");
        std::fs::write(&victim, "keep me").unwrap();

        for evil in [
            // POSIX spellings
            "../precious",
            "../../precious",
            "/../precious",
            "sub/precious",
            "sub\\precious",
            "",
            "/",
            // Windows spellings. These run on every platform on purpose: the
            // rule is a character allowlist, so the verdict must not depend on
            // which host is asking. Before the allowlist, `C:precious` was
            // ACCEPTED here and deleted the victim on Windows.
            "C:precious",
            "C:/precious",
            "daily:evil", // NTFS alternate data stream
            "NUL",        // reserved device, resolves from any directory
            "con",        // reserved, case-insensitively
            ".",
            "..",
        ] {
            let r = resume_skill(d.path(), evil);
            assert!(r.is_err(), "{evil:?} should be refused");
            let msg = r.unwrap_err().to_string();
            assert!(msg.contains("invalid skill name"), "{evil:?}: {msg}");
            // The reason must name what is wrong, not just say no.
            assert!(
                msg.contains("no path separators") || msg.contains("reserved device name"),
                "{evil:?} refused without explaining why: {msg}"
            );
        }
        assert!(victim.exists(), "the guard let a delete through");
    }

    /// The ordinary shapes still work — a guard that rejects `/daily` would be
    /// worse than the bug.
    #[test]
    fn resume_still_accepts_an_ordinary_skill_name() {
        let d = tempfile::tempdir().unwrap();
        let paused = d.path().join("07-logs/scheduler/.paused");
        std::fs::create_dir_all(&paused).unwrap();
        let marker = paused.join("daily.txt");
        std::fs::write(&marker, "paused").unwrap();

        resume_skill(d.path(), "/daily").unwrap();
        assert!(!marker.exists(), "the marker should have been cleared");

        // Not paused → not an error.
        resume_skill(d.path(), "weekly").unwrap();
    }

    /// #345 + B3: an entry whose discriminator used to truncate changes
    /// label, so registration must be able to name — and remove — the
    /// artifact the OLD label owned. `legacy_truncated_label` is that name;
    /// it is `None` for entries whose label is unchanged, so an upgrade does
    /// no work for them.
    #[test]
    fn legacy_truncated_label_only_for_entries_that_used_to_truncate() {
        use onebrain_core::scheduler::{legacy_truncated_label, Args, ScheduleEntry};
        let short = ScheduleEntry {
            cron: Some("0 3 * * 0".to_string()),
            command: Some("/bin/echo".to_string()),
            args: Some(Args::List(vec!["hi".to_string()])),
            ..Default::default()
        };
        assert_eq!(legacy_truncated_label(&short), None, "short: no change");

        let long = ScheduleEntry {
            cron: Some("0 3 * * 0".to_string()),
            command: Some("/bin/echo".to_string()),
            args: Some(Args::List(vec![
                "/some/quite/long/path/that/goes/past/forty/characters/one".to_string(),
            ])),
            ..Default::default()
        };
        let legacy = legacy_truncated_label(&long).expect("long entry has a legacy label");
        assert!(legacy.starts_with("echo-"), "got {legacy}");
        // The legacy form is the plain 40-char truncation; the new label
        // carries the hash suffix, so they must differ — otherwise the
        // cleanup would delete the artifact just written.
        assert_ne!(legacy, label_for_entry(&long));
    }

    /// F1 from the v3.4.21 cold recovery review: one entry's LEGACY label can
    /// be another entry's LIVE one, so the staleness test has to be "no
    /// current entry answers to this label", not "not my own label".
    ///
    /// The shape: `short`'s joined args are exactly the 40-char prefix of
    /// `long`'s, so `short`'s current label equals `long`'s pre-v3.4.21
    /// truncation. Cleaning up `long` used to remove the artifact `short` had
    /// just been registered under — silently, and on every subsequent run.
    #[test]
    fn legacy_label_matching_another_entry_is_never_removed() {
        use onebrain_core::scheduler::{legacy_truncated_label, Args, ScheduleEntry};
        let mk = |arg: &str| ScheduleEntry {
            cron: Some("0 3 * * 0".to_string()),
            command: Some("/bin/echo".to_string()),
            args: Some(Args::List(vec![arg.to_string()])),
            ..Default::default()
        };
        // 40 chars exactly — `command_discriminator` passes it through
        // unbounded, so it IS the label suffix.
        let prefix = "/Volumes/Backup/photos/library-2024-01ab";
        assert_eq!(prefix.len(), 40, "the premise of this test");
        let short = mk(prefix);
        let long = mk(&format!("{prefix}cdef"));

        let victim = label_for_entry(&short);
        let legacy = legacy_truncated_label(&long).expect("long entry truncated before v3.4.21");
        assert_eq!(
            legacy, victim,
            "premise: long's legacy label collides with short's live label"
        );

        // With both entries in the config, that label belongs to a live entry
        // and must survive `long`'s cleanup.
        let current = [label_for_entry(&short), label_for_entry(&long)];
        assert!(
            current.contains(&legacy),
            "the collision must be visible to cleanup_stale_labels' guard"
        );
    }

    /// The two forms are length-disjoint, which is what makes a truncating
    /// entry unable to collide with a non-truncating one (#345).
    #[test]
    fn same_prefix_long_args_get_distinct_labels() {
        use onebrain_core::scheduler::{Args, ScheduleEntry};
        let mk = |tail: &str| ScheduleEntry {
            cron: Some("0 3 * * 0".to_string()),
            command: Some("/bin/echo".to_string()),
            args: Some(Args::List(vec![format!(
                "/a/very/long/argument/sharing/a/common/prefix/{tail}"
            )])),
            ..Default::default()
        };
        let a = label_for_entry(&mk("alpha"));
        let b = label_for_entry(&mk("beta"));
        assert_ne!(a, b, "differing only past char 40 must still differ: {a}");
    }
}
