//! `onebrain doctor` — run all vault health checks and emit a report.
//!
//! On an interactive TTY (text mode) the checks are revealed one at a time
//! with a short per-step delay so the run reads as sequential work; piped /
//! non-TTY stdout and structured (`--json`/`--yaml`) modes get the instant
//! plain report unchanged. The plain renderer keeps the original icon/layout/
//! summary shape (`print_report`).
//!
//! `--fix` attempts targeted auto-repair recipes for each fixable warning,
//! then re-runs the checks so the user sees the result in a single
//! invocation. The recipes are deliberately narrow (one well-understood
//! action per check); anything ambiguous is reported as "untouched · run
//! the listed command yourself".

use crate::legacy_output::serialize_for_mode;
use crate::output::OutputMode;
use crate::safety::refuse_dangerous_vault_path;
use crate::vault_ctx;
use anyhow::{anyhow, Result};
use onebrain_core::{load_vault_config, DoctorResult, DoctorStatus};
use onebrain_fs::doctor::run_all_checks;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

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
///
/// `json` switches output from the plain-text formatter to a single JSON
/// document on stdout (for scripts / the `/doctor` plugin skill). In
/// JSON mode the initial report is suppressed and `--fix` outcomes are
/// captured into the wrapper instead of being printed line-by-line —
/// so the entire command produces exactly one JSON document.
///
/// `vault_flag` carries the global `--vault <PATH>` value (if any). The
/// vault is resolved through the canonical chain: flag > `ONEBRAIN_VAULT`
/// env > walk-up from cwd. This matches `vault current` / every other
/// v3.1 vault-aware command so `onebrain doctor --vault PATH` works from
/// outside the vault directory.
pub fn run(fix: bool, json: bool, vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<i32> {
    // v3.1: structured output is triggered by EITHER the local `--json`
    // flag (back-compat with v3.0 callers) OR any global format flag
    // (`--yaml`, `--output yaml`, …). `mode.is_structured()` catches every
    // non-text variant so `--yaml` no longer silently emits text.
    let want_structured = json || mode.is_structured();
    let vault_root = match vault_ctx::resolve(vault_flag)? {
        Some(r) => r.root,
        None => {
            // In structured mode, emit a failure envelope on stdout so
            // scripted consumers don't have to parse anyhow text from
            // stderr. Plain-text mode keeps the existing anyhow flow.
            if want_structured {
                let doc = serde_json::json!({
                    "ok": false,
                    "error": "not_in_vault",
                    "message": "not inside a vault (no onebrain.yml or vault.yml found)",
                });
                println!("{}", emit_structured(&doc, json, mode)?);
                return Ok(1);
            }
            return Err(anyhow!(
                "not inside a vault (no onebrain.yml or vault.yml found)"
            ));
        }
    };

    // Best-effort config load — on error, fall back to defaults so doctor can
    // still report what it sees (matches Bun behavior: stderr warning + defaults).
    let config = load_vault_config(&vault_root).unwrap_or_else(|err| {
        eprintln!("doctor: config load warning: {err}");
        onebrain_core::VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
        }
    });

    let mut results = run_all_checks(vault_root.as_path(), &config);
    if !want_structured {
        emit_text_report(&results)?;
    }

    let mut any_recipe_failed = false;
    let mut fix_outcomes_json: Vec<serde_json::Value> = Vec::new();
    if fix {
        // Dispatch on both Warn and Error: some checks (plugin-files,
        // onebrain.yml-keys) emit Error for the very failure modes the recipes
        // are designed to repair additively (missing INSTRUCTIONS.md, missing
        // `folders` block). The dispatcher returns Manual for unrelated
        // checks, so unhandled errors are surfaced as text rather than
        // silently re-attempted.
        let issues: Vec<&DoctorResult> = results
            .iter()
            .filter(|r| matches!(r.status, DoctorStatus::Warn | DoctorStatus::Error))
            .collect();
        if issues.is_empty() {
            if !want_structured {
                println!("\nFix · nothing to do — no issues.");
            }
        } else {
            if !want_structured {
                println!("\nFix · attempting recipes for {} issue(s)", issues.len());
            }
            let outcomes: Vec<(String, FixOutcome)> = issues
                .iter()
                .map(|r| {
                    (
                        r.check.clone(),
                        attempt_fix(r, vault_root.as_path(), want_structured),
                    )
                })
                .collect();
            any_recipe_failed = outcomes
                .iter()
                .any(|(_, o)| matches!(o, FixOutcome::Failed(_)));
            if want_structured {
                fix_outcomes_json = outcomes
                    .iter()
                    .map(|(check, o)| {
                        let (outcome, message) = match o {
                            FixOutcome::Fixed(m) => ("fixed", m.as_str()),
                            FixOutcome::Failed(m) => ("failed", m.as_str()),
                            FixOutcome::Manual(m) => ("manual", m.as_str()),
                        };
                        serde_json::json!({
                            "check": check,
                            "outcome": outcome,
                            "message": message,
                        })
                    })
                    .collect();
            } else {
                print_fix_summary(&outcomes);
                // Blank line separates the fix summary from the post-fix report.
                println!();
            }
            results = run_all_checks(vault_root.as_path(), &config);
            if !want_structured {
                print_report(&results, std::io::stdout())?;
            }
        }
    }

    let error_count = results
        .iter()
        .filter(|r| r.status == DoctorStatus::Error)
        .count();
    if want_structured {
        // Always emit `fix[]` when --fix was requested so consumers can
        // distinguish "user didn't ask to fix" (key absent) from "user asked
        // but nothing to fix" (key present, empty array). Schema stability.
        print_report_structured(
            &results,
            fix,
            fix_outcomes_json,
            json,
            mode,
            std::io::stdout(),
        )?;
    }
    // Exit non-zero on any post-fix error OR any failed recipe — so the
    // caller's shell-level `if $? -ne 0` catches both check escalations
    // and dispatched-recipe failures.
    Ok(if error_count == 0 && !any_recipe_failed {
        0
    } else {
        1
    })
}

/// Render `results` + optional fix outcomes as a single JSON document.
/// Field shape is documented inline; callers (the `/doctor` plugin skill,
/// CI consumers) treat the schema as stable for v3.x.
///
/// `fix_requested` is true when `--fix` was passed — the `fix[]` key is
/// emitted whenever this is true (even if empty), so consumers can
/// distinguish "user didn't ask to fix" (key absent) from "user asked but
/// nothing needed fixing" (key present, empty).
///
/// Naming note: `summary.passing` is the count of OK checks (not a boolean
/// — top-level `ok` is the boolean). The two fields are deliberately named
/// differently to prevent the `summary.ok` count-vs-boolean confusion that
/// the first draft of this code had.
fn print_report_structured<W: Write>(
    results: &[DoctorResult],
    fix_requested: bool,
    fix_outcomes: Vec<serde_json::Value>,
    legacy_json_flag: bool,
    mode: &OutputMode,
    mut w: W,
) -> Result<()> {
    let total = results.len();
    let errors = results
        .iter()
        .filter(|r| r.status == DoctorStatus::Error)
        .count();
    let warnings = results
        .iter()
        .filter(|r| r.status == DoctorStatus::Warn)
        .count();
    let passing = total - errors - warnings;
    let mut doc = serde_json::json!({
        "ok": errors == 0,
        "summary": {
            "total": total,
            "errors": errors,
            "warnings": warnings,
            "passing": passing,
        },
        "checks": results,
    });
    if fix_requested {
        doc.as_object_mut()
            .expect("doctor json root is object")
            .insert("fix".to_string(), serde_json::Value::Array(fix_outcomes));
    }
    writeln!(w, "{}", emit_structured(&doc, legacy_json_flag, mode)?)?;
    Ok(())
}

/// Render `doc` for the active structured-output mode.
///
/// v3.1 contract:
/// - `--yaml` / `--output yaml` (`mode.is_structured()` AND mode is YAML) →
///   YAML serialisation via [`serialize_for_mode`].
/// - `--json --pretty` / `--output json` with pretty → indented JSON via
///   the same dispatcher.
/// - Bare `--json` (legacy `json: bool`, no global format flag) → compact
///   JSON byte-identical to v3.0 output so scripted consumers don't drift.
///
/// Contract: caller invokes this only when `want_structured = json ||
/// mode.is_structured()` is true; the third arm here is therefore
/// unreachable from production code paths. `debug_assert!` makes any
/// future caller-side drift loud during testing; the compact-JSON
/// fallback prevents a prod panic if drift ships anyway.
fn emit_structured(
    doc: &serde_json::Value,
    legacy_json_flag: bool,
    mode: &OutputMode,
) -> Result<String> {
    if mode.is_structured() {
        Ok(serialize_for_mode(doc, mode))
    } else if legacy_json_flag {
        Ok(serde_json::to_string(doc)?)
    } else {
        debug_assert!(
            false,
            "emit_structured invoked without structured mode or legacy_json_flag"
        );
        Ok(serde_json::to_string(doc)?)
    }
}

/// Dispatch the warning to the matching fix recipe. The match keys on
/// `result.check` AND the message content because some check names cover
/// multiple sub-conditions with different fix recipes — e.g.
/// `qmd-embeddings` fires for both "N unembedded" (real recipe: `qmd
/// embed`) and "qmd_collection not set in vault.yml" (no automated
/// recipe — user must edit the vault config). Hidden hints that say
/// "Run onebrain doctor --fix to ..." are silently rewritten to a
/// non-circular message when no recipe maps.
fn attempt_fix(result: &DoctorResult, vault_root: &Path, json: bool) -> FixOutcome {
    match result.check.as_str() {
        // Only the "N unembedded" variant is auto-fixable. The
        // "qmd_collection not set" variant goes to Manual because
        // `qmd embed` without a configured collection is meaningless.
        "qmd-embeddings" if result.message.contains("unembedded") => fix_qmd_embeddings(json),
        // Re-run register-hooks idempotently. Repairs missing Stop hook,
        // missing PostToolUse qmd hook (when qmd_collection is set), AND
        // missing `Bash(onebrain *)` permission entry. The lib already
        // handles all three cases as one atomic settings.json write.
        "settings-hooks" => fix_settings_hooks(vault_root, json),
        // Re-overlay plugin files from the upstream tarball. Idempotent;
        // brings INSTRUCTIONS.md / agents/ / skills/ / .claude-plugin/
        // back if they were deleted or never synced.
        "plugin-files" => fix_plugin_files(vault_root, json),
        // Backfill missing standard folder keys + default `update_channel`
        // in onebrain.yml. Safe (additive) — never overwrites user values.
        "onebrain.yml-keys" => fix_vault_yml_keys(vault_root, json),
        // Strip the stale `extraKnownMarketplaces.onebrain` entry from
        // `.claude/settings.json`. Cosmetic config cleanup; no behavioral
        // change at runtime (the plugin is enabled via `enabledPlugins`).
        "claude-settings" => fix_claude_settings(vault_root, json),
        // Migrate legacy `vault.yml` → canonical `onebrain.yml` via a
        // single atomic `fs::rename`. Idempotent: drops legacy if both
        // exist (canonical wins); reports already-clean when only the
        // canonical filename is present.
        "vault-config-migration" => fix_vault_config_migration(vault_root, json),
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

/// Status-line emitter shared across recipes. In plain-text mode the line
/// goes to stdout (human-friendly inline with the fix summary); in JSON
/// mode it goes to stderr so the JSON document remains the only stdout
/// payload (consumer-friendly: `cmd 2>/dev/null` gives clean JSON).
fn status_line(json: bool, msg: &str) {
    if json {
        eprintln!("  · {msg}");
    } else {
        println!("  · {msg}");
    }
}

/// Recipe — `qmd-embeddings` warning means N files need embedding. Spawn
/// `qmd embed` and wait for it to finish; report success/failure based on
/// the exit code. In plain-text mode `qmd embed` output is inherited (user
/// sees the embedder's progress). In JSON mode it's captured to /dev/null
/// to keep stdout reserved for the doctor JSON document.
fn fix_qmd_embeddings(json: bool) -> FixOutcome {
    use std::process::{Command, Stdio};
    let qmd = match which::which("qmd") {
        Ok(p) => p,
        Err(_) => {
            return FixOutcome::Failed(
                "qmd binary not on PATH · install qmd then re-run".to_string(),
            )
        }
    };
    status_line(json, "running: qmd embed");
    let mut cmd = Command::new(qmd);
    cmd.arg("embed");
    if json {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let status = cmd.status();
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
fn fix_settings_hooks(vault_root: &Path, json: bool) -> FixOutcome {
    use onebrain_fs::register_hooks::{run, RegisterHooksOptions};
    status_line(json, "running: register-hooks");
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
fn fix_plugin_files(vault_root: &Path, json: bool) -> FixOutcome {
    use onebrain_fs::{run_vault_sync, VaultSyncOptions};
    // Mirror the safety guard from `onebrain vault-sync` so the recipe
    // cannot accidentally splatter the filesystem root or $HOME on a
    // misconfigured vault.
    if let Err(e) = refuse_dangerous_vault_path(vault_root) {
        return FixOutcome::Failed(e.to_string());
    }
    status_line(json, "running: vault-sync");
    // In JSON mode route vault-sync's PlainProgress to stderr so stdout
    // remains reserved for the JSON document.
    let opts = if json {
        VaultSyncOptions {
            is_tty: Some(false),
            progress_writer: Some(Box::new(std::io::stderr())),
            ..Default::default()
        }
    } else {
        VaultSyncOptions::default()
    };
    let result = run_vault_sync(vault_root, opts);
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

/// Recipe — `onebrain.yml-keys` warning means one or more of:
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
fn fix_vault_yml_keys(vault_root: &Path, json: bool) -> FixOutcome {
    use onebrain_core::{find_config_file, CONFIG_FILENAME};
    // Operate on whichever config file is present — canonical preferred,
    // legacy fallback. The `vault-config-migration` recipe rename runs
    // separately; this recipe is filename-agnostic.
    let path = find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(CONFIG_FILENAME)
        .to_string();
    status_line(json, &format!("running: backfill {filename}"));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return FixOutcome::Failed(format!("read {filename}: {e}")),
    };
    let mut yaml: serde_yaml::Value = match serde_yaml::from_str(&text) {
        Ok(v) => v,
        Err(e) => return FixOutcome::Failed(format!("parse {filename}: {e}")),
    };
    let mapping = match yaml.as_mapping_mut() {
        Some(m) => m,
        None => return FixOutcome::Failed(format!("{filename} root is not a mapping")),
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
        return FixOutcome::Fixed(format!("{filename} already in expected shape"));
    }

    let serialized = match serde_yaml::to_string(&yaml) {
        Ok(s) => s,
        Err(e) => return FixOutcome::Failed(format!("serialize {filename}: {e}")),
    };
    // Defense-in-depth: back up the config before this re-serializing write
    // (which drops comments). Hard precondition — no write without a backup.
    if let Err(e) = onebrain_fs::backup_config_file(&path) {
        return FixOutcome::Failed(format!("backup {filename} before write: {e}"));
    }
    if let Err(e) = atomic_write_text(&path, &serialized) {
        return FixOutcome::Failed(format!("write {filename}: {e}"));
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
fn fix_claude_settings(vault_root: &Path, json: bool) -> FixOutcome {
    use onebrain_fs::register_hooks::{settings_path, write_settings};
    status_line(json, "running: clean settings.json");
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

/// Recipe — `vault-config-migration` warning means the vault uses the
/// legacy `vault.yml` filename. Atomic single-syscall rename to canonical
/// `onebrain.yml`. When both files exist (split state), drop the legacy
/// one — the canonical filename takes precedence at runtime so the
/// legacy file is already ignored; removing it eliminates the source of
/// future drift.
fn fix_vault_config_migration(vault_root: &Path, json: bool) -> FixOutcome {
    use onebrain_core::{CONFIG_FILENAME, LEGACY_CONFIG_FILENAME};
    status_line(json, "running: migrate vault.yml → onebrain.yml");
    let canonical = vault_root.join(CONFIG_FILENAME);
    let legacy = vault_root.join(LEGACY_CONFIG_FILENAME);
    let canonical_exists = canonical.is_file();
    let legacy_exists = legacy.is_file();

    match (canonical_exists, legacy_exists) {
        (true, false) | (false, false) => {
            // Already on canonical (or no config at all — VaultYmlCheck
            // surfaces that error). Idempotent no-op.
            FixOutcome::Fixed("onebrain.yml in use — nothing to migrate".to_string())
        }
        (false, true) => {
            // Back up the legacy file before migrating, so the pre-rename
            // state is always recoverable. Hard precondition.
            if let Err(e) = onebrain_fs::backup_config_file(&legacy) {
                return FixOutcome::Failed(format!("backup vault.yml before migrate: {e}"));
            }
            match std::fs::rename(&legacy, &canonical) {
                Ok(()) => FixOutcome::Fixed("renamed vault.yml → onebrain.yml".to_string()),
                Err(e) => FixOutcome::Failed(format!("rename vault.yml: {e}")),
            }
        }
        (true, true) => {
            // Back up the stale legacy file before removing it — never delete
            // a config without a recoverable copy first.
            if let Err(e) = onebrain_fs::backup_config_file(&legacy) {
                return FixOutcome::Failed(format!("backup vault.yml before removal: {e}"));
            }
            match std::fs::remove_file(&legacy) {
                Ok(()) => FixOutcome::Fixed(
                    "removed stale vault.yml (onebrain.yml already present · backup kept)"
                        .to_string(),
                ),
                Err(e) => FixOutcome::Failed(format!("remove vault.yml: {e}")),
            }
        }
    }
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

/// Render a single check block: icon + status line, then any hint / details.
fn write_check<W: Write>(w: &mut W, r: &DoctorResult) -> Result<()> {
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
    Ok(())
}

/// Render the trailing one-line summary.
fn write_summary<W: Write>(w: &mut W, results: &[DoctorResult]) -> Result<()> {
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

/// Plain, instant text report — the canonical renderer. Used for piped /
/// non-TTY stdout and by unit tests (writes to any `Write`, no timing).
fn print_report<W: Write>(results: &[DoctorResult], mut w: W) -> Result<()> {
    writeln!(w, "OneBrain Doctor")?;
    writeln!(w)?;
    for r in results {
        write_check(&mut w, r)?;
    }
    writeln!(w)?;
    write_summary(&mut w, results)
}

/// Per-step delay (ms) for the animated TTY report. Overridable via
/// `ONEBRAIN_DOCTOR_STEP_MS`; `0` disables the animation (instant plain
/// report) — handy for impatient users, demos, and deterministic tests.
fn doctor_step_delay_ms() -> u64 {
    std::env::var("ONEBRAIN_DOCTOR_STEP_MS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(90)
}

/// Animated text report for an interactive terminal: each check first shows a
/// transient `⋯ checking <name>…` line, pauses briefly, then clears it and
/// reveals the result — so the run reads as one-thing-at-a-time work instead
/// of an instant wall of text. A `0` delay short-circuits to the plain report.
fn print_report_animated(results: &[DoctorResult]) -> Result<()> {
    let delay = doctor_step_delay_ms();
    if delay == 0 {
        return print_report(results, std::io::stdout());
    }
    let step = std::time::Duration::from_millis(delay);
    let mut w = std::io::stdout();
    writeln!(w, "OneBrain Doctor")?;
    writeln!(w)?;
    for r in results {
        // Transient progress line · cleared and replaced by the result block.
        write!(w, "  \u{22ef} {}\u{2026}", r.check)?;
        w.flush()?;
        std::thread::sleep(step);
        write!(w, "\r\u{1b}[K")?; // carriage-return + clear-to-end-of-line
        write_check(&mut w, r)?;
        w.flush()?;
    }
    writeln!(w)?;
    write_summary(&mut w, results)
}

/// Emit the text report to stdout, animating step-by-step on an interactive
/// terminal and falling back to the instant plain report when stdout is piped
/// / redirected / non-TTY (v3.1.1 doctor UX).
fn emit_text_report(results: &[DoctorResult]) -> Result<()> {
    if std::io::stdout().is_terminal() {
        print_report_animated(results)
    } else {
        print_report(results, std::io::stdout())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_core::DoctorResult;

    #[test]
    fn print_report_all_ok() {
        let results = vec![DoctorResult::ok("onebrain.yml", "valid")];
        let mut buf = Vec::new();
        print_report(&results, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("[\u{2713}]"));
        assert!(s.contains("all passed"));
    }

    #[test]
    fn doctor_step_delay_defaults_and_honors_env() {
        // Sole reader of this env var in-process, so no parallel-test race.
        std::env::remove_var("ONEBRAIN_DOCTOR_STEP_MS");
        assert_eq!(doctor_step_delay_ms(), 90, "default");
        std::env::set_var("ONEBRAIN_DOCTOR_STEP_MS", "0");
        assert_eq!(doctor_step_delay_ms(), 0, "0 disables the animation");
        std::env::set_var("ONEBRAIN_DOCTOR_STEP_MS", "250");
        assert_eq!(doctor_step_delay_ms(), 250, "explicit override");
        std::env::set_var("ONEBRAIN_DOCTOR_STEP_MS", "garbage");
        assert_eq!(
            doctor_step_delay_ms(),
            90,
            "unparseable falls back to default"
        );
        std::env::remove_var("ONEBRAIN_DOCTOR_STEP_MS");
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

    /// Compact-JSON mode (v3.0 back-compat shape) used by `--json` callers.
    fn legacy_json_compat_mode() -> OutputMode {
        OutputMode::Text {
            color: false,
            pretty: false,
        }
    }

    #[test]
    fn print_report_structured_emits_summary_and_top_level_ok() {
        let results = vec![
            DoctorResult::ok("a", "good"),
            DoctorResult::ok("b", "good"),
            DoctorResult::warn("c", "iffy"),
        ];
        let mut buf = Vec::new();
        print_report_structured(
            &results,
            false,
            vec![],
            true,
            &legacy_json_compat_mode(),
            &mut buf,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        let doc: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        // Top-level `ok` is boolean (no errors → true even with warnings).
        assert_eq!(doc["ok"], true);
        // `summary.passing` is the OK count — not boolean.
        assert_eq!(doc["summary"]["passing"], 2);
        assert_eq!(doc["summary"]["warnings"], 1);
        assert_eq!(doc["summary"]["errors"], 0);
        assert_eq!(doc["summary"]["total"], 3);
        // `fix` key absent when not requested.
        assert!(doc.get("fix").is_none());
    }

    #[test]
    fn print_report_structured_ok_false_when_any_error() {
        let results = vec![DoctorResult::error("a", "broken")];
        let mut buf = Vec::new();
        print_report_structured(
            &results,
            false,
            vec![],
            true,
            &legacy_json_compat_mode(),
            &mut buf,
        )
        .unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(doc["ok"], false);
        assert_eq!(doc["summary"]["errors"], 1);
    }

    #[test]
    fn print_report_structured_emits_empty_fix_array_when_requested_with_no_issues() {
        // `fix[]` must appear (even empty) so consumers can distinguish
        // "user didn't ask to fix" from "user asked but nothing to fix" —
        // schema stability.
        let results = vec![DoctorResult::ok("onebrain.yml", "valid")];
        let mut buf = Vec::new();
        print_report_structured(
            &results,
            true,
            vec![],
            true,
            &legacy_json_compat_mode(),
            &mut buf,
        )
        .unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert!(doc.get("fix").is_some());
        assert_eq!(doc["fix"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn print_report_structured_carries_fix_outcomes_through() {
        let results = vec![DoctorResult::ok("onebrain.yml", "valid")];
        let outcomes = vec![serde_json::json!({
            "check": "qmd-embeddings",
            "outcome": "fixed",
            "message": "qmd embed completed",
        })];
        let mut buf = Vec::new();
        print_report_structured(
            &results,
            true,
            outcomes,
            true,
            &legacy_json_compat_mode(),
            &mut buf,
        )
        .unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(doc["fix"][0]["outcome"], "fixed");
        assert_eq!(doc["fix"][0]["check"], "qmd-embeddings");
    }

    #[test]
    fn print_report_structured_serializes_check_hint_and_details() {
        let results = vec![DoctorResult::warn("c", "iffy")
            .with_hint("Try X")
            .with_details(vec!["d1".into(), "d2".into()])];
        let mut buf = Vec::new();
        print_report_structured(
            &results,
            false,
            vec![],
            true,
            &legacy_json_compat_mode(),
            &mut buf,
        )
        .unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(doc["checks"][0]["status"], "warn");
        assert_eq!(doc["checks"][0]["hint"], "Try X");
        assert_eq!(doc["checks"][0]["details"][1], "d2");
    }

    /// v3.1 regression guard: doctor must honor `--yaml` and emit YAML, not
    /// the bare JSON envelope (the bug the user found in alpha smoke).
    #[test]
    fn print_report_structured_yaml_mode_emits_yaml_not_json() {
        let results = vec![DoctorResult::ok("a", "good")];
        let mut buf = Vec::new();
        print_report_structured(&results, false, vec![], false, &OutputMode::Yaml, &mut buf)
            .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            !s.trim_start().starts_with('{'),
            "yaml mode emitted JSON braces: {s}"
        );
        let v: serde_yaml::Value =
            serde_yaml::from_str(&s).expect("yaml mode output must parse as YAML");
        assert!(v.is_mapping(), "yaml root must be a mapping, got {v:?}");
    }

    /// v3.1 regression guard: doctor must honor `--json --pretty` and indent.
    #[test]
    fn print_report_structured_pretty_json_is_multiline_indented() {
        let results = vec![DoctorResult::ok("a", "good")];
        let mut buf = Vec::new();
        print_report_structured(
            &results,
            false,
            vec![],
            false,
            &OutputMode::Json { pretty: true },
            &mut buf,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains('\n'), "pretty JSON must be multiline: {s}");
        assert!(s.contains("  "), "pretty JSON must be indented: {s}");
        let _v: serde_json::Value =
            serde_json::from_str(s.trim()).expect("still parseable as JSON");
    }

    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn fix_vault_yml_keys_backfills_missing_keys() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "qmd_collection: foo\n").unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => {
                assert!(msg.contains("update_channel"), "msg: {msg}");
                assert!(msg.contains("inbox"), "msg: {msg}");
            }
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
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
            d.path().join("onebrain.yml"),
            "update_channel: stable\nfolders: null\n",
        )
        .unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("inbox"), "msg: {msg}"),
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(after.contains("inbox: 00-inbox"));
        assert!(after.contains("logs: 07-logs"));
    }

    #[test]
    fn fix_vault_yml_keys_strips_deprecated_keys() {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "update_channel: stable\n\
             onebrain_version: \"2.1.0\"\n\
             method: legacy\n\
             runtime:\n  harness: claude-code\n\
             folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
        )
        .unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => {
                assert!(msg.contains("onebrain_version"), "msg: {msg}");
                assert!(msg.contains("method"), "msg: {msg}");
                assert!(msg.contains("runtime.harness"), "msg: {msg}");
            }
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(!after.contains("onebrain_version"), "{after}");
        assert!(!after.contains("method: legacy"), "{after}");
        assert!(!after.contains("harness"), "{after}");
    }

    #[test]
    fn fix_vault_yml_keys_repairs_checkpoint_nums() {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "update_channel: stable\n\
             checkpoint:\n  messages: 0\n  minutes: -5\n\
             folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
        )
        .unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("checkpoint"), "msg: {msg}"),
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(after.contains("messages: 15"));
        assert!(after.contains("minutes: 30"));
    }

    #[test]
    fn fix_vault_yml_keys_idempotent_on_clean_file() {
        let d = tempdir().unwrap();
        let clean = "update_channel: stable\n\
                     folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n";
        fs::write(d.path().join("onebrain.yml"), clean).unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("already"), "msg: {msg}"),
            other => panic!("expected Fixed (no-op), got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
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
        let outcome = fix_claude_settings(d.path(), false);
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
        let outcome = fix_claude_settings(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("already clean"), "msg: {msg}"),
            other => panic!("expected Fixed (no-op), got: {other:?}"),
        }
    }

    /// v3.1 dual-read transition · `vault-config-migration` recipe must
    /// atomically rename `vault.yml` → `onebrain.yml` and leave the rest
    /// of the vault untouched.
    #[test]
    fn fix_vault_config_migration_renames_legacy_to_canonical() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("vault.yml"), "qmd_collection: legacy\n").unwrap();
        let outcome = fix_vault_config_migration(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("renamed"), "msg: {msg}"),
            other => panic!("expected Fixed, got: {other:?}"),
        }
        assert!(d.path().join("onebrain.yml").is_file());
        assert!(!d.path().join("vault.yml").exists());
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, "qmd_collection: legacy\n");
    }

    #[test]
    fn fix_vault_config_migration_idempotent_when_only_canonical_exists() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "qmd_collection: ob\n").unwrap();
        let outcome = fix_vault_config_migration(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => {
                assert!(msg.contains("nothing to migrate"), "msg: {msg}")
            }
            other => panic!("expected Fixed (no-op), got: {other:?}"),
        }
        assert!(d.path().join("onebrain.yml").is_file());
        assert!(!d.path().join("vault.yml").exists());
    }

    #[test]
    fn fix_vault_config_migration_when_both_exist_keeps_canonical() {
        // Split state: canonical wins, legacy removed.
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "canonical: yes\n").unwrap();
        fs::write(d.path().join("vault.yml"), "legacy: yes\n").unwrap();
        let outcome = fix_vault_config_migration(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("removed stale"), "msg: {msg}"),
            other => panic!("expected Fixed, got: {other:?}"),
        }
        // Canonical preserved verbatim · legacy gone.
        let canonical = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(canonical, "canonical: yes\n");
        assert!(!d.path().join("vault.yml").exists());
    }

    #[test]
    fn fix_plugin_files_refuses_filesystem_root() {
        let mut root = std::env::temp_dir();
        while let Some(parent) = root.parent() {
            root = parent.to_path_buf();
        }
        let outcome = fix_plugin_files(&root, false);
        match outcome {
            FixOutcome::Failed(msg) => assert!(msg.contains("filesystem root"), "msg: {msg}"),
            other => panic!("expected Failed (safety guard), got: {other:?}"),
        }
    }

    #[test]
    fn print_report_snapshot_mixed_statuses() {
        let results = vec![
            DoctorResult::ok("onebrain.yml", "valid").with_details(vec!["qmd: ob-1".into()]),
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
