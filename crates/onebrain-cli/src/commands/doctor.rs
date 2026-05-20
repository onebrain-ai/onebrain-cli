//! `onebrain doctor` — run all vault health checks and emit a plain-text
//! report. Mirrors Bun's `printNonTtyOutput` byte-for-byte (icons, layout,
//! summary line) so cross-implementation parity tests stay green.
//!
//! `--fix` attempts targeted auto-repair recipes for each fixable warning,
//! then re-runs the checks so the user sees the result in a single
//! invocation. The recipes are deliberately narrow (one well-understood
//! action per check); anything ambiguous is reported as "untouched · run
//! the listed command yourself".

use crate::safety::refuse_dangerous_vault_path;
use anyhow::{anyhow, Context, Result};
use onebrain_core::{find_vault_root, load_vault_config, DoctorResult, DoctorStatus};
use onebrain_fs::doctor::run_all_checks;
use std::env;
use std::io::Write;
use std::path::Path;

/// Outcome of a single fix attempt — printed as part of the `--fix`
/// summary so the user can see what changed (or didn't).
#[derive(Debug)]
enum FixOutcome {
    /// Fix ran cleanly and the underlying issue should be resolved.
    Fixed(String),
    /// Fix was attempted but did not finish (subprocess failed, timed out,
    /// etc.). The message explains why.
    Failed(String),
    /// No automated recipe exists for this warning — the user must take
    /// the action manually. Message is the suggested command.
    Manual(String),
}

/// Entry point — returns `Ok(0)` on no errors, `Ok(1)` when any check
/// produced `DoctorStatus::Error`. With `--fix`, the run is two-pass:
/// initial check, fix attempts on each warning, then a final re-check
/// whose result drives the exit code.
pub fn run(fix: bool) -> Result<i32> {
    let cwd = env::current_dir().context("read current directory")?;
    let vault_root =
        find_vault_root(&cwd).ok_or_else(|| anyhow!("not inside a vault (no vault.yml found)"))?;

    // Best-effort config load — on error, fall back to defaults so doctor can
    // still report what it sees (matches Bun behavior: stderr warning + defaults).
    let config = load_vault_config(&vault_root).unwrap_or_else(|err| {
        eprintln!("doctor: vault.yml load warning: {err}");
        onebrain_core::VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
        }
    });

    let mut results = run_all_checks(vault_root.as_path(), &config);
    print_report(&results, std::io::stdout())?;

    let mut any_recipe_failed = false;
    if fix {
        // Dispatch on both Warn and Error: some checks (plugin-files,
        // vault.yml-keys) emit Error for the very failure modes the recipes
        // are designed to repair additively (missing INSTRUCTIONS.md, missing
        // `folders` block). The dispatcher returns Manual for unrelated
        // checks, so unhandled errors are surfaced as text rather than
        // silently re-attempted.
        let issues: Vec<&DoctorResult> = results
            .iter()
            .filter(|r| matches!(r.status, DoctorStatus::Warn | DoctorStatus::Error))
            .collect();
        if issues.is_empty() {
            println!("\nFix · nothing to do — no issues.");
        } else {
            println!(
                "\nFix · attempting recipes for {} issue(s)",
                issues.len()
            );
            let outcomes: Vec<(String, FixOutcome)> = issues
                .iter()
                .map(|r| (r.check.clone(), attempt_fix(r, vault_root.as_path())))
                .collect();
            any_recipe_failed = outcomes
                .iter()
                .any(|(_, o)| matches!(o, FixOutcome::Failed(_)));
            print_fix_summary(&outcomes);
            // Re-run checks so the final exit code + report reflects the
            // post-fix state. `print_report` carries its own banner so we
            // skip a duplicate "Doctor (post-fix):" header — a single
            // blank line is enough to separate the two reports.
            println!();
            results = run_all_checks(vault_root.as_path(), &config);
            print_report(&results, std::io::stdout())?;
        }
    }

    let error_count = results
        .iter()
        .filter(|r| r.status == DoctorStatus::Error)
        .count();
    // Exit non-zero on any post-fix error OR any failed recipe — so the
    // caller's shell-level `if $? -ne 0` catches both check escalations
    // and dispatched-recipe failures.
    Ok(if error_count == 0 && !any_recipe_failed {
        0
    } else {
        1
    })
}

/// Dispatch the warning to the matching fix recipe. The match keys on
/// `result.check` AND the message content because some check names cover
/// multiple sub-conditions with different fix recipes — e.g.
/// `qmd-embeddings` fires for both "N unembedded" (real recipe: `qmd
/// embed`) and "qmd_collection not set in vault.yml" (no automated
/// recipe — user must edit the vault config). Hidden hints that say
/// "Run onebrain doctor --fix to ..." are silently rewritten to a
/// non-circular message when no recipe maps.
fn attempt_fix(result: &DoctorResult, vault_root: &Path) -> FixOutcome {
    match result.check.as_str() {
        // Only the "N unembedded" variant is auto-fixable. The
        // "qmd_collection not set" variant goes to Manual because
        // `qmd embed` without a configured collection is meaningless.
        "qmd-embeddings" if result.message.contains("unembedded") => fix_qmd_embeddings(),
        // Re-run register-hooks idempotently. Repairs missing Stop hook,
        // missing PostToolUse qmd hook (when qmd_collection is set), AND
        // missing `Bash(onebrain *)` permission entry. The lib already
        // handles all three cases as one atomic settings.json write.
        "settings-hooks" => fix_settings_hooks(vault_root),
        // Re-overlay plugin files from the upstream tarball. Idempotent;
        // brings INSTRUCTIONS.md / agents/ / skills/ / .claude-plugin/
        // back if they were deleted or never synced.
        "plugin-files" => fix_plugin_files(vault_root),
        // Backfill missing standard folder keys + default `update_channel`
        // in vault.yml. Safe (additive) — never overwrites user values.
        "vault.yml-keys" => fix_vault_yml_keys(vault_root),
        // Strip the stale `extraKnownMarketplaces.onebrain` entry from
        // `.claude/settings.json`. Cosmetic config cleanup; no behavioral
        // change at runtime (the plugin is enabled via `enabledPlugins`).
        "claude-settings" => fix_claude_settings(vault_root),
        // Orphan checkpoints can't be auto-deleted safely — the user may
        // still want to consolidate them via `/wrapup`. Steer them there
        // explicitly rather than risk silent data loss.
        "orphan-checkpoints" => FixOutcome::Manual(
            "run `/wrapup` in Claude to consolidate orphan checkpoints into a session log"
                .to_string(),
        ),
        _ => FixOutcome::Manual(manual_message(result)),
    }
}

/// Compose the per-warning Manual message. Strips any circular
/// "Run onebrain doctor --fix to ..." phrasing from the original hint
/// so users don't get pointed back at the command they just ran.
fn manual_message(result: &DoctorResult) -> String {
    let raw_hint = result.hint.as_deref().unwrap_or("");
    let cleaned =
        if raw_hint.contains("doctor --fix") || raw_hint.contains("Run onebrain doctor --fix") {
            "recipe not yet implemented for this check"
        } else if raw_hint.is_empty() {
            "see check details for next step"
        } else {
            raw_hint
        };
    format!("no automated recipe for `{}` · {cleaned}", result.check)
}

/// Recipe — `qmd-embeddings` warning means N files need embedding. Spawn
/// `qmd embed` and wait for it to finish; report success/failure based on
/// the exit code. The user sees full `qmd embed` output streamed to their
/// terminal in real time (we inherit stdio rather than capturing it).
fn fix_qmd_embeddings() -> FixOutcome {
    use std::process::Command;
    let qmd = match which::which("qmd") {
        Ok(p) => p,
        Err(_) => {
            return FixOutcome::Failed(
                "qmd binary not on PATH · install qmd then re-run".to_string(),
            )
        }
    };
    println!("  · running: qmd embed");
    let status = Command::new(qmd).arg("embed").status();
    match status {
        Ok(s) if s.success() => FixOutcome::Fixed("qmd embed completed".to_string()),
        Ok(s) => FixOutcome::Failed(format!(
            "qmd embed exited with code {}",
            s.code().unwrap_or(-1)
        )),
        Err(e) => FixOutcome::Failed(format!("spawn qmd embed: {e}")),
    }
}

/// Recipe — `settings-hooks` warning means the Stop hook, PostToolUse
/// qmd hook, or `Bash(onebrain *)` permission entry is missing or wrong.
/// Re-running `register-hooks` is fully idempotent (Bun parity) and
/// repairs all three in one atomic settings.json write.
fn fix_settings_hooks(vault_root: &Path) -> FixOutcome {
    use onebrain_fs::register_hooks::{run, RegisterHooksOptions};
    println!("  · running: register-hooks");
    let opts = RegisterHooksOptions {
        vault_dir: Some(vault_root.to_path_buf()),
        ..Default::default()
    };
    match run(opts) {
        // `run` always sets ok=true on the success paths and returns Err on
        // failure — no third "ok=false" branch is reachable today. Surface
        // both the success message and any error string directly.
        Ok(r) => FixOutcome::Fixed(format!(
            "hooks registered · {} permission(s) added",
            r.permissions_added.len()
        )),
        Err(e) => FixOutcome::Failed(format!("register-hooks: {e}")),
    }
}

/// Recipe — `plugin-files` warning means INSTRUCTIONS.md / agents/ /
/// skills/ / .claude-plugin/ is missing or incomplete. Re-overlay from
/// the upstream tarball via the existing vault-sync orchestrator.
fn fix_plugin_files(vault_root: &Path) -> FixOutcome {
    use onebrain_fs::{run_vault_sync, VaultSyncOptions};
    // Mirror the safety guard from `onebrain vault-sync` so the recipe
    // cannot accidentally splatter the filesystem root or $HOME on a
    // misconfigured vault.
    if let Err(e) = refuse_dangerous_vault_path(vault_root) {
        return FixOutcome::Failed(e.to_string());
    }
    println!("  · running: vault-sync");
    let result = run_vault_sync(vault_root, VaultSyncOptions::default());
    if result.ok {
        FixOutcome::Fixed("plugin files re-overlaid from upstream".to_string())
    } else {
        FixOutcome::Failed(
            result
                .error
                .unwrap_or_else(|| "vault-sync failed (no error detail)".to_string()),
        )
    }
}

/// Recipe — `vault.yml-keys` warning means one or more of:
///   - standard folder keys missing (`inbox` / `projects` / ...) or the
///     entire `folders:` block is missing/null
///   - `update_channel` not set
///   - deprecated keys still present (`onebrain_version`, `method`,
///     `runtime.harness`)
///   - `checkpoint.messages` / `checkpoint.minutes` ≤ 0
///
/// The recipe handles all four. YAML comments are not preserved (serde_yaml
/// re-serializes from the parsed model) — the Fixed message calls this out
/// so the user knows what changed besides the keys.
fn fix_vault_yml_keys(vault_root: &Path) -> FixOutcome {
    println!("  · running: backfill vault.yml");
    let path = vault_root.join("vault.yml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return FixOutcome::Failed(format!("read vault.yml: {e}")),
    };
    let mut yaml: serde_yaml::Value = match serde_yaml::from_str(&text) {
        Ok(v) => v,
        Err(e) => return FixOutcome::Failed(format!("parse vault.yml: {e}")),
    };
    let mapping = match yaml.as_mapping_mut() {
        Some(m) => m,
        None => return FixOutcome::Failed("vault.yml root is not a mapping".to_string()),
    };

    let mut added: Vec<&'static str> = Vec::new();
    let mut removed: Vec<&'static str> = Vec::new();
    let mut repaired: Vec<&'static str> = Vec::new();

    // 1. Backfill `update_channel`.
    let channel_key = serde_yaml::Value::String("update_channel".to_string());
    if !mapping.contains_key(&channel_key) {
        mapping.insert(channel_key, serde_yaml::Value::String("stable".to_string()));
        added.push("update_channel");
    }

    // 2. Backfill `folders.<key>` defaults. If `folders` is missing OR null
    //    OR not a mapping, replace it with an empty mapping before inserting.
    //    `entry().or_insert_with()` does NOT replace existing nulls, hence
    //    the explicit check.
    let folders_key = serde_yaml::Value::String("folders".to_string());
    let needs_replace = match mapping.get(&folders_key) {
        Some(v) => !v.is_mapping(),
        None => true,
    };
    if needs_replace {
        mapping.insert(
            folders_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    if let Some(folders) = mapping
        .get_mut(&folders_key)
        .and_then(|v| v.as_mapping_mut())
    {
        const STANDARD: &[(&str, &str)] = &[
            ("inbox", "00-inbox"),
            ("projects", "01-projects"),
            ("areas", "02-areas"),
            ("knowledge", "03-knowledge"),
            ("resources", "04-resources"),
            ("agent", "05-agent"),
            ("archive", "06-archive"),
            ("logs", "07-logs"),
        ];
        for (key, default) in STANDARD {
            let yaml_key = serde_yaml::Value::String((*key).to_string());
            if !folders.contains_key(&yaml_key) {
                folders.insert(yaml_key, serde_yaml::Value::String((*default).to_string()));
                added.push(*key);
            }
        }
    }

    // 3. Strip deprecated keys.
    for key in &["onebrain_version", "method"] {
        let k = serde_yaml::Value::String((*key).to_string());
        if mapping.remove(&k).is_some() {
            removed.push(*key);
        }
    }
    let runtime_key = serde_yaml::Value::String("runtime".to_string());
    if let Some(runtime) = mapping
        .get_mut(&runtime_key)
        .and_then(|v| v.as_mapping_mut())
    {
        let harness_key = serde_yaml::Value::String("harness".to_string());
        if runtime.remove(&harness_key).is_some() {
            removed.push("runtime.harness");
        }
        // Drop the parent `runtime` block if it's now empty — keeps the
        // file tidy after the deprecated child key is removed.
        let now_empty = runtime.is_empty();
        if now_empty {
            mapping.remove(&runtime_key);
        }
    }

    // 4. Repair non-positive `checkpoint.messages` / `checkpoint.minutes`.
    //    Defaults match Bun: 15 messages, 30 minutes.
    let checkpoint_key = serde_yaml::Value::String("checkpoint".to_string());
    if let Some(checkpoint) = mapping
        .get_mut(&checkpoint_key)
        .and_then(|v| v.as_mapping_mut())
    {
        for (key, default) in &[("messages", 15_u64), ("minutes", 30_u64)] {
            let k = serde_yaml::Value::String((*key).to_string());
            let needs_fix = checkpoint
                .get(&k)
                .map(|v| !value_is_positive_number(v))
                .unwrap_or(false);
            if needs_fix {
                checkpoint.insert(k, serde_yaml::Value::Number((*default).into()));
                repaired.push(*key);
            }
        }
    }

    if added.is_empty() && removed.is_empty() && repaired.is_empty() {
        return FixOutcome::Fixed("vault.yml already in expected shape".to_string());
    }

    let serialized = match serde_yaml::to_string(&yaml) {
        Ok(s) => s,
        Err(e) => return FixOutcome::Failed(format!("serialize vault.yml: {e}")),
    };
    if let Err(e) = atomic_write_text(&path, &serialized) {
        return FixOutcome::Failed(format!("write vault.yml: {e}"));
    }

    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("backfilled {}: {}", added.len(), added.join(", ")));
    }
    if !removed.is_empty() {
        parts.push(format!(
            "removed deprecated {}: {}",
            removed.len(),
            removed.join(", ")
        ));
    }
    if !repaired.is_empty() {
        parts.push(format!(
            "repaired {} checkpoint key(s): {}",
            repaired.len(),
            repaired.join(", ")
        ));
    }
    FixOutcome::Fixed(format!(
        "{} (note: YAML comments not preserved)",
        parts.join(" · ")
    ))
}

/// Recipe — `claude-settings` warning means `.claude/settings.json`
/// carries a stale `extraKnownMarketplaces.onebrain` entry from a
/// pre-marketplace install. Remove it — the plugin is loaded via
/// `enabledPlugins` instead, so the marketplace block is dead config.
///
/// Writes via `register_hooks::write_settings` for atomic tmp+rename and the
/// canonical 4-space indent (matches register-hooks output byte-for-byte).
fn fix_claude_settings(vault_root: &Path) -> FixOutcome {
    use onebrain_fs::register_hooks::{settings_path, write_settings};
    println!("  · running: clean settings.json");
    let path = settings_path(vault_root);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return FixOutcome::Failed(format!("read settings.json: {e}")),
    };
    let mut json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return FixOutcome::Failed(format!("parse settings.json: {e}")),
    };
    let obj = match json.as_object_mut() {
        Some(o) => o,
        None => return FixOutcome::Failed("settings.json root is not an object".to_string()),
    };

    let marketplaces = obj
        .get_mut("extraKnownMarketplaces")
        .and_then(|v| v.as_object_mut());
    let mut removed_any = false;
    if let Some(mp) = marketplaces {
        if mp.remove("onebrain").is_some() {
            removed_any = true;
        }
        // If the wrapper is now empty, drop it entirely.
        if mp.is_empty() {
            obj.remove("extraKnownMarketplaces");
        }
    }
    if !removed_any {
        return FixOutcome::Fixed("settings.json already clean".to_string());
    }

    if let Err(e) = write_settings(&path, &json) {
        return FixOutcome::Failed(format!("write settings.json: {e}"));
    }
    FixOutcome::Fixed("removed stale extraKnownMarketplaces.onebrain".to_string())
}

/// Match Bun's `typeof value === 'number' && value > 0` for YAML values.
/// Used by `fix_vault_yml_keys` to decide whether to repair a checkpoint
/// number — mirrors `vault_yml_keys::is_positive_number`.
fn value_is_positive_number(v: &serde_yaml::Value) -> bool {
    match v {
        serde_yaml::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                f > 0.0
            } else if let Some(i) = n.as_i64() {
                i > 0
            } else if let Some(u) = n.as_u64() {
                u > 0
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Atomic text write: `path.tmp` → rename. Same pattern as
/// `register_hooks::write_settings` (and `onebrain-cache::state`); used
/// here for YAML where the canonical helper is JSON-specific. Creates
/// parent dirs as needed.
fn atomic_write_text(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.to_path_buf();
    let new_ext = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    };
    tmp.set_extension(new_ext);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn print_fix_summary(outcomes: &[(String, FixOutcome)]) {
    let mut fixed = 0;
    let mut failed = 0;
    let mut manual = 0;
    for (check, outcome) in outcomes {
        match outcome {
            FixOutcome::Fixed(msg) => {
                fixed += 1;
                println!("  ✓ {check}: {msg}");
            }
            FixOutcome::Failed(msg) => {
                failed += 1;
                println!("  ✗ {check}: {msg}");
            }
            FixOutcome::Manual(msg) => {
                manual += 1;
                println!("  · {check}: {msg}");
            }
        }
    }
    println!("\nFix summary: {fixed} fixed · {failed} failed · {manual} manual",);
}

fn print_report<W: Write>(results: &[DoctorResult], mut w: W) -> Result<()> {
    writeln!(w, "OneBrain Doctor")?;
    writeln!(w)?;
    for r in results {
        let icon = match r.status {
            DoctorStatus::Ok => "[\u{2713}]",
            DoctorStatus::Warn => "[!]",
            DoctorStatus::Error => "[\u{2717}]",
        };
        writeln!(w, "  {} {:<20} {}", icon, r.check, r.message)?;
        if let Some(hint) = &r.hint {
            writeln!(w, "        \u{2192} {hint}")?;
        }
        for d in &r.details {
            writeln!(w, "        \u{00b7} {d}")?;
        }
    }
    writeln!(w)?;
    let total = results.len();
    let errors = results
        .iter()
        .filter(|r| r.status == DoctorStatus::Error)
        .count();
    let warnings = results
        .iter()
        .filter(|r| r.status == DoctorStatus::Warn)
        .count();
    if errors > 0 {
        writeln!(
            w,
            "Summary: {total} checks · {errors} error(s) · {warnings} warning(s) — fix before using"
        )?;
    } else if warnings > 0 {
        writeln!(
            w,
            "Summary: {total} checks · {warnings} warning(s) — ok to run"
        )?;
    } else {
        writeln!(w, "Summary: {total} checks — all passed")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_core::DoctorResult;

    #[test]
    fn print_report_all_ok() {
        let results = vec![DoctorResult::ok("vault.yml", "valid")];
        let mut buf = Vec::new();
        print_report(&results, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("[\u{2713}]"));
        assert!(s.contains("all passed"));
    }

    #[test]
    fn print_report_summarizes_errors_and_warnings() {
        let results = vec![
            DoctorResult::error("a", "broken"),
            DoctorResult::warn("b", "iffy"),
            DoctorResult::ok("c", "good"),
        ];
        let mut buf = Vec::new();
        print_report(&results, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("1 error(s)"));
        assert!(s.contains("1 warning(s)"));
        assert!(s.contains("fix before using"));
    }

    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn fix_vault_yml_keys_backfills_missing_keys() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("vault.yml"), "qmd_collection: foo\n").unwrap();
        let outcome = fix_vault_yml_keys(d.path());
        match outcome {
            FixOutcome::Fixed(msg) => {
                assert!(msg.contains("update_channel"), "msg: {msg}");
                assert!(msg.contains("inbox"), "msg: {msg}");
            }
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert!(after.contains("update_channel: stable"));
        assert!(after.contains("inbox: 00-inbox"));
        assert!(after.contains("logs: 07-logs"));
        assert!(after.contains("qmd_collection: foo")); // preserved
    }

    #[test]
    fn fix_vault_yml_keys_handles_null_folders() {
        // Regression: `entry().or_insert_with()` does NOT replace existing
        // `Null` values, so a `folders: null` line previously caused the
        // recipe to silently skip the 8 sub-key inserts.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("vault.yml"),
            "update_channel: stable\nfolders: null\n",
        )
        .unwrap();
        let outcome = fix_vault_yml_keys(d.path());
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("inbox"), "msg: {msg}"),
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert!(after.contains("inbox: 00-inbox"));
        assert!(after.contains("logs: 07-logs"));
    }

    #[test]
    fn fix_vault_yml_keys_strips_deprecated_keys() {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("vault.yml"),
            "update_channel: stable\n\
             onebrain_version: \"2.1.0\"\n\
             method: legacy\n\
             runtime:\n  harness: claude-code\n\
             folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
        )
        .unwrap();
        let outcome = fix_vault_yml_keys(d.path());
        match outcome {
            FixOutcome::Fixed(msg) => {
                assert!(msg.contains("onebrain_version"), "msg: {msg}");
                assert!(msg.contains("method"), "msg: {msg}");
                assert!(msg.contains("runtime.harness"), "msg: {msg}");
            }
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert!(!after.contains("onebrain_version"), "{after}");
        assert!(!after.contains("method: legacy"), "{after}");
        assert!(!after.contains("harness"), "{after}");
    }

    #[test]
    fn fix_vault_yml_keys_repairs_checkpoint_nums() {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("vault.yml"),
            "update_channel: stable\n\
             checkpoint:\n  messages: 0\n  minutes: -5\n\
             folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
        )
        .unwrap();
        let outcome = fix_vault_yml_keys(d.path());
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("checkpoint"), "msg: {msg}"),
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert!(after.contains("messages: 15"));
        assert!(after.contains("minutes: 30"));
    }

    #[test]
    fn fix_vault_yml_keys_idempotent_on_clean_file() {
        let d = tempdir().unwrap();
        let clean = "update_channel: stable\n\
                     folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n";
        fs::write(d.path().join("vault.yml"), clean).unwrap();
        let outcome = fix_vault_yml_keys(d.path());
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("already"), "msg: {msg}"),
            other => panic!("expected Fixed (no-op), got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert_eq!(after, clean, "file untouched when nothing to do");
    }

    #[test]
    fn fix_claude_settings_removes_stale_marketplace() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        let original = serde_json::json!({
            "extraKnownMarketplaces": {
                "onebrain": { "source": { "repo": "kengio/onebrain" } }
            },
            "permissions": { "allow": ["Read"] }
        });
        fs::write(
            d.path().join(".claude/settings.json"),
            serde_json::to_string_pretty(&original).unwrap(),
        )
        .unwrap();
        let outcome = fix_claude_settings(d.path());
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("removed"), "msg: {msg}"),
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after_text = fs::read_to_string(d.path().join(".claude/settings.json")).unwrap();
        let after_json: serde_json::Value = serde_json::from_str(&after_text).unwrap();
        assert!(after_json.get("extraKnownMarketplaces").is_none());
        assert_eq!(after_json["permissions"]["allow"][0], "Read");
        // 4-space indent (via register_hooks::write_settings)
        assert!(
            after_text.contains("    \"permissions\""),
            "expected 4-space indent: {after_text}"
        );
    }

    #[test]
    fn fix_claude_settings_idempotent_on_clean_file() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        fs::write(
            d.path().join(".claude/settings.json"),
            r#"{"permissions": {}}"#,
        )
        .unwrap();
        let outcome = fix_claude_settings(d.path());
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("already clean"), "msg: {msg}"),
            other => panic!("expected Fixed (no-op), got: {other:?}"),
        }
    }

    #[test]
    fn fix_plugin_files_refuses_filesystem_root() {
        let mut root = std::env::temp_dir();
        while let Some(parent) = root.parent() {
            root = parent.to_path_buf();
        }
        let outcome = fix_plugin_files(&root);
        match outcome {
            FixOutcome::Failed(msg) => assert!(msg.contains("filesystem root"), "msg: {msg}"),
            other => panic!("expected Failed (safety guard), got: {other:?}"),
        }
    }

    #[test]
    fn print_report_snapshot_mixed_statuses() {
        let results = vec![
            DoctorResult::ok("vault.yml", "valid").with_details(vec!["qmd: ob-1".into()]),
            DoctorResult::warn("settings-hooks", "2 issue(s)")
                .with_hint("Run onebrain doctor --fix to repair hooks")
                .with_details(vec!["Stop hook missing".into()]),
            DoctorResult::error("folders", "7/8 present")
                .with_hint("Missing: 01-projects")
                .with_details(vec!["missing: 01-projects".into()]),
        ];
        let mut buf = Vec::new();
        print_report(&results, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        insta::assert_snapshot!(output);
    }
}
