//! `onebrain doctor` — run all vault health checks and emit a plain-text
//! report. Mirrors Bun's `printNonTtyOutput` byte-for-byte (icons, layout,
//! summary line) so cross-implementation parity tests stay green.
//!
//! `--fix` attempts targeted auto-repair recipes for each fixable warning,
//! then re-runs the checks so the user sees the result in a single
//! invocation. The recipes are deliberately narrow (one well-understood
//! action per check); anything ambiguous is reported as "untouched · run
//! the listed command yourself".

use anyhow::{anyhow, Context, Result};
use onebrain_core::{find_vault_root, load_vault_config, DoctorResult, DoctorStatus};
use onebrain_fs::doctor::run_all_checks;
use std::env;
use std::io::Write;
use std::path::Path;

/// Outcome of a single fix attempt — printed as part of the `--fix`
/// summary so the user can see what changed (or didn't).
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
        let warnings: Vec<&DoctorResult> = results
            .iter()
            .filter(|r| r.status == DoctorStatus::Warn)
            .collect();
        if warnings.is_empty() {
            println!("\nFix · nothing to do — no warnings.");
        } else {
            println!(
                "\nFix · attempting recipes for {} warning(s)",
                warnings.len()
            );
            let outcomes: Vec<(String, FixOutcome)> = warnings
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
        Ok(r) if r.ok => FixOutcome::Fixed(format!(
            "hooks registered · {} permission(s) added",
            r.permissions_added.len()
        )),
        Ok(_) => FixOutcome::Failed("register-hooks returned non-ok".to_string()),
        Err(e) => FixOutcome::Failed(format!("register-hooks: {e}")),
    }
}

/// Recipe — `plugin-files` warning means INSTRUCTIONS.md / agents/ /
/// skills/ / .claude-plugin/ is missing or incomplete. Re-overlay from
/// the upstream tarball via the existing vault-sync orchestrator.
fn fix_plugin_files(vault_root: &Path) -> FixOutcome {
    use onebrain_fs::{run_vault_sync, VaultSyncOptions};
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

/// Recipe — `vault.yml-keys` warning means one or more standard folder
/// keys (`inbox` / `projects` / ...) or the `update_channel` key is
/// missing. Backfill defaults into vault.yml without overwriting any
/// explicit user value. Pure additive merge.
fn fix_vault_yml_keys(vault_root: &Path) -> FixOutcome {
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

    // Backfill update_channel.
    let mut added = Vec::new();
    let channel_key = serde_yaml::Value::String("update_channel".to_string());
    if !mapping.contains_key(&channel_key) {
        mapping.insert(channel_key, serde_yaml::Value::String("stable".to_string()));
        added.push("update_channel");
    }

    // Backfill `folders.<key>` defaults — never overwrite, only insert.
    let folders_key = serde_yaml::Value::String("folders".to_string());
    let folders_entry = mapping
        .entry(folders_key)
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    if let Some(folders) = folders_entry.as_mapping_mut() {
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

    if added.is_empty() {
        return FixOutcome::Fixed("vault.yml already has all required keys".to_string());
    }

    let serialized = match serde_yaml::to_string(&yaml) {
        Ok(s) => s,
        Err(e) => return FixOutcome::Failed(format!("serialize vault.yml: {e}")),
    };
    if let Err(e) = std::fs::write(&path, serialized) {
        return FixOutcome::Failed(format!("write vault.yml: {e}"));
    }
    FixOutcome::Fixed(format!(
        "backfilled {} key(s): {}",
        added.len(),
        added.join(", ")
    ))
}

/// Recipe — `claude-settings` warning means `.claude/settings.json`
/// carries a stale `extraKnownMarketplaces.onebrain` entry from a
/// pre-marketplace install. Remove it — the plugin is loaded via
/// `enabledPlugins` instead, so the marketplace block is dead config.
fn fix_claude_settings(vault_root: &Path) -> FixOutcome {
    let path = vault_root.join(".claude").join("settings.json");
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

    let serialized = match serde_json::to_string_pretty(&json) {
        Ok(s) => s,
        Err(e) => return FixOutcome::Failed(format!("serialize settings.json: {e}")),
    };
    if let Err(e) = std::fs::write(&path, serialized) {
        return FixOutcome::Failed(format!("write settings.json: {e}"));
    }
    FixOutcome::Fixed("removed stale extraKnownMarketplaces.onebrain".to_string())
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
