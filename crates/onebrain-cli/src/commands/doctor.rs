//! `onebrain doctor` — run all vault health checks and emit a report.
//!
//! On an interactive TTY (text mode) the checks are revealed one at a time
//! with a short per-step delay so the run reads as sequential work; piped /
//! non-TTY stdout and structured (`--json`/`--yaml`) modes get the instant
//! plain report unchanged. The grouped renderer keeps the approved
//! section/glyph/summary layout (`render_grouped_report`).
//!
//! `--fix` shows the grouped report once, then — after a confirmation prompt
//! on an interactive TTY (auto-yes under `--json`/`--yes`/non-interactive so
//! the `/doctor` skill and cron aren't blocked) — applies targeted auto-repair
//! recipes and prints a compact post-fix verdict footer (not the whole report
//! a second time). The recipes are deliberately narrow (one well-understood
//! action per check); anything ambiguous is reported as "untouched · run the
//! listed command yourself". Declining the prompt leaves the vault untouched.
//!
//! Every run that finds a config file stamps `stats.last_doctor_run` (and,
//! when `--fix` ran, `stats.last_doctor_fix`) into it via a comment-preserving
//! line edit — regardless of whether the checks passed, since the stamp
//! records that doctor *ran*, not that the vault is healthy. This keeps a
//! terminal `onebrain doctor` fresh the same way the `/doctor` plugin skill
//! does. The stamp is best-effort: a failure is noted on stderr (unless
//! `--quiet`) but never changes the exit code, and an already-current value is
//! left untouched.

use crate::legacy_output::serialize_for_mode;
use crate::output::{framing_rule_n, write_framed_header, OutputMode, RULE_WIDTH};
use crate::safety::refuse_dangerous_vault_path;
use crate::vault_ctx;
use anyhow::{anyhow, Result};
use onebrain_core::{load_vault_config, DoctorResult, DoctorStatus};
use onebrain_fs::doctor::run_all_checks;
use std::io::Write;
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
/// produced `DoctorStatus::Error`. With `--fix`, the run is two-pass: initial
/// check, then fix attempts — on the auto-fixable issues after a confirmation
/// prompt in text mode, or every recipe with no prompt in `--json` mode —
/// followed by a final re-check whose result drives the exit code.
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
pub fn run(
    fix: bool,
    json: bool,
    yes: bool,
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    quiet: bool,
) -> Result<i32> {
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
    // Frame width from the PRE-fix report so the deferred `--fix` footer lines
    // up with the header shown above it (both measured from the same results).
    let report_rule_width = doctor_rule_width(&results, &vault_display_name(vault_root.as_path()));
    if !want_structured {
        // Under `--fix` the verdict footer is deferred until after the fix pass
        // (one final footer, not a redundant before-and-after pair); a plain
        // run prints it inline with the report.
        emit_text_report(&results, vault_root.as_path(), mode, quiet, !fix)?;
    }

    let mut any_recipe_failed = false;
    let mut fix_outcomes_json: Vec<serde_json::Value> = Vec::new();
    if fix {
        // Warn + Error are both candidates: some checks (plugin-files,
        // onebrain.yml-keys, folders) emit Error for the very failure modes
        // the recipes repair. `planned_action` classifies which have an
        // automated recipe vs which need a manual step.
        let issues: Vec<&DoctorResult> = results
            .iter()
            .filter(|r| matches!(r.status, DoctorStatus::Warn | DoctorStatus::Error))
            .collect();
        if want_structured {
            // Machine path (the `/doctor` skill drives `--fix --json`): no
            // prompt, run every recipe, capture outcomes for the `fix[]` array.
            if !issues.is_empty() {
                let outcomes: Vec<(String, FixOutcome)> = issues
                    .iter()
                    .map(|r| (r.check.clone(), attempt_fix(r, vault_root.as_path(), true)))
                    .collect();
                any_recipe_failed = outcomes
                    .iter()
                    .any(|(_, o)| matches!(o, FixOutcome::Failed(_)));
                fix_outcomes_json = outcomes
                    .iter()
                    .map(|(check, o)| {
                        let (outcome, message) = match o {
                            FixOutcome::Fixed(m) => ("fixed", m.as_str()),
                            FixOutcome::Failed(m) => ("failed", m.as_str()),
                            FixOutcome::Manual(m) => ("manual", m.as_str()),
                        };
                        serde_json::json!({ "check": check, "outcome": outcome, "message": message })
                    })
                    .collect();
                results = run_all_checks(vault_root.as_path(), &config);
            }
        } else {
            // Text path: preview the plan, confirm, then apply only the
            // auto-fixable recipes (manual-only issues never trigger a prompt).
            let auto: Vec<&DoctorResult> = issues
                .iter()
                .copied()
                .filter(|r| planned_action(r).is_some())
                .collect();
            let manual: Vec<&DoctorResult> = issues
                .iter()
                .copied()
                .filter(|r| planned_action(r).is_none())
                .collect();

            if issues.is_empty() {
                println!("\nNothing to fix — all checks pass.");
            } else {
                if !auto.is_empty() {
                    println!("\nWill apply {} automated fix(es):", auto.len());
                    for r in &auto {
                        println!(
                            "  • {} — {}",
                            display_label(&r.check),
                            planned_action(r).unwrap_or("")
                        );
                    }
                }
                if !manual.is_empty() {
                    println!(
                        "\n{} issue(s) need a manual step (no automated fix):",
                        manual.len()
                    );
                    for r in &manual {
                        let hint = r.hint.as_deref().unwrap_or(r.message.as_str());
                        println!("  · {} — {}", display_label(&r.check), hint);
                    }
                }
                if auto.is_empty() {
                    // Nothing to auto-apply — don't ask a misleading "Apply
                    // fixes?" when confirming would change nothing.
                    println!("\nNothing to auto-fix — see the manual steps above.");
                } else if confirm_fix(auto.len(), false, yes) {
                    let outcomes: Vec<(String, FixOutcome)> = auto
                        .iter()
                        .map(|r| (r.check.clone(), attempt_fix(r, vault_root.as_path(), false)))
                        .collect();
                    any_recipe_failed = outcomes
                        .iter()
                        .any(|(_, o)| matches!(o, FixOutcome::Failed(_)));
                    print_fix_summary(&outcomes);
                    results = run_all_checks(vault_root.as_path(), &config);
                } else {
                    println!("\nNo changes made.");
                }
            }
            // Single deferred verdict footer — the pre-fix report omitted its
            // own footer so this is the only one the user sees. Uses the
            // pre-fix frame width so it lines up with the header above.
            println!();
            let color = crate::output::is_color_text(mode);
            write_summary_footer(&mut std::io::stdout(), &results, color, report_rule_width)?;
        }
    }

    // Stamp the run timestamp now that every check (and any --fix recipe) has
    // settled and the config file is at its final location. Best-effort — the
    // exit code below reflects check/recipe results only, never the stamp.
    stamp_doctor_run(vault_root.as_path(), fix, quiet);

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

/// Decide whether the text-mode `--fix` should apply its recipes.
///
/// `structured` short-circuits to `true` as a defensive guard — production
/// routes `--fix --json` through the separate structured branch in `run()` and
/// never calls this; `--yes` short-circuits too. Otherwise prompt on an
/// interactive TTY. A non-interactive plain run (piped stdin/stdout — e.g. cron
/// without `--yes`) proceeds without prompting, matching pre-3.2.4 behaviour so
/// existing automation keeps working. A read error is treated as "decline".
fn confirm_fix(fixable_count: usize, structured: bool, yes: bool) -> bool {
    use std::io::{IsTerminal, Write};
    if structured || yes {
        return true;
    }
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return true;
    }
    // The plan (the bulleted action list) is printed by the caller just above
    // this prompt, so keep the question itself short.
    print!("\nApply {fixable_count} fix(es)? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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
        // Create any missing standard folders on disk, named from onebrain.yml
        // (so a customised `folders:` layout gets the right directories).
        "folders" => fix_folders(vault_root, json),
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

/// Describe what `--fix` would do to an issue WITHOUT executing it, so the
/// plan can be previewed before the confirmation prompt. `Some(action)` for an
/// auto-fixable check, `None` when only a manual step applies (e.g. `qmd`
/// collection not set, orphan checkpoints).
///
/// Keep the match arms in sync with [`attempt_fix`] — same check names and the
/// same `qmd-embeddings` "unembedded" message guard.
fn planned_action(result: &DoctorResult) -> Option<&'static str> {
    match result.check.as_str() {
        "qmd-embeddings" if result.message.contains("unembedded") => {
            Some("re-embed pending documents")
        }
        "settings-hooks" => Some("register the Stop + qmd hooks and permissions"),
        "plugin-files" => Some("re-download plugin files from upstream"),
        "folders" => Some("create the missing standard folders"),
        "onebrain.yml-keys" => Some("backfill missing onebrain.yml keys"),
        "claude-settings" => Some("remove the stale marketplace entry"),
        "vault-config-migration" => Some("migrate vault.yml → onebrain.yml"),
        _ => None,
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

/// Recipe — `folders` error means one or more standard vault folders are
/// missing on disk. Create them, using the folder names from `onebrain.yml`
/// (the `FoldersCheck` checks those exact names, so a customised `folders:`
/// layout gets the right directories), plus the `00-inbox/imports` subdir
/// that `init` creates. Additive and idempotent — never deletes or renames.
fn fix_folders(vault_root: &Path, json: bool) -> FixOutcome {
    use onebrain_core::load_vault_config_at;
    if let Err(e) = refuse_dangerous_vault_path(vault_root) {
        return FixOutcome::Failed(e.to_string());
    }
    status_line(json, "running: create folders");
    let config = load_vault_config_at(vault_root).unwrap_or_else(|_| onebrain_core::VaultConfig {
        qmd_collection: None,
        checkpoint: Default::default(),
        folders: Default::default(),
    });
    let f = &config.folders;
    // Order mirrors FoldersCheck so the post-fix re-check reports 8/8.
    let names = [
        &f.inbox,
        &f.projects,
        &f.areas,
        &f.knowledge,
        &f.resources,
        &f.agent,
        &f.archive,
        &f.logs,
    ];
    let mut created: Vec<String> = Vec::new();
    for name in names {
        let path = vault_root.join(name.as_str());
        if !path.is_dir() {
            if let Err(e) = std::fs::create_dir_all(&path) {
                return FixOutcome::Failed(format!("create {name}: {e}"));
            }
            created.push(name.clone());
        }
    }
    // Match `init`: ensure the inbox staging subdir exists too. Surface a
    // failure rather than swallow it — `FoldersCheck` doesn't inspect
    // `imports/`, so a silent miss here would be invisible end-to-end.
    let imports = vault_root.join(f.inbox.as_str()).join("imports");
    if !imports.is_dir() {
        if let Err(e) = std::fs::create_dir_all(&imports) {
            return FixOutcome::Failed(format!("create {}/imports: {e}", f.inbox));
        }
        created.push(format!("{}/imports", f.inbox));
    }
    if created.is_empty() {
        FixOutcome::Fixed("all standard folders already present".to_string())
    } else {
        FixOutcome::Fixed(format!("created {}: {}", created.len(), created.join(", ")))
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

/// True when the config's top-level `stats` key is an inline mapping or
/// scalar (`stats: {…}` / `stats: null`) rather than the block form that
/// [`upsert_doctor_stats`] can extend — such a file is left untouched.
fn config_has_inline_stats(text: &str) -> bool {
    text.lines().any(|l| {
        let indented = l.starts_with(' ') || l.starts_with('\t');
        !indented && l.starts_with("stats:") && l.trim_end() != "stats:"
    })
}

/// Upsert `stats.last_doctor_run: <today>` (and `stats.last_doctor_fix`
/// when `also_fix`) into raw config text, returning `Some(new_text)` when a
/// change was made and `None` when the file already carries today's
/// value(s) — so a same-day re-run never rewrites the file or touches its
/// mtime.
///
/// Deliberately a line edit rather than a `serde_yaml` round-trip: a plain
/// read-only `onebrain doctor` stamps the timestamp on every run, so it must
/// preserve the user's comments and key order and touch only the timestamp
/// line(s). Child indentation is matched from any existing `stats:` child,
/// defaulting to two spaces. An inline `stats: {…}` mapping is left
/// untouched (returns `None`) rather than risk corrupting it.
fn upsert_doctor_stats(text: &str, today: &str, also_fix: bool) -> Option<String> {
    let is_indented = |l: &str| l.starts_with(' ') || l.starts_with('\t');
    // Preserve the file's existing line ending (CRLF on Windows-authored
    // configs) — `lines()` strips the `\r`, so we rejoin with whichever the
    // file used rather than silently normalising every line to LF.
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let ends_with_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    let keys: &[&str] = if also_fix {
        &["last_doctor_run", "last_doctor_fix"]
    } else {
        &["last_doctor_run"]
    };

    // The top-level `stats:` block header (zero indentation, block form).
    let stats_idx = lines
        .iter()
        .position(|l| !is_indented(l) && l.trim_end() == "stats:");

    // A top-level inline `stats: {…}` (or `stats: null`) can't take block
    // children — refuse rather than corrupt it (only when there is no
    // block-form header to edit).
    if stats_idx.is_none() && config_has_inline_stats(text) {
        return None;
    }

    let mut changed = false;
    match stats_idx {
        Some(si) => {
            // Block extent: subsequent blank or indented lines.
            let mut end = si + 1;
            while end < lines.len() && (lines[end].is_empty() || is_indented(&lines[end])) {
                end += 1;
            }
            // Trailing blank lines belong after the block, not in it — back
            // `end` up past them so an inserted key sits beside the existing
            // children rather than below a blank separator before a sibling.
            while end > si + 1 && lines[end - 1].is_empty() {
                end -= 1;
            }
            // Match an existing child's indentation; default two spaces.
            let indent = lines[si + 1..end]
                .iter()
                .find(|l| is_indented(l))
                .map(|l| l[..l.len() - l.trim_start().len()].to_string())
                .unwrap_or_else(|| "  ".to_string());

            for key in keys {
                let prefix = format!("{key}:");
                let desired = format!("{indent}{key}: {today}");
                match (si + 1..end).find(|&j| lines[j].trim_start().starts_with(&prefix)) {
                    Some(j) if lines[j] == desired => {}
                    Some(j) => {
                        lines[j] = desired;
                        changed = true;
                    }
                    None => {
                        lines.insert(end, desired);
                        end += 1;
                        changed = true;
                    }
                }
            }
        }
        None => {
            lines.push("stats:".to_string());
            for key in keys {
                lines.push(format!("  {key}: {today}"));
            }
            changed = true;
        }
    }

    if !changed {
        return None;
    }
    let mut out = lines.join(newline);
    if ends_with_newline {
        out.push_str(newline);
    }
    Some(out)
}

/// Stamp `last_doctor_run` (and `last_doctor_fix` when `--fix` ran) into the
/// active config file. Best-effort: it never changes the doctor exit code —
/// the timestamp is a convenience, not a check result. No config file ⇒ no-op
/// (doctor never creates `onebrain.yml`; that is `init`'s job).
///
/// Failures and the inline-`stats:` skip emit a one-line stderr note unless
/// `--quiet`. Stderr is used (not the structured envelope) so the note is
/// visible in `--json`/`--yaml` runs too without disturbing the single JSON
/// document on stdout — matching the existing config-load warning.
fn stamp_doctor_run(vault_root: &Path, fix: bool, quiet: bool) {
    use onebrain_core::find_config_file;
    let Some(path) = find_config_file(vault_root) else {
        return; // no config file ⇒ nothing to stamp (init's job, not doctor's)
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            if !quiet {
                eprintln!(
                    "doctor: could not read {} to stamp last_doctor_run: {e}",
                    path.display()
                );
            }
            return;
        }
    };
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    match upsert_doctor_stats(&text, &today, fix) {
        Some(updated) => {
            if let Err(e) = atomic_write_text(&path, &updated) {
                if !quiet {
                    eprintln!("doctor: could not update last_doctor_run: {e}");
                }
            }
        }
        // `None` is the common "already today" no-op, but it also covers a
        // refusal to edit an inline `stats:` mapping — surface that case so a
        // never-advancing timestamp doesn't look like a silent bug.
        None if !quiet && config_has_inline_stats(&text) => {
            eprintln!(
                "doctor: `stats` in {} is an inline mapping; last_doctor_run not stamped — convert it to a block to enable stamping",
                path.display()
            );
        }
        None => {}
    }
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

// ─────────────────────────────────────────────────────────────────────────
// Grouped doctor report (v3.2.1) — checks bucketed into 4 sections and
// rendered through the shared braille-spinner progress primitive. Passes are
// quiet; warnings / fails are prominent with their hint as the indented `└`
// line. Structured (`--json`/`--yaml`) output is unchanged — this is purely
// the human text/TTY surface.
// ─────────────────────────────────────────────────────────────────────────

/// The approved 4-section grouping of the 9 checks, in display order. Each
/// entry is `(section header, [check names in order])`. Check names are the
/// stable `DoctorResult::check` identifiers produced by the check modules.
const DOCTOR_SECTIONS: [(&str, &[&str]); 4] = [
    (
        "Config",
        &[
            "onebrain.yml",
            "onebrain.yml-keys",
            "vault-config-migration",
        ],
    ),
    ("Vault structure", &["folders", "plugin-files"]),
    ("Integration", &["settings-hooks", "claude-settings"]),
    ("Index & state", &["orphan-checkpoints", "qmd-embeddings"]),
];

/// Short, scannable display label for a check name (matches the approved
/// layout). Unknown checks fall back to their raw name so a future check
/// still renders something sensible before its label is added here.
fn display_label(check: &str) -> &str {
    match check {
        "onebrain.yml" => "onebrain.yml",
        "onebrain.yml-keys" => "schema",
        "vault-config-migration" => "config migration",
        "folders" => "folders",
        "plugin-files" => "plugin files",
        "settings-hooks" => "hooks",
        "claude-settings" => "claude settings",
        "orphan-checkpoints" => "checkpoints",
        "qmd-embeddings" => "qmd",
        other => other,
    }
}

/// Map a check's tri-state into the progress primitive's [`StepStatus`].
fn step_status_of(status: DoctorStatus) -> crate::output::StepStatus {
    use crate::output::StepStatus;
    match status {
        DoctorStatus::Ok => StepStatus::Ok,
        DoctorStatus::Warn => StepStatus::Warn,
        DoctorStatus::Error => StepStatus::Fail,
    }
}

/// Bucket `results` into the 4 display sections as [`Section`]s of [`Step`]s.
///
/// - Detail = the check's `message` (the right-hand status text).
/// - Hint = the check's `hint`, but only carried for warn / fail (passes stay
///   quiet per the design; the primitive also drops hints on OK steps).
/// - Any result whose check name isn't in [`DOCTOR_SECTIONS`] is appended to a
///   trailing "Other" section so nothing is silently dropped — a defensive
///   guard against a new check landing without a section assignment.
fn build_sections(results: &[DoctorResult]) -> Vec<crate::output::Section> {
    use crate::output::{Section, Step};

    let to_step = |r: &DoctorResult| {
        let hint = match r.status {
            DoctorStatus::Ok => None,
            _ => r.hint.clone(),
        };
        Step::new(
            display_label(&r.check),
            step_status_of(r.status),
            Some(r.message.clone()),
            hint,
        )
    };

    let mut sections: Vec<Section> = Vec::with_capacity(DOCTOR_SECTIONS.len());
    let mut placed: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for (header, checks) in DOCTOR_SECTIONS {
        let mut steps = Vec::new();
        for name in checks {
            if let Some(r) = results.iter().find(|r| r.check == *name) {
                steps.push(to_step(r));
                placed.insert(*name);
            }
        }
        if !steps.is_empty() {
            sections.push(Section::new(header, steps));
        }
    }

    // Defensive: surface any unmapped check rather than dropping it.
    let leftovers: Vec<Step> = results
        .iter()
        .filter(|r| !placed.contains(r.check.as_str()))
        .map(to_step)
        .collect();
    if !leftovers.is_empty() {
        sections.push(Section::new("Other", leftovers));
    }

    sections
}

/// Render the summary footer: a rule, the verdict line (overall glyph, the
/// ok/warn/fail counts, and the total), an optional `--fix` next-action line
/// when there are repairable issues, then a closing rule.
fn write_summary_footer<W: Write>(
    w: &mut W,
    results: &[DoctorResult],
    color: bool,
    rule_width: usize,
) -> Result<()> {
    use crate::output::StepStatus;
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

    // Overall verdict glyph: fail dominates, then warn, then ok.
    let verdict = if errors > 0 {
        StepStatus::Fail
    } else if warnings > 0 {
        StepStatus::Warn
    } else {
        StepStatus::Ok
    };
    let (dim, reset) = if color {
        ("\x1b[2m", "\x1b[0m")
    } else {
        ("", "")
    };
    let glyph_prefix = verdict.ansi_prefix(color);
    let glyph_reset = if color { "\x1b[0m" } else { "" };

    // "fail" has no plural form; "warning" does.
    let warnings_word = if warnings == 1 { "warning" } else { "warnings" };

    // Build the verdict line so the total sits flush against the rule's right
    // edge. The left segment is ` <glyph>  <counts>`; the glyph and the `·`
    // separators are all single-column, so the visible length is computable
    // from the uncoloured text (ANSI escapes around the glyph add no columns).
    let counts = format!("{passing} ok · {warnings} {warnings_word} · {errors} fail");
    let total_str = format!("{total} checks");
    // 1 leading space + 1 glyph column + 2 spaces + counts.
    let left_cols = 1 + 1 + 2 + counts.chars().count();
    // At least 2 spaces between counts and total even if the line is wide.
    let gap = rule_width
        .saturating_sub(left_cols + total_str.chars().count())
        .max(2);
    let rule = framing_rule_n(rule_width);

    writeln!(w)?;
    writeln!(w, "{dim}{rule}{reset}")?;
    writeln!(
        w,
        " {glyph_prefix}{glyph}{glyph_reset}  {counts}{pad}{total_str}",
        glyph = verdict.glyph(),
        pad = " ".repeat(gap),
    )?;
    // `--fix` next-action — shown only when at least one issue has an
    // automated recipe. A manual-only remainder (e.g. `qmd_collection not set`)
    // gets no pointer, since `--fix` can't repair it.
    let any_auto_fixable = results.iter().any(|r| {
        matches!(r.status, DoctorStatus::Warn | DoctorStatus::Error) && planned_action(r).is_some()
    });
    if any_auto_fixable {
        writeln!(w, " →  onebrain doctor --fix  to auto-repair")?;
    }
    writeln!(w, "{dim}{rule}{reset}")?;
    Ok(())
}

/// Render the full grouped report to `w` via a [`ProgressRenderer`].
///
/// `force_static` drives the gating seam: when false the renderer is built
/// from live stdout + `mode` + `quiet` (animates only for a colour TTY,
/// non-quiet, text mode); when true it never animates (deterministic tests).
/// `color` is the resolved colour bit (header/footer styling).
///
/// This is the single rendering entry point used for BOTH the initial report
/// and the post-`--fix` re-render, so they share one layout.
///
/// `show_footer` is false for the pre-fix report under `--fix`: the verdict
/// footer is deferred until after the fix pass so the run shows exactly one
/// (final) footer instead of a redundant before-and-after pair.
fn render_grouped_report<W: Write>(
    mut w: W,
    results: &[DoctorResult],
    vault_name: &str,
    color: bool,
    animate: bool,
    show_footer: bool,
) -> Result<()> {
    use crate::output::ProgressRenderer;
    // Widen the frame so the rules span the longest line (e.g. a "Missing:
    // 00-inbox, 01-projects, …" hint), instead of stopping short of the text.
    let rule_width = doctor_rule_width(results, vault_name);
    // Doctor's own header (distinct from the brand wordmark banner on stderr),
    // via the shared framed-header helper that `update` also uses.
    write_framed_header(
        &mut w,
        "🔬",
        &format!("OneBrain Doctor · {vault_name}"),
        color,
        rule_width,
    )?;
    let sections = build_sections(results);
    {
        // force_static = !animate.
        let mut renderer = ProgressRenderer::with_writer(&mut w, !animate, color);
        for section in &sections {
            renderer.render_section(section)?;
        }
    }
    if show_footer {
        write_summary_footer(&mut w, results, color, rule_width)?;
    }
    Ok(())
}

/// Width for the doctor frame rules: the widest rendered body line (section
/// headers, check lines, hint `└` lines) and the header title, clamped to a
/// [`RULE_WIDTH`] floor. Measured by rendering the body uncoloured to a buffer
/// so ANSI escapes don't inflate the count; the body's glyphs (`✓ ⚠ ✗ └ ─`)
/// are single-column, so a `char` count equals the display width. The 🔬 in the
/// header title renders as two columns, accounted for explicitly.
fn doctor_rule_width(results: &[DoctorResult], vault_name: &str) -> usize {
    use crate::output::ProgressRenderer;
    let sections = build_sections(results);
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut renderer = ProgressRenderer::with_writer(&mut buf, true, false);
        for section in &sections {
            let _ = renderer.render_section(section);
        }
    }
    let body_max = String::from_utf8_lossy(&buf)
        .lines()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0);
    // Header line is ` 🔬  <title>`: 1 lead space + 2 emoji columns + 2 spaces.
    let header_cols = 5 + format!("OneBrain Doctor · {vault_name}").chars().count();
    body_max.max(header_cols).max(RULE_WIDTH)
}

/// Derive the human vault name from its root path (the final path component),
/// falling back to "vault" for an unnamed root.
fn vault_display_name(vault_root: &Path) -> String {
    vault_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("vault")
        .to_string()
}

/// Emit the grouped text report to stdout. Animates step-by-step only on a
/// colour, non-quiet, interactive terminal (the [`ProgressRenderer`] gate);
/// piped / non-TTY / structured / `--no-color` / `--quiet` get the instant
/// static layout.
///
/// The gating decision (and colour bit) come from the pure
/// [`crate::output::should_animate`] / [`crate::output::is_color_text`]
/// helpers — the single source of truth for "should this animate?" shared
/// with [`ProgressRenderer::new`].
fn emit_text_report(
    results: &[DoctorResult],
    vault_root: &Path,
    mode: &OutputMode,
    quiet: bool,
    show_footer: bool,
) -> Result<()> {
    use crate::output::{is_color_text, should_animate};
    use std::io::IsTerminal;
    // Compute the gating decision directly — no throwaway renderer round-trip.
    let animate = should_animate(mode, std::io::stdout().is_terminal(), quiet);
    let color = is_color_text(mode);
    let name = vault_display_name(vault_root);
    render_grouped_report(
        std::io::stdout(),
        results,
        &name,
        color,
        animate,
        show_footer,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_core::DoctorResult;

    /// Build the canonical 9-check fixture in the real check-name order, with
    /// a couple of warnings/fails so grouping + footer logic is exercised.
    fn nine_check_results() -> Vec<DoctorResult> {
        vec![
            DoctorResult::ok("onebrain.yml", "valid · stable · qmd ob-1"),
            DoctorResult::ok("onebrain.yml-keys", "all keys ok"),
            DoctorResult::ok("vault-config-migration", "onebrain.yml in use"),
            DoctorResult::ok("folders", "8/8 present"),
            DoctorResult::ok("plugin-files", "complete"),
            DoctorResult::warn("settings-hooks", "PostToolUse (qmd) duplicated (×2)")
                .with_hint("onebrain doctor --fix"),
            DoctorResult::ok("claude-settings", "ok"),
            DoctorResult::ok("orphan-checkpoints", "0 orphans"),
            DoctorResult::warn("qmd-embeddings", "3 unembedded").with_hint("qmd embed"),
        ]
    }

    fn render_static_report(results: &[DoctorResult], color: bool) -> String {
        let mut buf = Vec::new();
        render_grouped_report(&mut buf, results, "ob-1", color, false, true).unwrap();
        String::from_utf8(buf).unwrap()
    }

    // ── Section grouping ─────────────────────────────────────────────────

    #[test]
    fn build_sections_assigns_each_check_to_its_section() {
        let results = nine_check_results();
        let sections = build_sections(&results);
        assert_eq!(sections.len(), 4, "expected 4 sections");
        let by_header: std::collections::HashMap<&str, Vec<&str>> = sections
            .iter()
            .map(|s| {
                (
                    s.header.as_str(),
                    s.steps.iter().map(|st| st.label.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            by_header["Config"],
            vec!["onebrain.yml", "schema", "config migration"]
        );
        assert_eq!(
            by_header["Vault structure"],
            vec!["folders", "plugin files"]
        );
        assert_eq!(by_header["Integration"], vec!["hooks", "claude settings"]);
        assert_eq!(by_header["Index & state"], vec!["checkpoints", "qmd"]);
    }

    #[test]
    fn build_sections_surfaces_unmapped_check_in_other() {
        let mut results = nine_check_results();
        results.push(DoctorResult::warn("brand-new-check", "hmm"));
        let sections = build_sections(&results);
        let other = sections.iter().find(|s| s.header == "Other");
        assert!(other.is_some(), "unmapped check must land in Other");
        assert_eq!(other.unwrap().steps[0].label, "brand-new-check");
    }

    #[test]
    fn build_sections_carries_hint_only_for_non_ok() {
        let results = vec![
            DoctorResult::ok("onebrain.yml", "valid").with_hint("ignored on ok"),
            DoctorResult::warn("settings-hooks", "dup").with_hint("onebrain doctor --fix"),
        ];
        let sections = build_sections(&results);
        let ok_step = &sections
            .iter()
            .flat_map(|s| &s.steps)
            .find(|st| st.label == "onebrain.yml")
            .unwrap();
        assert!(ok_step.hint.is_none(), "OK steps must not carry a hint");
        let warn_step = sections
            .iter()
            .flat_map(|s| &s.steps)
            .find(|st| st.label == "hooks")
            .unwrap();
        assert_eq!(warn_step.hint.as_deref(), Some("onebrain doctor --fix"));
    }

    // ── Static rendered output: glyphs / labels / hints ──────────────────

    #[test]
    fn static_report_shows_header_section_labels_and_glyphs() {
        let out = render_static_report(&nine_check_results(), false);
        // Doctor's own header (distinct from the brand banner). Two spaces
        // after the wide 🔬 glyph so the title doesn't butt against it.
        assert!(
            out.contains("🔬  OneBrain Doctor · ob-1"),
            "header: {out:?}"
        );
        // Section headers.
        for header in ["Config", "Vault structure", "Integration", "Index & state"] {
            assert!(out.contains(header), "section {header}: {out:?}");
        }
        // OK glyph + short label + detail.
        assert!(out.contains("✓ onebrain.yml"), "ok line: {out:?}");
        assert!(out.contains("✓ schema"), "schema line: {out:?}");
        // Warn glyph + label + the indented hint line.
        assert!(out.contains("⚠ hooks"), "warn line: {out:?}");
        assert!(
            out.contains("└ onebrain doctor --fix"),
            "warn hint line: {out:?}"
        );
        assert!(out.contains("└ qmd embed"), "qmd hint line: {out:?}");
    }

    #[test]
    fn static_report_passes_have_no_hint_line() {
        // A pass that happens to carry a hint must stay quiet.
        let results = vec![DoctorResult::ok("onebrain.yml", "valid").with_hint("never-shown-hint")];
        let out = render_static_report(&results, false);
        assert!(
            !out.contains("never-shown-hint"),
            "pass hint leaked: {out:?}"
        );
        assert!(!out.contains("└"), "pass must not show └ line: {out:?}");
    }

    #[test]
    fn static_report_emits_no_spinner_or_carriage_return() {
        let out = render_static_report(&nine_check_results(), true);
        assert!(!out.contains('\r'), "static must not redraw: {out:?}");
        for f in crate::output::SPINNER_FRAMES {
            assert!(!out.contains(f), "static must not paint spinner: {out:?}");
        }
    }

    // ── Summary footer: counts + verdict + --fix action ──────────────────

    #[test]
    fn footer_counts_and_warn_verdict_with_fix_action() {
        let out = render_static_report(&nine_check_results(), false);
        // 7 ok · 2 warnings · 0 fail · 9 checks (matches the approved layout).
        assert!(
            out.contains("7 ok · 2 warnings · 0 fail"),
            "counts: {out:?}"
        );
        assert!(out.contains("9 checks"), "total: {out:?}");
        // Warn verdict glyph present.
        assert!(out.contains("⚠"), "verdict glyph: {out:?}");
        // Fixable issues → --fix next-action shown.
        assert!(
            out.contains("onebrain doctor --fix  to auto-repair"),
            "fix action: {out:?}"
        );
    }

    #[test]
    fn footer_rule_spans_the_verdict_line() {
        // The rule must be at least as wide as the verdict it frames ("extend
        // the line to cover the text"). Mono mode has no ANSI escapes, so char
        // counts equal visible columns.
        let out = render_static_report(&nine_check_results(), false);
        let lines: Vec<&str> = out.lines().collect();
        let rule_len = lines
            .iter()
            .find(|l| !l.is_empty() && l.chars().all(|c| c == '─'))
            .map(|l| l.chars().count())
            .expect("a rule line of box dashes");
        // v3.2.4: the rule is at least the default width, and widens to span
        // the longest content line so the frame never stops short of the text.
        assert!(
            rule_len >= RULE_WIDTH,
            "rule at least the default width · got {rule_len}"
        );
        let widest_content = lines
            .iter()
            .filter(|l| !l.is_empty() && !l.chars().all(|c| c == '─'))
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0);
        assert!(
            rule_len >= widest_content,
            "rule ({rule_len}) must cover the widest content line ({widest_content})"
        );
        let verdict = lines
            .iter()
            .find(|l| l.contains("ok ·") && l.contains("checks"))
            .expect("verdict line");
        assert!(
            verdict.chars().count() <= rule_len,
            "verdict ({}) must not exceed rule ({rule_len}): {verdict:?}",
            verdict.chars().count(),
        );
        // Total is right-aligned flush to the rule's right edge.
        assert!(
            verdict.trim_end().ends_with("9 checks"),
            "total not right-aligned: {verdict:?}"
        );
    }

    #[test]
    fn footer_all_ok_shows_check_verdict_and_no_fix_action() {
        let results: Vec<DoctorResult> = nine_check_results()
            .into_iter()
            .map(|r| DoctorResult::ok(r.check, "ok"))
            .collect();
        let out = render_static_report(&results, false);
        assert!(
            out.contains("9 ok · 0 warnings · 0 fail"),
            "counts: {out:?}"
        );
        // All-clean → no --fix pointer.
        assert!(
            !out.contains("to auto-repair"),
            "clean run must not show --fix action: {out:?}"
        );
    }

    #[test]
    fn footer_fail_verdict_when_any_error() {
        let mut results = nine_check_results();
        results[3] =
            DoctorResult::error("folders", "0/8 present").with_hint("onebrain init --force");
        let out = render_static_report(&results, false);
        assert!(out.contains("✗ folders"), "fail line: {out:?}");
        assert!(out.contains("· 1 fail"), "fail count: {out:?}");
        assert!(out.contains("✗"), "fail verdict glyph: {out:?}");
        assert!(
            out.contains("to auto-repair"),
            "fix action still shown: {out:?}"
        );
    }

    // ── TTY-gating decision (doctor's emit path) ─────────────────────────

    #[test]
    fn doctor_animates_only_for_color_tty_non_quiet() {
        use crate::output::should_animate;
        let color_tty = OutputMode::Text {
            color: true,
            pretty: true,
        };
        assert!(
            should_animate(&color_tty, true, false),
            "color TTY non-quiet"
        );
        assert!(!should_animate(&color_tty, true, true), "quiet off");
        assert!(!should_animate(&color_tty, false, false), "non-tty off");
        let mono = OutputMode::Text {
            color: false,
            pretty: true,
        };
        assert!(!should_animate(&mono, true, false), "no-color off");
        // v3.2.15: Table / Tsv variants dropped from `OutputMode`.
        for structured in [OutputMode::Json { pretty: true }, OutputMode::Yaml] {
            assert!(
                !should_animate(&structured, true, false),
                "structured {structured:?} off"
            );
        }
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
    fn fix_folders_creates_missing_standard_folders() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "update_channel: stable\n").unwrap();
        fs::create_dir_all(d.path().join("00-inbox")).unwrap();
        fs::create_dir_all(d.path().join("01-projects")).unwrap();
        let outcome = fix_folders(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("created"), "msg: {msg}"),
            other => panic!("expected Fixed, got: {other:?}"),
        }
        for name in [
            "00-inbox",
            "01-projects",
            "02-areas",
            "03-knowledge",
            "04-resources",
            "05-agent",
            "06-archive",
            "07-logs",
        ] {
            assert!(d.path().join(name).is_dir(), "missing folder {name}");
        }
        assert!(d.path().join("00-inbox/imports").is_dir(), "inbox/imports");
    }

    #[test]
    fn fix_folders_uses_custom_names_from_config() {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "folders:\n  inbox: my-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
        )
        .unwrap();
        let outcome = fix_folders(d.path(), false);
        assert!(matches!(outcome, FixOutcome::Fixed(_)));
        assert!(
            d.path().join("my-inbox").is_dir(),
            "custom inbox name created"
        );
        assert!(
            !d.path().join("00-inbox").exists(),
            "default name must not be created when overridden"
        );
    }

    #[test]
    fn fix_folders_idempotent_when_all_present() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "update_channel: stable\n").unwrap();
        for name in [
            "00-inbox",
            "01-projects",
            "02-areas",
            "03-knowledge",
            "04-resources",
            "05-agent",
            "06-archive",
            "07-logs",
        ] {
            fs::create_dir_all(d.path().join(name)).unwrap();
        }
        fs::create_dir_all(d.path().join("00-inbox/imports")).unwrap();
        let outcome = fix_folders(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("already present"), "msg: {msg}"),
            other => panic!("expected Fixed no-op, got: {other:?}"),
        }
    }

    // ── --fix confirmation gate (v3.2.4) ─────────────────────────────────

    #[test]
    fn confirm_fix_auto_yes_in_structured_mode() {
        // The /doctor skill drives `--fix --json` — it must never block.
        assert!(confirm_fix(3, true, false));
    }

    #[test]
    fn confirm_fix_auto_yes_with_yes_flag() {
        assert!(confirm_fix(3, false, true));
    }

    #[test]
    fn confirm_fix_proceeds_when_non_interactive() {
        // Under `cargo test` stdin/stdout aren't TTYs → no prompt, proceed
        // (matches pre-3.2.4 cron/piped behaviour). The interactive decline
        // path needs a real TTY and is verified manually.
        assert!(confirm_fix(3, false, false));
    }

    #[test]
    fn planned_action_classifies_auto_vs_manual() {
        // Locks the invariant the doc comment asks a human to maintain:
        // `planned_action` must agree with `attempt_fix`'s routing. The
        // `qmd-embeddings` "unembedded" guard is the one message-dependent arm.
        assert!(planned_action(&DoctorResult::warn("qmd-embeddings", "3 unembedded")).is_some());
        assert!(planned_action(&DoctorResult::warn(
            "qmd-embeddings",
            "qmd_collection not set"
        ))
        .is_none());
        // Manual-only check → no automated action.
        assert!(planned_action(&DoctorResult::warn("orphan-checkpoints", "2 orphans")).is_none());
        // Representative auto-fixable check.
        assert!(planned_action(&DoctorResult::error("folders", "0/8 present")).is_some());
    }

    /// Lock the full grouped (static, monochrome) layout so any drift in
    /// header / section grouping / glyphs / footer surfaces in
    /// `cargo insta review`. Monochrome so the snapshot has no ANSI noise.
    #[test]
    fn grouped_report_snapshot_mixed_statuses() {
        let results = vec![
            DoctorResult::ok("onebrain.yml", "valid · stable · qmd ob-1"),
            DoctorResult::ok("onebrain.yml-keys", "all keys ok"),
            DoctorResult::ok("vault-config-migration", "onebrain.yml in use"),
            DoctorResult::error("folders", "7/8 present").with_hint("onebrain init --force"),
            DoctorResult::ok("plugin-files", "complete"),
            DoctorResult::warn("settings-hooks", "PostToolUse (qmd) duplicated (×2)")
                .with_hint("onebrain doctor --fix"),
            DoctorResult::ok("claude-settings", "ok"),
            DoctorResult::ok("orphan-checkpoints", "0 orphans"),
            DoctorResult::ok("qmd-embeddings", "602 indexed · 0 unembedded"),
        ];
        let mut buf = Vec::new();
        render_grouped_report(&mut buf, &results, "ob-1", false, false, true).unwrap();
        let output = String::from_utf8(buf).unwrap();
        insta::assert_snapshot!(output);
    }

    // ---- last_doctor_run / last_doctor_fix stamping (v3.2.3) ----

    #[test]
    fn upsert_creates_stats_block_when_absent() {
        let out = upsert_doctor_stats("qmd_collection: ob\n", "2026-05-27", false).unwrap();
        assert!(out.contains("qmd_collection: ob"), "preserved: {out}");
        assert!(
            out.contains("stats:\n  last_doctor_run: 2026-05-27"),
            "added: {out}"
        );
        assert!(
            !out.contains("last_doctor_fix"),
            "no fix without --fix: {out}"
        );
    }

    #[test]
    fn upsert_replaces_existing_run_date() {
        let text = "stats:\n  last_doctor_run: 2026-01-01\n";
        let out = upsert_doctor_stats(text, "2026-05-27", false).unwrap();
        assert!(out.contains("last_doctor_run: 2026-05-27"), "{out}");
        assert!(!out.contains("2026-01-01"), "old date gone: {out}");
        // exactly one run line — no duplicate insert
        assert_eq!(out.matches("last_doctor_run:").count(), 1, "{out}");
    }

    #[test]
    fn upsert_inserts_run_key_into_existing_block() {
        // stats block exists but only has an unrelated child.
        let text = "stats:\n  last_memory_review: 2026-02-02\n";
        let out = upsert_doctor_stats(text, "2026-05-27", false).unwrap();
        assert!(
            out.contains("last_memory_review: 2026-02-02"),
            "preserved sibling: {out}"
        );
        assert!(out.contains("last_doctor_run: 2026-05-27"), "{out}");
    }

    #[test]
    fn upsert_inserts_above_trailing_blank_before_sibling() {
        // A blank line separating the stats block from a following sibling
        // must stay the separator — the new key joins the existing children
        // above it, not stranded below the blank.
        let text = "stats:\n  last_memory_review: 2026-02-02\n\nschedule:\n- cron: 0 9 * * *\n";
        let out = upsert_doctor_stats(text, "2026-05-27", false).unwrap();
        assert!(
            out.contains(
                "  last_memory_review: 2026-02-02\n  last_doctor_run: 2026-05-27\n\nschedule:"
            ),
            "new key joins the block above the blank separator: {out:?}"
        );
    }

    #[test]
    fn upsert_also_stamps_fix_when_requested() {
        let out = upsert_doctor_stats("qmd_collection: ob\n", "2026-05-27", true).unwrap();
        assert!(out.contains("last_doctor_run: 2026-05-27"), "{out}");
        assert!(out.contains("last_doctor_fix: 2026-05-27"), "{out}");
    }

    #[test]
    fn upsert_idempotent_returns_none_when_current() {
        let text = "stats:\n  last_doctor_run: 2026-05-27\n";
        assert!(upsert_doctor_stats(text, "2026-05-27", false).is_none());
    }

    #[test]
    fn upsert_idempotent_only_for_requested_keys() {
        // run already current, but --fix needs the fix line ⇒ must still write.
        let text = "stats:\n  last_doctor_run: 2026-05-27\n";
        let out = upsert_doctor_stats(text, "2026-05-27", true).unwrap();
        assert!(out.contains("last_doctor_fix: 2026-05-27"), "{out}");
        assert_eq!(out.matches("last_doctor_run:").count(), 1, "{out}");
    }

    #[test]
    fn upsert_preserves_comments_and_other_blocks() {
        let text = "# top comment\nupdate_channel: stable\nstats:\n  last_doctor_run: 2026-01-01\nschedule:\n- cron: 0 9 * * *\n  skill: /daily\n";
        let out = upsert_doctor_stats(text, "2026-05-27", false).unwrap();
        assert!(out.contains("# top comment"), "comment kept: {out}");
        assert!(out.contains("schedule:"), "schedule kept: {out}");
        assert!(out.contains("skill: /daily"), "schedule body kept: {out}");
        assert!(out.contains("last_doctor_run: 2026-05-27"), "{out}");
    }

    #[test]
    fn upsert_matches_four_space_child_indent() {
        let text = "stats:\n    last_doctor_run: 2026-01-01\n";
        let out = upsert_doctor_stats(text, "2026-05-27", true).unwrap();
        assert!(
            out.contains("    last_doctor_run: 2026-05-27"),
            "4-space run: {out}"
        );
        assert!(
            out.contains("    last_doctor_fix: 2026-05-27"),
            "4-space fix: {out}"
        );
    }

    #[test]
    fn upsert_preserves_missing_trailing_newline() {
        let out = upsert_doctor_stats("qmd_collection: ob", "2026-05-27", false).unwrap();
        assert!(!out.ends_with('\n'), "no trailing newline added: {out:?}");
        let out_nl = upsert_doctor_stats("qmd_collection: ob\n", "2026-05-27", false).unwrap();
        assert!(out_nl.ends_with('\n'), "trailing newline kept: {out_nl:?}");
    }

    #[test]
    fn upsert_preserves_crlf_line_endings() {
        // Windows-authored config: every line break must stay CRLF, not be
        // silently normalised to bare LF on stamp.
        let text = "stats:\r\n  last_doctor_run: 2026-01-01\r\n";
        let out = upsert_doctor_stats(text, "2026-05-27", false).unwrap();
        assert!(out.contains("last_doctor_run: 2026-05-27"), "{out:?}");
        assert_eq!(
            out.matches('\n').count(),
            out.matches("\r\n").count(),
            "all line endings must remain CRLF: {out:?}"
        );
        assert!(out.ends_with("\r\n"), "trailing CRLF preserved: {out:?}");
    }

    #[test]
    fn upsert_inserts_crlf_when_creating_block_in_crlf_file() {
        // A newly-appended stats block must also use the file's CRLF ending.
        let out = upsert_doctor_stats("qmd_collection: ob\r\n", "2026-05-27", true).unwrap();
        assert!(out.contains("stats:\r\n"), "{out:?}");
        assert_eq!(
            out.matches('\n').count(),
            out.matches("\r\n").count(),
            "appended lines must use CRLF: {out:?}"
        );
    }

    #[test]
    fn upsert_leaves_inline_stats_untouched() {
        // An inline mapping can't take block children — refuse rather than corrupt.
        let text = "stats: { last_doctor_run: 2026-01-01 }\n";
        assert!(upsert_doctor_stats(text, "2026-05-27", false).is_none());
    }

    #[test]
    fn stamp_doctor_run_writes_today_into_onebrain_yml() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "qmd_collection: ob\n").unwrap();
        stamp_doctor_run(d.path(), false, true);
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            after.contains(&format!("last_doctor_run: {today}")),
            "{after}"
        );
        assert!(after.contains("qmd_collection: ob"), "preserved: {after}");
        assert!(
            !after.contains("last_doctor_fix"),
            "no fix when fix=false: {after}"
        );
    }

    #[test]
    fn stamp_doctor_run_stamps_fix_and_legacy_vault_yml() {
        let d = tempdir().unwrap();
        // Legacy filename only — find_config_file resolves vault.yml.
        fs::write(d.path().join("vault.yml"), "qmd_collection: ob\n").unwrap();
        stamp_doctor_run(d.path(), true, true);
        let after = fs::read_to_string(d.path().join("vault.yml")).unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            after.contains(&format!("last_doctor_run: {today}")),
            "{after}"
        );
        assert!(
            after.contains(&format!("last_doctor_fix: {today}")),
            "{after}"
        );
    }

    #[test]
    fn stamp_doctor_run_noop_without_config_file() {
        let d = tempdir().unwrap();
        // No config file at all — must not panic and must not create one.
        stamp_doctor_run(d.path(), false, true);
        assert!(!d.path().join("onebrain.yml").exists());
        assert!(!d.path().join("vault.yml").exists());
    }
}
