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
//! validate each entry, build a [`LaunchdContext`] from the current process
//! environment, run collision detection, then write (or `--dry-run` print).

use anyhow::{anyhow, Context, Result};
use onebrain_core::scheduler::{
    self, generate_plist, is_command_mode, is_one_shot, is_skill_mode, label_for_entry, plist_path,
    validate_at, validate_cron, validate_entry, Args, LaunchdContext, ScheduleConfig,
    ScheduleEntry, SchedulerError, SkillFrontmatter,
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
/// frame, and the parent needs the count to distinguish "no schedule
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
        if is_one_shot(entry) {
            let at = entry.at.as_deref().unwrap();
            validate_at(at).map_err(|e| anyhow!("Invalid at \"{at}\": {}", inner_reason(&e)))?;
            sanitize_args_for_one_shot(entry)?;
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

    let ctx = build_launchd_context(&vault)?;
    detect_collisions(&resolved, &ctx)?;

    for entry in &resolved {
        let plist = generate_plist(entry, &ctx);
        let target = plist_path(&label_for_entry(entry), &ctx.homedir);
        if dry_run {
            if !quiet {
                println!("---  {}  ---", target.display());
                println!("{plist}");
            }
            continue;
        }
        // Best-effort: create LaunchAgents dir if missing. launchd refuses to
        // load from a non-existent path, but the parent should exist on any
        // macOS install — failure here implies a misconfigured HOME or an
        // unusual sandbox, so surface the error rather than swallow it.
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("create LaunchAgents directory at {}", parent.display())
            })?;
        }
        // #116 bug 2 introduced an args/cron discriminator into command-mode
        // labels, so a command entry registered before this release has a
        // stale plist sitting at the OLD basename-only path — still on disk
        // AND still loaded in launchd. Clean it up before writing the new
        // plist so a `--refresh` doesn't leave both the old and new jobs
        // firing. Best-effort: never blocks registration on failure.
        cleanup_stale_legacy_plist(entry, &target, &ctx, quiet);
        std::fs::write(&target, &plist)
            .with_context(|| format!("write plist to {}", target.display()))?;
        if !quiet {
            println!("\u{2713} Wrote {}", target.display());
        }
    }

    if !quiet {
        println!("\nRegistered {} schedule entries.", entries.len());
        println!("Use launchctl to load (or restart launchd):");
        for entry in &resolved {
            let target = plist_path(&label_for_entry(entry), &ctx.homedir);
            println!("  launchctl load {}", target.display());
        }
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

fn read_vault_config(vault: &Path) -> Result<ScheduleConfig> {
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

/// Reject shell-special chars in any arg token for one-shot entries — these
/// get embedded inside a `sh -c "..."` wrapper, so unsanitized tokens would
/// allow shell injection.
///
/// **Map KEYS are checked as well as values.** Skill-mode one-shot entries
/// emit each map pair as a `--arg="{k}={v}"` fragment inside the shell
/// string (see `one_shot_skill_block`), so a malicious KEY (e.g.
/// `x"; touch /tmp/pwned; echo "`) breaks out of the quoted fragment exactly
/// like a malicious value would. Earlier this scanned `m.values()` only,
/// leaving keys as an unguarded injection vector.
fn sanitize_args_for_one_shot(entry: &ScheduleEntry) -> Result<()> {
    let tokens: Vec<&str> = match &entry.args {
        Some(Args::List(v)) => v.iter().map(String::as_str).collect(),
        Some(Args::Map(m)) => m
            .iter()
            .flat_map(|(k, v)| [k.as_str(), v.as_str()])
            .collect(),
        None => return Ok(()),
    };
    for t in tokens {
        if has_shell_special(t) {
            return Err(anyhow!(SchedulerError::ShellSpecialInOneShotArg(t.into())));
        }
    }
    Ok(())
}

fn has_shell_special(s: &str) -> bool {
    s.contains('"') || s.contains('$') || s.contains('`') || s.contains('\\')
}

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

    // Reject shell-special chars in recurring skill-mode args — in the KEY
    // as well as the value. One-shot entries are validated upstream by
    // `sanitize_args_for_one_shot` (which also checks keys). Checking the key
    // here keeps skill-mode consistent and defends any future code path that
    // interpolates a recurring entry's map key into a shell context.
    if let Some(Args::Map(m)) = &entry.args {
        for (k, v) in m {
            if has_shell_special(k) || has_shell_special(v) {
                return Err(anyhow!(SchedulerError::ShellSpecialInArg {
                    key: k.clone(),
                    value: v.clone(),
                }));
            }
        }
    }
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

fn build_launchd_context(vault: &Path) -> Result<LaunchdContext> {
    let skill_cli_path = env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "onebrain".to_string());
    let homedir = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    Ok(LaunchdContext {
        vault_path: vault.to_path_buf(),
        skill_cli_path,
        log_base_path: vault.join(resolve_logs_folder(vault)).join("scheduler"),
        homedir,
        uid: current_uid(),
    })
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

#[cfg(not(unix))]
fn current_uid() -> u32 {
    // Mirror Bun's `process.getuid?.() ?? 501` Windows fallback.
    501
}

/// Two entries normalizing to the same plist path conflict. We label them
/// with their `skill:` or `command:` discriminator for the error message.
fn detect_collisions(resolved: &[ScheduleEntry], ctx: &LaunchdContext) -> Result<()> {
    let mut seen: HashMap<PathBuf, ScheduleEntry> = HashMap::new();
    for entry in resolved {
        let target = plist_path(&label_for_entry(entry), &ctx.homedir);
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
            return Err(anyhow!(SchedulerError::Conflict {
                new: new_label,
                existing: existing_label,
                path: target.display().to_string(),
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

/// During registration, remove a stale pre-#116 legacy-labeled plist for a
/// command-mode entry whose NEW (discriminator-bearing) label differs from
/// the legacy one. Without this, `--refresh` would leave the old plist both
/// on disk AND still loaded in launchd, so the same command fires twice
/// (old schedule + new schedule) until the user manually cleans it up.
///
/// Best-effort in both steps — never fails registration:
/// - `launchctl bootout gui/<uid>/<legacy-label>` unloads the running job
///   (ignored if launchd doesn't have it loaded, or `launchctl` itself is
///   unavailable in the test/CI sandbox).
/// - The stale plist FILE is always removed when present, independent of
///   whether bootout succeeded — this is the one step that matters for "does
///   it fire again after the next login", since a file-only bootout failure
///   just means the CURRENTLY loaded instance keeps running until the user's
///   next logout/login cycle (the residual caveat callers should surface).
fn cleanup_stale_legacy_plist(
    entry: &ScheduleEntry,
    new_target: &Path,
    ctx: &LaunchdContext,
    quiet: bool,
) {
    let Some(legacy_label_safe) = legacy_command_label(entry) else {
        return;
    };
    let legacy_label = format!("com.onebrain.{legacy_label_safe}");
    let legacy_target = ctx
        .homedir
        .join("Library/LaunchAgents")
        .join(format!("{legacy_label}.plist"));

    if legacy_target == *new_target || !legacy_target.exists() {
        return;
    }

    // Best-effort unload — ignore failure (job may not be loaded, or
    // `launchctl` may be unavailable in a sandboxed/CI environment).
    let _ = std::process::Command::new("launchctl")
        .arg("bootout")
        .arg(format!("gui/{}", ctx.uid))
        .arg(&legacy_target)
        .output();

    match std::fs::remove_file(&legacy_target) {
        Ok(()) => {
            if !quiet {
                println!(
                    "\u{2713} Removed stale legacy plist {} (superseded by {})",
                    legacy_target.display(),
                    new_target.display()
                );
            }
        }
        Err(e) => {
            if !quiet {
                eprintln!(
                    "⚠ Could not remove stale legacy plist {} — {e}; it may keep firing until \
                     removed\n\
                     💡 delete {} manually",
                    legacy_target.display(),
                    legacy_target.display()
                );
            }
        }
    }
}

fn remove_all(vault: &Path) -> Result<()> {
    let config = read_vault_config(vault)?;
    let homedir = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    for entry in &config.schedule {
        let target = plist_path(&label_for_entry(entry), &homedir);
        if target.exists() {
            std::fs::remove_file(&target)
                .with_context(|| format!("remove {}", target.display()))?;
            println!("\u{2713} Removed {}", target.display());
        }
    }
    Ok(())
}

fn print_status(vault: &Path) -> Result<()> {
    let config = read_vault_config(vault)?;
    let entries = &config.schedule;
    let homedir = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    println!("Registered schedules: {}", entries.len());
    for entry in entries {
        let target = plist_path(&label_for_entry(entry), &homedir);
        let installed = if target.exists() {
            "\u{2713}"
        } else {
            "\u{2717}"
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
    fn has_shell_special_detects_all_four() {
        assert!(has_shell_special("a\"b"));
        assert!(has_shell_special("a$b"));
        assert!(has_shell_special("a`b"));
        assert!(has_shell_special("a\\b"));
        assert!(!has_shell_special("a-b_c.d/e"));
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

    // ── sanitize_args_for_one_shot ────────────────────────────────────────────

    #[test]
    fn sanitize_args_for_one_shot_accepts_clean_list_args() {
        use onebrain_core::scheduler::{Args, ScheduleEntry};
        let entry = ScheduleEntry {
            at: Some("2026-05-13 14:30".to_string()),
            command: Some("/bin/echo".to_string()),
            args: Some(Args::List(vec!["hello".to_string(), "world".to_string()])),
            ..Default::default()
        };
        assert!(sanitize_args_for_one_shot(&entry).is_ok());
    }

    #[test]
    fn sanitize_args_for_one_shot_rejects_shell_special_in_list() {
        use onebrain_core::scheduler::{Args, ScheduleEntry};
        let entry = ScheduleEntry {
            at: Some("2026-05-13 14:30".to_string()),
            command: Some("/bin/echo".to_string()),
            args: Some(Args::List(vec!["$EVIL".to_string()])),
            ..Default::default()
        };
        assert!(sanitize_args_for_one_shot(&entry).is_err());
    }

    #[test]
    fn sanitize_args_for_one_shot_accepts_clean_map_args() {
        use onebrain_core::scheduler::ScheduleEntry;
        // Build the entry via YAML deserialization to avoid a direct
        // `indexmap` dependency in this crate (indexmap lives in onebrain-core).
        let entry: ScheduleEntry = serde_yaml::from_str(
            "at: \"2026-05-13 14:30\"\nskill: /distill\nargs:\n  topic: safe-value\n",
        )
        .unwrap();
        assert!(sanitize_args_for_one_shot(&entry).is_ok());
    }

    #[test]
    fn sanitize_args_for_one_shot_rejects_shell_special_in_map_value() {
        use onebrain_core::scheduler::ScheduleEntry;
        let entry: ScheduleEntry = serde_yaml::from_str(
            "at: \"2026-05-13 14:30\"\nskill: /distill\nargs:\n  topic: \"bad`value\"\n",
        )
        .unwrap();
        assert!(sanitize_args_for_one_shot(&entry).is_err());
    }

    #[test]
    fn sanitize_args_for_one_shot_rejects_shell_special_in_map_key() {
        // Security regression (CVE): a one-shot skill map KEY with
        // shell-special chars is an injection vector because
        // `one_shot_skill_block` emits `--arg="{k}={v}"` inside a
        // `/bin/sh -c "..."` wrapper. The exact live PoC input that broke
        // out and executed `touch /tmp/onebrain_poc_pwned` must now be
        // REJECTED at register time. Previously this scanned values only and
        // passed the entry through.
        use onebrain_core::scheduler::ScheduleEntry;
        let entry: ScheduleEntry = serde_yaml::from_str(
            "at: \"2026-05-13 14:30\"\nskill: /distill\nargs:\n  \
             \"x\\\"; touch /tmp/onebrain_poc_pwned; echo \\\"\": harmless\n",
        )
        .unwrap();
        let err = sanitize_args_for_one_shot(&entry)
            .expect_err("map key with shell-special chars must be rejected");
        assert!(err.to_string().contains("shell-special"), "got: {err}");
    }

    #[test]
    fn sanitize_args_for_one_shot_accepts_no_args() {
        use onebrain_core::scheduler::ScheduleEntry;
        let entry = ScheduleEntry {
            at: Some("2026-05-13 14:30".to_string()),
            command: Some("/bin/true".to_string()),
            args: None,
            ..Default::default()
        };
        assert!(sanitize_args_for_one_shot(&entry).is_ok());
    }

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

    #[test]
    fn validate_schedulable_rejects_shell_special_in_recurring_args() {
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "distill", "schedulable: true");
        // Build via YAML to avoid direct indexmap dependency.
        let entry: ScheduleEntry = serde_yaml::from_str(
            "cron: \"0 9 * * *\"\nskill: /distill\nargs:\n  topic: \"$(evil)\"\n",
        )
        .unwrap();
        let err = validate_schedulable(dir.path(), &entry).unwrap_err();
        assert!(err.to_string().contains("shell-special"), "got: {err}");
    }

    #[test]
    fn validate_schedulable_rejects_shell_special_in_recurring_arg_key() {
        // Companion to the value check above: a map KEY with shell-special
        // chars must also be rejected (was previously unchecked — only
        // values were scanned).
        use onebrain_core::scheduler::ScheduleEntry;
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "distill", "schedulable: true");
        let entry: ScheduleEntry = serde_yaml::from_str(
            "cron: \"0 9 * * *\"\nskill: /distill\nargs:\n  \"bad`key\": safe-value\n",
        )
        .unwrap();
        let err = validate_schedulable(dir.path(), &entry).unwrap_err();
        assert!(err.to_string().contains("shell-special"), "got: {err}");
    }

    // ── detect_collisions ─────────────────────────────────────────────────────

    #[test]
    fn detect_collisions_ok_when_no_duplicates() {
        use onebrain_core::scheduler::{LaunchdContext, ScheduleEntry};
        let dir = tempfile::tempdir().unwrap();
        let ctx = LaunchdContext {
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
        use onebrain_core::scheduler::{LaunchdContext, ScheduleEntry};
        let dir = tempfile::tempdir().unwrap();
        let ctx = LaunchdContext {
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
            err.to_string().contains("normalize to the same plist path"),
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
        use onebrain_core::scheduler::{LaunchdContext, ScheduleEntry};
        let dir = tempfile::tempdir().unwrap();
        let ctx = LaunchdContext {
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
            msg.contains("normalize to the same plist path"),
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
        use onebrain_core::scheduler::{LaunchdContext, ScheduleEntry};
        let dir = tempfile::tempdir().unwrap();
        let ctx = LaunchdContext {
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
    fn run_remove_when_plist_not_on_disk_is_noop() {
        // Exercises the `if target.exists()` false branch in remove_all:
        // plist absent → silently skip, return Ok(()).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "schedule:\n- cron: \"0 9 * * *\"\n  skill: /daily\n",
        )
        .unwrap();
        let result = run(
            Some(dir.path().to_path_buf()),
            false,
            true,
            false,
            None,
            false,
            None,
        );
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

    #[test]
    fn cleanup_stale_legacy_plist_removes_file_when_labels_differ() {
        use onebrain_core::scheduler::{Args, LaunchdContext, ScheduleEntry};
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let legacy = agents_dir.join("com.onebrain.echo.plist");
        std::fs::write(&legacy, "stale").unwrap();

        let ctx = LaunchdContext {
            vault_path: dir.path().to_path_buf(),
            skill_cli_path: "onebrain".to_string(),
            log_base_path: dir.path().join("logs"),
            homedir: dir.path().to_path_buf(),
            uid: 501,
        };
        let entry = ScheduleEntry {
            cron: Some("0 3 * * 0".to_string()),
            command: Some("/bin/echo".to_string()),
            args: Some(Args::List(vec!["hello".to_string()])),
            ..Default::default()
        };
        let new_target = agents_dir.join("com.onebrain.echo-hello.plist");
        cleanup_stale_legacy_plist(&entry, &new_target, &ctx, true);

        assert!(!legacy.exists(), "stale legacy plist must be removed");
    }

    #[test]
    fn cleanup_stale_legacy_plist_noop_when_new_target_equals_legacy() {
        // No discriminator (no args, no distinguishing cron collision risk in
        // this synthetic case) → new label == legacy label → nothing to clean.
        use onebrain_core::scheduler::{LaunchdContext, ScheduleEntry};
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let legacy = agents_dir.join("com.onebrain.true.plist");
        std::fs::write(&legacy, "not actually stale").unwrap();

        let ctx = LaunchdContext {
            vault_path: dir.path().to_path_buf(),
            skill_cli_path: "onebrain".to_string(),
            log_base_path: dir.path().join("logs"),
            homedir: dir.path().to_path_buf(),
            uid: 501,
        };
        // No args and no cron discriminator possible here since command_discriminator
        // falls back to cron when args are absent — use a bare command with a cron
        // that still produces a discriminator UNLESS we bypass by pointing
        // new_target at the same legacy path directly (simulating "no change").
        let entry = ScheduleEntry {
            cron: Some("0 9 * * *".to_string()),
            command: Some("/usr/bin/true".to_string()),
            ..Default::default()
        };
        cleanup_stale_legacy_plist(&entry, &legacy, &ctx, true);

        assert!(
            legacy.exists(),
            "must not remove the plist when new_target IS the legacy path"
        );
    }

    #[test]
    fn cleanup_stale_legacy_plist_noop_when_no_legacy_file_present() {
        use onebrain_core::scheduler::{Args, LaunchdContext, ScheduleEntry};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Library/LaunchAgents")).unwrap();

        let ctx = LaunchdContext {
            vault_path: dir.path().to_path_buf(),
            skill_cli_path: "onebrain".to_string(),
            log_base_path: dir.path().join("logs"),
            homedir: dir.path().to_path_buf(),
            uid: 501,
        };
        let entry = ScheduleEntry {
            cron: Some("0 3 * * 0".to_string()),
            command: Some("/bin/echo".to_string()),
            args: Some(Args::List(vec!["hello".to_string()])),
            ..Default::default()
        };
        let new_target = dir
            .path()
            .join("Library/LaunchAgents/com.onebrain.echo-hello.plist");
        // No legacy file on disk — must be a silent no-op, no panic.
        cleanup_stale_legacy_plist(&entry, &new_target, &ctx, true);
    }
}
