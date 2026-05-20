//! `onebrain register-schedule` — emit launchd plists for each `vault.yml`
//! `schedule:` entry. Mirrors Bun `src/commands/register-schedule.ts`.
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
    let vault = match vault {
        Some(path) => path,
        None => {
            let cwd = env::current_dir().context("read current directory")?;
            resolve_vault_root(&cwd)?
        }
    };

    if remove {
        return remove_all(&vault);
    }
    if status {
        return print_status(&vault);
    }
    if let Some(skill) = test {
        return test_run(&vault, &skill);
    }
    if let Some(skill) = resume {
        return resume_skill(&vault, &skill);
    }
    if refresh {
        println!("(--refresh: re-emitting plists with current vault path)");
    }

    let config = read_vault_config(&vault)?;
    let entries = config.schedule;
    if entries.is_empty() {
        println!("No schedule entries in vault.yml. Nothing to register.");
        return Ok(());
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
            println!("---  {}  ---", target.display());
            println!("{plist}");
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
        std::fs::write(&target, &plist)
            .with_context(|| format!("write plist to {}", target.display()))?;
        println!("\u{2713} Wrote {}", target.display());
    }

    println!("\nRegistered {} schedule entries.", entries.len());
    println!("Use launchctl to load (or restart launchd):");
    for entry in &resolved {
        let target = plist_path(&label_for_entry(entry), &ctx.homedir);
        println!("  launchctl load {}", target.display());
    }
    Ok(())
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
    let yaml_path = vault.join("vault.yml");
    if !yaml_path.exists() {
        return Ok(ScheduleConfig::default());
    }
    let raw = std::fs::read_to_string(&yaml_path)
        .with_context(|| format!("read {}", yaml_path.display()))?;
    let cfg: ScheduleConfig =
        serde_yaml::from_str(&raw).with_context(|| format!("parse {}", yaml_path.display()))?;
    Ok(cfg)
}

/// Reject shell-special chars in any arg value for one-shot entries —
/// these get embedded inside a `sh -c "..."` wrapper, so unsanitized values
/// would allow shell injection.
fn sanitize_args_for_one_shot(entry: &ScheduleEntry) -> Result<()> {
    let values: Vec<&str> = match &entry.args {
        Some(Args::List(v)) => v.iter().map(String::as_str).collect(),
        Some(Args::Map(m)) => m.values().map(String::as_str).collect(),
        None => return Ok(()),
    };
    for v in values {
        if has_shell_special(v) {
            return Err(anyhow!(SchedulerError::ShellSpecialInOneShotArg(v.into())));
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

    // Reject shell-special chars in recurring skill-mode args. One-shot
    // entries are validated upstream by `sanitize_args_for_one_shot`.
    if let Some(Args::Map(m)) = &entry.args {
        for (k, v) in m {
            if has_shell_special(v) {
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
            "register-schedule: refusing unsafe folders.logs value '{raw}' (must be a relative \
             path with no `..` segments) — falling back to '07-logs'"
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
                "no `schedule:` entry matching skill `{skill}` in vault.yml — \
                 run `onebrain register-schedule --status` to list entries"
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
        eprintln!("--test: skill `{skill_name}` exited with code {code}");
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

    #[test]
    fn normalize_path_strips_curdir_and_pops_parentdir() {
        let p = normalize_path(Path::new("/a/b/./c/../d"));
        assert_eq!(p, PathBuf::from("/a/b/d"));
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
}
