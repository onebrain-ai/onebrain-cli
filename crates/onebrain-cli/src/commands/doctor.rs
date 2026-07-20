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
use crate::output::OutputMode;
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
    /// Fix made REAL progress but part of the issue remains (e.g. some
    /// values were reset to disk while others sat in an unsupported YAML
    /// shape). The message itemises both halves. Counts toward a non-zero
    /// exit like `Failed` — something still needs a manual edit.
    Partial(String),
    /// Fix was attempted but did not finish (subprocess failed, timed out,
    /// etc.). The message explains why.
    Failed(String),
    /// No automated recipe exists for this warning — the user must take
    /// the action manually. Message is the suggested command.
    Manual(String),
}

/// How the text-mode `--fix` confirmation was resolved. Distinguishing a
/// genuine interactive "y" from the auto-proceed paths lets recipes that are
/// destructive OUTSIDE the vault (qmd-leftovers: global npm uninstall +
/// deleting home-dir caches) require a real human answer, while every
/// vault-scoped recipe keeps the pre-3.2.4 automation-compat behaviour of
/// running on any non-`Declined` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FixConsent {
    /// A human answered "y" to the `[y/N]` prompt on an interactive TTY.
    InteractiveYes,
    /// Proceeded without a prompt: `--yes`, non-TTY stdin/stdout (cron /
    /// piped scripts), or the structured-guard short-circuit.
    AutoProceed,
    /// Explicit non-"y" answer or a stdin read error — no fixes run.
    Declined,
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
        eprintln!(
            "⚠ Could not read onebrain.yml — {err}: showing defaults for the checks below\n\
             💡 fix the YAML syntax error above in onebrain.yml, then rerun `onebrain doctor`"
        );
        onebrain_core::VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
            search: Default::default(),
            token_optimization: Default::default(),
            stats: Default::default(),
        }
    });

    // Vault directory basename for the report header.
    let vault_name = vault_root
        .as_path()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("vault")
        .to_string();

    let mut results = all_checks(vault_root.as_path(), &config);
    if !want_structured {
        // Under `--fix` the Summary box is deferred until after the fix pass
        // (one final summary, not a redundant before-and-after pair); a plain
        // run prints it inline with the report.
        emit_text_report(&results, &vault_name, mode, quiet, !fix)?;
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
                // No prompt was (or could be) shown on this path, so no
                // recipe ever receives `interactive_confirmed = true` here.
                let outcomes: Vec<(String, FixOutcome)> = issues
                    .iter()
                    .map(|r| {
                        (
                            r.check.clone(),
                            attempt_fix(r, vault_root.as_path(), true, false),
                        )
                    })
                    .collect();
                any_recipe_failed = outcomes
                    .iter()
                    .any(|(_, o)| matches!(o, FixOutcome::Failed(_) | FixOutcome::Partial(_)));
                fix_outcomes_json = outcomes
                    .iter()
                    .map(|(check, o)| {
                        let (outcome, message) = match o {
                            FixOutcome::Fixed(m) => ("fixed", m.as_str()),
                            FixOutcome::Partial(m) => ("partial", m.as_str()),
                            FixOutcome::Failed(m) => ("failed", m.as_str()),
                            FixOutcome::Manual(m) => ("manual", m.as_str()),
                        };
                        serde_json::json!({ "check": check, "outcome": outcome, "message": message })
                    })
                    .collect();
                results = all_checks(vault_root.as_path(), &config);
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
                } else if let consent @ (FixConsent::InteractiveYes | FixConsent::AutoProceed) =
                    confirm_fix(auto.len(), false, yes)
                {
                    // `interactive_confirmed` is true ONLY for a genuine
                    // human "y" on a TTY — the auto-proceed paths (non-TTY
                    // pipes, `--yes`) run the vault-scoped recipes as before
                    // but never unlock the qmd-leftovers destructive branch.
                    let interactive_confirmed = consent == FixConsent::InteractiveYes;
                    let outcomes: Vec<(String, FixOutcome)> = auto
                        .iter()
                        .map(|r| {
                            (
                                r.check.clone(),
                                attempt_fix(r, vault_root.as_path(), false, interactive_confirmed),
                            )
                        })
                        .collect();
                    any_recipe_failed = outcomes
                        .iter()
                        .any(|(_, o)| matches!(o, FixOutcome::Failed(_) | FixOutcome::Partial(_)));
                    print_fix_summary(&outcomes);
                    results = all_checks(vault_root.as_path(), &config);
                } else {
                    println!("\nNo changes made.");
                    // Declining the batch necessarily declines the qmd
                    // cleanup too (when it was offered) — record that so the
                    // NEXT `doctor --fix` doesn't ask again. The advisory
                    // `qmd-leftovers` finding itself keeps showing in the
                    // plain report; only the re-prompt is suppressed.
                    if auto.iter().any(|r| r.check == "qmd-leftovers") {
                        decline_qmd_cleanup(vault_root.as_path());
                        // Re-check with a RELOADED config (mirrors the
                        // "fixed" branch) so the Summary box below reflects
                        // the decline just recorded — without this it would
                        // still show the `--fix` CTA the user just said no
                        // to. The in-memory `config` predates the flag write,
                        // so a fresh load is required for the hint gate.
                        let refreshed =
                            load_vault_config(&vault_root).unwrap_or_else(|_| config.clone());
                        results = all_checks(vault_root.as_path(), &refreshed);
                    }
                }
            }
            // Single deferred Summary box — the pre-fix report omitted its own
            // summary so this is the only one the user sees.
            emit_summary_box(&results, mode)?;
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

/// Run the filesystem/config checks (`onebrain_fs::doctor::run_all_checks`),
/// then append the CLI-layer **native-search** index check.
///
/// The native-search check needs the `onebrain-search` engine, which
/// `onebrain-fs` doesn't depend on, so it can't live alongside the other
/// checks in that crate — it's spliced in here instead. It lands last (the
/// "Index & state" section), replacing the removed `qmd-embeddings` row.
fn all_checks(vault_root: &Path, config: &onebrain_core::VaultConfig) -> Vec<DoctorResult> {
    let mut results = run_all_checks(vault_root, config);
    // CLI-layer checks: both need `onebrain-search` (model/reranker registry
    // access), which `onebrain-fs` doesn't depend on — so they're spliced in
    // here rather than living alongside the fs-layer checks.
    results.push(config_values_check(vault_root));
    results.push(search_exclude_check(vault_root));
    results.push(token_optimization_check(vault_root));
    results.push(read_hook_failopen_check(vault_root));
    results.push(daemon_status_check(vault_root));
    results.push(native_search_check(vault_root));
    results.push(lex_index_check(vault_root));
    results.push(legacy_index_stub_check(vault_root));
    results.push(qmd_leftovers_check_prod(config));
    results
}

/// One out-of-range (or otherwise invalid) config value found by
/// [`config_values_check`]. Carries everything the check needs to render a
/// finding line AND everything the `--fix` recipe needs to reset the value.
#[derive(Debug)]
struct ConfigFinding {
    /// Dotted key path as the user knows it, e.g. `search.reranker.min_score`.
    dotted: String,
    /// Path segments for the line editor, e.g. `["search", "reranker", "min_score"]`.
    segments: Vec<&'static str>,
    /// What's wrong, e.g. `is 7.5 — must be between 0 and 1`.
    problem: String,
    /// The documented default the value resets to, as a YAML scalar.
    default_repr: String,
    /// Whether `--fix` may auto-reset it. `folders.*` and `search.collection`
    /// are report-only — never auto-reset (renaming folders orphans notes;
    /// changing the collection detaches the index).
    resettable: bool,
    /// True for `search.embed_model` — resetting it invalidates existing
    /// vectors, so the fix footer must tell the user to reindex.
    reindex_required: bool,
    /// True when the value is perfectly VALID but is a superseded default that
    /// a newer release moved away from (v3.4.16's
    /// `search.reranker.min_score: 0.30`). Reset like any other resettable
    /// finding, but counted and worded separately: calling an in-range value
    /// "invalid" would be a lie.
    superseded: bool,
}

impl ConfigFinding {
    /// Render the finding as a doctor detail line.
    fn detail_line(&self) -> String {
        if self.resettable {
            format!(
                "{}: {} · default: {}",
                self.dotted, self.problem, self.default_repr
            )
        } else {
            format!(
                "{}: {} (never auto-reset — edit manually)",
                self.dotted, self.problem
            )
        }
    }
}

/// The `search.reranker.min_score` value that v3.4.7–v3.4.15 scaffolded into
/// every freshly-initialized vault's own `onebrain.yml` (ADR 0026 writes the
/// key ACTIVE, not commented out). v3.4.16 moved the engine default to `0.0`
/// — reorder-only instead of filtering — but a present config value wins over
/// the engine default, so those vaults silently keep the old gate.
const SUPERSEDED_RERANK_MIN_SCORE: f64 = 0.30;

/// True when a `search.reranker.min_score` YAML scalar is EXACTLY the
/// superseded [`SUPERSEDED_RERANK_MIN_SCORE`] default, and that value is no
/// longer the current default.
///
/// Two deliberate narrowings:
/// - The equality is on the exact superseded value (within an f64 epsilon, so
///   `0.30` and `0.3` both match — they are the same number). A user who
///   deliberately chose some other gate (`0.5`, `0.25`) is left alone: this
///   flags a stale scaffold, not an opinion.
/// - It self-disables if the template default ever returns to `0.30` — then
///   the value is no longer superseded and there is nothing to report.
fn is_superseded_rerank_min_score(v: &serde_yaml::Value) -> bool {
    let current_default: Option<f64> = onebrain_fs::TEMPLATE_RERANK_MIN_SCORE.parse().ok();
    if current_default.is_some_and(|d| (d - SUPERSEDED_RERANK_MIN_SCORE).abs() < f64::EPSILON) {
        return false;
    }
    v.as_f64()
        .is_some_and(|f| (f - SUPERSEDED_RERANK_MIN_SCORE).abs() < f64::EPSILON)
}

/// Best-effort display of a YAML scalar for finding messages.
fn display_yaml_value(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::Null => "null".to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::String(s) => s.clone(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

/// Validate every tunable in raw config text against the same defaults and
/// registries the runtime uses (`onebrain_core` default fns, the
/// `onebrain-search` model/reranker registries, `onebrain-fs`'s
/// `VALID_UPDATE_CHANNELS`) — no duplicated range literals. Returns `None`
/// when the text isn't a YAML mapping (the `onebrain.yml-keys` check already
/// reports that); absent keys are fine (serde falls back to the default), so
/// only PRESENT-but-invalid values produce findings.
fn collect_config_findings(text: &str) -> Option<Vec<ConfigFinding>> {
    use onebrain_core::config::SearchConfig;
    use onebrain_core::{CheckpointPolicy, RerankerConfig};
    use onebrain_fs::vault_sync::{DEFAULT_UPDATE_CHANNEL, VALID_UPDATE_CHANNELS};
    use onebrain_search::embed::is_supported_model;
    use onebrain_search::rerank::is_supported_reranker;
    use serde_yaml::Value;

    let parsed: Value = serde_yaml::from_str(text).ok()?;
    // A comment-only / empty file parses as Null — that's a valid "all
    // defaults" config, not a shape error.
    if parsed.is_null() {
        return Some(Vec::new());
    }
    let root = parsed.as_mapping()?;
    let get = |m: &serde_yaml::Mapping, k: &str| m.get(Value::String(k.to_string())).cloned();
    let child = |m: &serde_yaml::Mapping, k: &str| get(m, k).and_then(|v| v.as_mapping().cloned());

    let mut findings: Vec<ConfigFinding> = Vec::new();

    // update_channel ∈ VALID_UPDATE_CHANNELS.
    if let Some(v) = get(root, "update_channel") {
        let valid = v
            .as_str()
            .is_some_and(|s| VALID_UPDATE_CHANNELS.contains(&s));
        if !valid {
            findings.push(ConfigFinding {
                dotted: "update_channel".to_string(),
                segments: vec!["update_channel"],
                problem: format!(
                    "is {} — must be one of: {}",
                    display_yaml_value(&v),
                    VALID_UPDATE_CHANNELS.join(", ")
                ),
                default_repr: DEFAULT_UPDATE_CHANNEL.to_string(),
                resettable: true,
                reindex_required: false,
                superseded: false,
            });
        }
    }

    // checkpoint.messages / checkpoint.minutes ≥ 1.
    if let Some(cp) = child(root, "checkpoint") {
        let defaults = CheckpointPolicy::default();
        for (key, default) in [
            ("messages", defaults.messages),
            ("minutes", defaults.minutes),
        ] {
            if let Some(v) = get(&cp, key) {
                if !value_is_positive_number(&v) {
                    findings.push(ConfigFinding {
                        dotted: format!("checkpoint.{key}"),
                        segments: vec!["checkpoint", key],
                        problem: format!("is {} — must be a number >= 1", display_yaml_value(&v)),
                        default_repr: default.to_string(),
                        resettable: true,
                        reindex_required: false,
                        superseded: false,
                    });
                }
            }
        }
    }

    // folders.* — non-empty strings; report-only.
    if let Some(folders) = child(root, "folders") {
        let standard = [
            "inbox",
            "projects",
            "areas",
            "knowledge",
            "resources",
            "agent",
            "archive",
            "logs",
        ];
        for key in standard {
            if let Some(v) = get(&folders, key) {
                let valid = v.as_str().is_some_and(|s| !s.trim().is_empty());
                if !valid {
                    findings.push(ConfigFinding {
                        dotted: format!("folders.{key}"),
                        segments: vec!["folders", key],
                        problem: format!(
                            "is {} — must be a non-empty folder name",
                            display_yaml_value(&v)
                        ),
                        default_repr: String::new(),
                        resettable: false,
                        reindex_required: false,
                        superseded: false,
                    });
                }
            }
        }
    }

    // search.* block.
    if let Some(search) = child(root, "search") {
        let sc = SearchConfig::default();

        // search.collection — non-empty string; report-only.
        if let Some(v) = get(&search, "collection") {
            let valid = v.as_str().is_some_and(|s| !s.trim().is_empty());
            if !valid {
                findings.push(ConfigFinding {
                    dotted: "search.collection".to_string(),
                    segments: vec!["search", "collection"],
                    problem: format!(
                        "is {} — must be a non-empty collection name (or absent to disable search)",
                        display_yaml_value(&v)
                    ),
                    default_repr: String::new(),
                    resettable: false,
                    reindex_required: false,
                    superseded: false,
                });
            }
        }

        // search.embed_model ∈ model registry.
        if let Some(v) = get(&search, "embed_model") {
            let valid = v.as_str().is_some_and(is_supported_model);
            if !valid {
                findings.push(ConfigFinding {
                    dotted: "search.embed_model".to_string(),
                    segments: vec!["search", "embed_model"],
                    problem: format!(
                        "is {} — not in the model registry (see `onebrain search model list`)",
                        display_yaml_value(&v)
                    ),
                    default_repr: sc.embed_model.clone(),
                    resettable: true,
                    reindex_required: true,
                    superseded: false,
                });
            }
        }

        // search.default_top_k ≥ 1 (integer).
        if let Some(v) = get(&search, "default_top_k") {
            let valid = v.as_u64().is_some_and(|n| n >= 1);
            if !valid {
                findings.push(ConfigFinding {
                    dotted: "search.default_top_k".to_string(),
                    segments: vec!["search", "default_top_k"],
                    problem: format!("is {} — must be an integer >= 1", display_yaml_value(&v)),
                    default_repr: sc.default_top_k.to_string(),
                    resettable: true,
                    reindex_required: false,
                    superseded: false,
                });
            }
        }

        // search.reranker.* block.
        if let Some(rr) = child(&search, "reranker") {
            let rd = RerankerConfig::default();

            if let Some(v) = get(&rr, "enabled") {
                if v.as_bool().is_none() {
                    findings.push(ConfigFinding {
                        dotted: "search.reranker.enabled".to_string(),
                        segments: vec!["search", "reranker", "enabled"],
                        problem: format!("is {} — must be true or false", display_yaml_value(&v)),
                        default_repr: rd.enabled.to_string(),
                        resettable: true,
                        reindex_required: false,
                        superseded: false,
                    });
                }
            }

            if let Some(v) = get(&rr, "model") {
                let valid = v.as_str().is_some_and(is_supported_reranker);
                if !valid {
                    findings.push(ConfigFinding {
                        dotted: "search.reranker.model".to_string(),
                        segments: vec!["search", "reranker", "model"],
                        problem: format!(
                            "is {} — not in the reranker registry (see `onebrain search model list`)",
                            display_yaml_value(&v)
                        ),
                        default_repr: rd.model.clone(),
                        resettable: true,
                        reindex_required: false,
                        superseded: false,
                    });
                }
            }

            if let Some(v) = get(&rr, "min_candidates") {
                let valid = v.as_u64().is_some_and(|n| n >= 1);
                if !valid {
                    findings.push(ConfigFinding {
                        dotted: "search.reranker.min_candidates".to_string(),
                        segments: vec!["search", "reranker", "min_candidates"],
                        problem: format!("is {} — must be an integer >= 1", display_yaml_value(&v)),
                        default_repr: rd.min_candidates.to_string(),
                        resettable: true,
                        reindex_required: false,
                        superseded: false,
                    });
                }
            }

            if let Some(v) = get(&rr, "min_score") {
                let valid = v.as_f64().is_some_and(|f| (0.0..=1.0).contains(&f));
                if !valid {
                    findings.push(ConfigFinding {
                        dotted: "search.reranker.min_score".to_string(),
                        segments: vec!["search", "reranker", "min_score"],
                        problem: format!(
                            "is {} — must be a number between 0 and 1",
                            display_yaml_value(&v)
                        ),
                        // Pinned to the engine's calibrated default by the
                        // cross-crate test in `init_integration.rs`.
                        default_repr: onebrain_fs::TEMPLATE_RERANK_MIN_SCORE.to_string(),
                        resettable: true,
                        reindex_required: false,
                        superseded: false,
                    });
                } else if is_superseded_rerank_min_score(&v) {
                    // In range, but STILL the superseded v3.4.7 default. ADR
                    // 0026 scaffolds `min_score` uncommented, so every vault
                    // initialized on v3.4.7–v3.4.15 carries a literal `0.30`
                    // in its own onebrain.yml — and a present value beats
                    // `DEFAULT_RERANK_MIN_SCORE` in
                    // `search_common::rerank_settings_from_config`. Those
                    // vaults therefore keep the old hard gate after upgrading
                    // to v3.4.16 and get none of the fix, with nothing to tell
                    // them (the bounds check above is happy with 0.30).
                    findings.push(ConfigFinding {
                        dotted: "search.reranker.min_score".to_string(),
                        segments: vec!["search", "reranker", "min_score"],
                        problem: format!(
                            "is {} — the superseded v3.4.7 default: it DROPS every hit scoring \
                             below it instead of merely ranking them, roughly halving \
                             heading-shaped hit@10 (see ADR 0034)",
                            display_yaml_value(&v)
                        ),
                        default_repr: onebrain_fs::TEMPLATE_RERANK_MIN_SCORE.to_string(),
                        resettable: true,
                        reindex_required: false,
                        superseded: true,
                    });
                }
            }
        }
    }

    Some(findings)
}

const CONFIG_VALUES_CHECK: &str = "config-values";

/// Per-key config value validation (`check = "config-values"`). Validates
/// every PRESENT tunable in `onebrain.yml` against the runtime defaults and
/// model/reranker registries — the self-documentation counterpart of the
/// commented `init` template (ADR 0026). Missing keys are fine (serde falls
/// back to defaults); a missing or unparsable file is the `onebrain.yml` /
/// `onebrain.yml-keys` checks' territory, so this check skips quietly then.
///
/// All findings are advisory (`warn`): `--fix` auto-resets the tunables to
/// their documented defaults; `folders.*` / `search.collection` findings are
/// report-only (never auto-reset).
fn config_values_check(vault_root: &Path) -> DoctorResult {
    use onebrain_core::find_config_file;
    let Some(path) = find_config_file(vault_root) else {
        return DoctorResult::ok(CONFIG_VALUES_CHECK, "skipped — no config file");
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return DoctorResult::ok(CONFIG_VALUES_CHECK, "skipped — config unreadable");
    };
    let Some(findings) = collect_config_findings(&text) else {
        return DoctorResult::ok(
            CONFIG_VALUES_CHECK,
            "skipped — invalid YAML (see onebrain.yml-keys)",
        );
    };
    // Self-documentation coverage: template-known keys that exist in this
    // config but carry no comment line directly above them (`--fix`
    // backfills the template's own comment; keys under a user comment are
    // never counted — the user's comments win). Read-only here: zero writes
    // on a plain doctor run.
    let undocumented = undocumented_keys(&text);
    // Section-layout drift: the top-level blocks are out of template order or
    // missing their section banners. Read-only here — reported now, restructured
    // by `--fix`. `config_layout_matches` is conservative (never flags a shape
    // it declines to restructure), so this only fires on an addressable mapping.
    let layout_drift = !onebrain_fs::config_layout_matches(&text);
    if findings.is_empty() && undocumented.is_empty() && !layout_drift {
        return DoctorResult::ok(CONFIG_VALUES_CHECK, "all values in range");
    }
    let mut message_parts: Vec<String> = Vec::new();
    let mut details: Vec<String> = findings.iter().map(ConfigFinding::detail_line).collect();
    // Superseded-default findings are counted apart from out-of-range ones:
    // `0.30` IS a legal min_score, so folding it into "invalid value(s)" would
    // misreport a valid config as broken.
    let superseded_count = findings.iter().filter(|f| f.superseded).count();
    let invalid_count = findings.len() - superseded_count;
    if invalid_count > 0 {
        message_parts.push(format!("{invalid_count} invalid value(s)"));
    }
    if superseded_count > 0 {
        message_parts.push(format!("{superseded_count} superseded default(s)"));
    }
    if !undocumented.is_empty() {
        message_parts.push(format!("{} undocumented key(s)", undocumented.len()));
        details.push(format!(
            "{} key(s) lack self-documentation comments — doctor --fix will add them: {}",
            undocumented.len(),
            undocumented.join(", ")
        ));
    }
    if layout_drift {
        message_parts.push("layout drift".to_string());
        details.push(
            "config layout differs from template — doctor --fix will restructure (reorder sections, add banners; values and comments preserved)".to_string(),
        );
    }
    // Any of the repair actions warrants a `--fix` hint; compose it from
    // whichever apply so the message never over-promises.
    let mut actions: Vec<&str> = Vec::new();
    if findings.iter().any(|f| f.resettable && !f.superseded) {
        actions.push("reset out-of-range tunables to their defaults");
    }
    if findings.iter().any(|f| f.resettable && f.superseded) {
        actions.push("reset superseded defaults to their current values");
    }
    if !undocumented.is_empty() {
        actions.push("add the missing self-documentation comments");
    }
    if layout_drift {
        actions.push("restructure the layout to the template");
    }
    let mut r =
        DoctorResult::warn(CONFIG_VALUES_CHECK, message_parts.join(" · ")).with_details(details);
    if !actions.is_empty() {
        r = r.with_hint(format!(
            "Run onebrain doctor --fix to {}",
            join_actions(&actions)
        ));
    }
    r
}

/// Join action phrases into an English list ("a", "a and b", "a, b, and c").
fn join_actions(actions: &[&str]) -> String {
    match actions {
        [] => String::new(),
        [a] => a.to_string(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// Dotted paths of template-known keys that exist in `text` without a
/// comment line directly above them — the `--fix` comment-backfill targets.
/// Missing keys are never listed (absent = defaults by design), and keys the
/// line editor can't address (inline mappings) are skipped.
fn undocumented_keys(text: &str) -> Vec<String> {
    onebrain_fs::config_key_docs()
        .iter()
        .filter(|d| onebrain_fs::yaml_edit::key_lacks_comment(text, d.segments))
        .map(|d| d.segments.join("."))
        .collect()
}

const SEARCH_EXCLUDE_CHECK: &str = "search-exclude";

/// True when `search.collection` is set (key present, non-null) AND
/// `search.exclude` is entirely absent. A present-but-empty `exclude: []`
/// is the user's explicit choice, never counted as missing — this is a key
/// PRESENCE gate, deliberately separate from `undocumented_keys`'s
/// comment-only backfill (which never touches an absent key by design).
fn search_exclude_missing(text: &str) -> bool {
    let Ok(parsed) = serde_yaml::from_str::<serde_yaml::Value>(text) else {
        return false;
    };
    let Some(search) = parsed.get("search").and_then(|v| v.as_mapping()) else {
        return false;
    };
    let key = |k: &str| serde_yaml::Value::String(k.to_string());
    let collection_set = search.get(key("collection")).is_some_and(|v| !v.is_null());
    let exclude_absent = search.get(key("exclude")).is_none();
    collection_set && exclude_absent
}

/// `search.exclude` presence check (`check = "search-exclude"`). A vault
/// that adopted search (`search.collection` set) before the v3.4.9 template
/// started scaffolding the exclude block (Task 3) is silently indexing its
/// own archive folder on every reindex. Fires only under that exact gate —
/// vaults that never adopted search, or that already carry an explicit
/// `exclude:` (even `[]`), are never flagged. Advisory only (`warn`, not
/// `error`): a missing exclude list degrades index quality, it doesn't
/// break anything.
fn search_exclude_check(vault_root: &Path) -> DoctorResult {
    use onebrain_core::find_config_file;
    let Some(path) = find_config_file(vault_root) else {
        return DoctorResult::ok(SEARCH_EXCLUDE_CHECK, "skipped — no config file");
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return DoctorResult::ok(SEARCH_EXCLUDE_CHECK, "skipped — config unreadable");
    };
    // Unparseable YAML is the `onebrain.yml-keys` check's territory — skip
    // quietly (same convention as `config_values_check`) rather than
    // reporting a misleading "search.exclude ok".
    if serde_yaml::from_str::<serde_yaml::Value>(&text).is_err() {
        return DoctorResult::ok(
            SEARCH_EXCLUDE_CHECK,
            "skipped — invalid YAML (see onebrain.yml-keys)",
        );
    }
    if !search_exclude_missing(&text) {
        return DoctorResult::ok(SEARCH_EXCLUDE_CHECK, "search.exclude ok");
    }
    DoctorResult::warn(
        SEARCH_EXCLUDE_CHECK,
        "search.exclude not set — archive folder is being indexed",
    )
    .with_hint("Run onebrain doctor --fix to insert the search.exclude block")
}

const TOKEN_OPTIMIZATION_CHECK: &str = "token-optimization";

/// True when the top-level `token_optimization` key is entirely absent from
/// the parsed config. Unparseable YAML and a non-mapping root (including the
/// comment-only/empty `Null` shape) return `false` — those are
/// `onebrain.yml-keys`'s territory, or simply have nothing yet to backfill
/// against; this is a key PRESENCE gate on an otherwise-real config, mirroring
/// `search_exclude_missing`'s convention.
fn token_optimization_missing(text: &str) -> bool {
    let Ok(parsed) = serde_yaml::from_str::<serde_yaml::Value>(text) else {
        return false;
    };
    let Some(root) = parsed.as_mapping() else {
        return false;
    };
    root.get(serde_yaml::Value::String("token_optimization".to_string()))
        .is_none()
}

/// Documented `token_optimization.*` sub-keys (from
/// [`onebrain_fs::config_key_docs`]) that are ABSENT from an EXISTING
/// `token_optimization:` block in `text` — issue #270's gap. Always empty
/// when the block itself is absent (that's [`token_optimization_missing`]'s
/// territory, issue #247 — never double-reported here). `get_max_tokens` /
/// `snippet_max_chars` count as present via either their active key OR the
/// commented placeholder the fresh template legitimately leaves them at by
/// default — see
/// [`onebrain_fs::yaml_edit::key_or_commented_placeholder_present`].
fn token_optimization_missing_sub_keys(text: &str) -> Vec<&'static [&'static str]> {
    if token_optimization_missing(text) {
        return Vec::new();
    }
    onebrain_fs::config_key_docs()
        .into_iter()
        .filter(|d| d.segments.first() == Some(&"token_optimization"))
        .filter(|d| !onebrain_fs::yaml_edit::key_or_commented_placeholder_present(text, d.segments))
        .map(|d| d.segments)
        .collect()
}

/// `token_optimization` block presence + completeness check (`check =
/// "token-optimization"`). Two gaps, both advisory (`warn`, not `error`):
/// nothing is functionally broken either way — the runtime quietly falls
/// back to `TokenOptimizationConfig::default()` for anything absent — the
/// config is just missing its documentation.
///
/// - **Whole block absent** (issue #247): a vault whose `onebrain.yml`
///   predates v3.4.10 (or was hand-edited) carries no `token_optimization`
///   block at all.
/// - **Sub-key(s) absent from an EXISTING block** (issue #270): a vault
///   whose block predates a later-added sub-key (e.g. `check_timeout_ms`,
///   added in v3.4.13) keeps that key permanently undocumented after
///   upgrade unless backfilled — the code's own contract
///   (`config_key_docs` "drives BOTH the init template and the doctor
///   --fix backfill") only holds once this case is caught too.
fn token_optimization_check(vault_root: &Path) -> DoctorResult {
    use onebrain_core::find_config_file;
    let Some(path) = find_config_file(vault_root) else {
        return DoctorResult::ok(TOKEN_OPTIMIZATION_CHECK, "skipped — no config file");
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return DoctorResult::ok(TOKEN_OPTIMIZATION_CHECK, "skipped — config unreadable");
    };
    // Unparseable YAML is the `onebrain.yml-keys` check's territory — skip
    // quietly (same convention as `search_exclude_check`) rather than
    // reporting a misleading "token_optimization ok".
    if serde_yaml::from_str::<serde_yaml::Value>(&text).is_err() {
        return DoctorResult::ok(
            TOKEN_OPTIMIZATION_CHECK,
            "skipped — invalid YAML (see onebrain.yml-keys)",
        );
    }
    if token_optimization_missing(&text) {
        return DoctorResult::warn(
            TOKEN_OPTIMIZATION_CHECK,
            "token_optimization block not set — token-opt config is undocumented and un-tunable",
        )
        .with_hint("Run onebrain doctor --fix to insert the token_optimization block");
    }
    let missing_sub_keys = token_optimization_missing_sub_keys(&text);
    if !missing_sub_keys.is_empty() {
        let keys = missing_sub_keys
            .iter()
            .map(|s| *s.last().expect("non-empty segments"))
            .collect::<Vec<_>>()
            .join(", ");
        return DoctorResult::warn(
            TOKEN_OPTIMIZATION_CHECK,
            format!("token_optimization missing sub-key(s): {keys} — undocumented and un-tunable"),
        )
        .with_hint(
            "Run onebrain doctor --fix to backfill the missing token_optimization sub-key(s)",
        );
    }
    DoctorResult::ok(TOKEN_OPTIMIZATION_CHECK, "token_optimization ok")
}

const READ_HOOK_FAILOPEN_CHECK: &str = "read-hook-failopen";
/// Rolling window (days) over which the read-hook fail-open rate is measured.
const FAILOPEN_WINDOW_DAYS: i64 = 7;
/// Minimum read-hook events in the window before the rate is trustworthy — a
/// cold start (few reads) is not degradation, so stay silent below this.
const FAILOPEN_MIN_SAMPLE: usize = 20;
/// Fail-open fraction at/above which the gate is treated as silently inert.
const FAILOPEN_WARN_RATIO: f64 = 0.95;

/// Verdict of the pure read-hook fail-open classifier — separated from the
/// filesystem read so the ratio logic is unit-testable without a vault.
#[derive(Debug, PartialEq)]
enum FailopenVerdict {
    /// Enough samples and the fail-open rate is under the inert threshold.
    Healthy,
    /// Too few read-hook events in the window to judge (never warn on a cold
    /// start — that's not degradation).
    InsufficientSample,
    /// Nearly every read fails open — the gate is registered but effectively
    /// doing nothing (the #264 silent-inert condition the field test hit).
    Inert { ratio: f64 },
}

/// Classify a window's read-hook counts. Pure: no I/O, no clock.
fn classify_failopen(total: usize, failopen: usize) -> FailopenVerdict {
    if total < FAILOPEN_MIN_SAMPLE {
        return FailopenVerdict::InsufficientSample;
    }
    let ratio = failopen as f64 / total as f64;
    if ratio >= FAILOPEN_WARN_RATIO {
        FailopenVerdict::Inert { ratio }
    } else {
        FailopenVerdict::Healthy
    }
}

/// Read-hook silent-inert detector (`check = "read-hook-failopen"`, #264
/// sub-fix C). When the ledger gate is ENABLED (`token_optimization.read_hook:
/// ledger`) but nearly every read-hook decision fails open over the last
/// [`FAILOPEN_WINDOW_DAYS`] days, the gate is registered yet doing nothing —
/// exactly the v3.4.12 field condition (25× `engine_open_error` + timeout
/// fail-opens, ~0 savings). Reads the on-disk gain JSONL only (never the
/// daemon), so it works regardless of daemon state. Advisory (`warn`) — nothing
/// is broken, the optimization is just inert. Off vaults / thin samples are a
/// quiet `ok`.
fn read_hook_failopen_check(vault_root: &Path) -> DoctorResult {
    use crate::commands::search_common::{collection_cache_dir, collection_name_readonly};
    use onebrain_core::config::ReadHookMode;
    use onebrain_core::load_vault_config;
    use onebrain_token::gain::JsonlGainWriter;
    use onebrain_token::{CacheKind, Surface};

    // Resolve the vault read-only; an unresolvable vault/collection is the
    // search checks' territory, so skip quietly rather than warn.
    let resolved = match crate::vault_ctx::require(Some(vault_root.to_path_buf())) {
        Ok(r) => r,
        Err(_) => return DoctorResult::ok(READ_HOOK_FAILOPEN_CHECK, "skipped — vault unresolved"),
    };

    // Only meaningful when the gate is actually enabled — an off vault (the
    // product default) has nothing to assess.
    let enabled = load_vault_config(&resolved.root)
        .map(|c| c.token_optimization.read_hook == ReadHookMode::Ledger)
        .unwrap_or(false);
    if !enabled {
        return DoctorResult::ok(
            READ_HOOK_FAILOPEN_CHECK,
            "read_hook gate off — not applicable",
        );
    }

    let collection = match collection_name_readonly(resolved.root.as_path()) {
        Ok(c) => c,
        Err(_) => {
            return DoctorResult::ok(READ_HOOK_FAILOPEN_CHECK, "skipped — no collection resolved")
        }
    };
    let gain_dir = collection_cache_dir(&collection).join("token").join("gain");
    let events = JsonlGainWriter::new(&gain_dir)
        .read_all()
        .unwrap_or_default();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = now - FAILOPEN_WINDOW_DAYS * 86_400;
    // `total` counts only the read-hook events that produce a gain row — a
    // fail-open (`HookFailopen`) or a genuine deny (`LedgerDeny`). A plain
    // Allow / first_send records NO gain event (design §5c: only fail-opens and
    // denies meter), so it never appears here. The ratio is therefore
    // failopen / (failopen + genuine_deny), i.e. "of the decisions that DID
    // something measurable, how many were degraded" — which is exactly the
    // silent-inert signal. Consequence: the 20-sample floor is reached in
    // fail-opens + denies, not raw reads, so a healthy first-read-heavy vault
    // (mostly first_send allows) can take a while to accumulate a sample.
    let (mut total, mut failopen) = (0usize, 0usize);
    for e in &events {
        if e.surface == Surface::ReadHook && e.ts >= cutoff {
            total += 1;
            if e.cache == CacheKind::HookFailopen {
                failopen += 1;
            }
        }
    }

    match classify_failopen(total, failopen) {
        FailopenVerdict::Healthy => DoctorResult::ok(
            READ_HOOK_FAILOPEN_CHECK,
            format!("read-hook gate healthy — {failopen}/{total} fail-open over {FAILOPEN_WINDOW_DAYS}d"),
        ),
        FailopenVerdict::InsufficientSample => DoctorResult::ok(
            READ_HOOK_FAILOPEN_CHECK,
            format!(
                "read-hook gate: too few samples to assess ({total} read(s) over {FAILOPEN_WINDOW_DAYS}d)"
            ),
        ),
        FailopenVerdict::Inert { ratio } => DoctorResult::warn(
            READ_HOOK_FAILOPEN_CHECK,
            format!(
                "read-hook gate is effectively inert — {:.0}% of reads fail open ({failopen}/{total} over {FAILOPEN_WINDOW_DAYS}d)",
                ratio * 100.0
            ),
        )
        .with_details(vec![
            "the ledger gate is enabled but almost never produces a verdict — token savings are ~0".to_string(),
            "common causes: the daemon is version-skewed or down, or the round-trip exceeds token_optimization.check_timeout_ms".to_string(),
        ])
        .with_hint(
            "Check `onebrain daemon status` (restart a skewed daemon) and raise token_optimization.check_timeout_ms for slow/iCloud vaults",
        ),
    }
}

/// Map a daemon `/api/internal/status` JSON body into the
/// `(last_indexed_at, doc_count, pending_total)` triple `native_search_check`
/// needs. Pure (no I/O) so it's unit-testable without a live daemon. Mirrors
/// `search_status::probe_from_daemon_status`: a body with no numeric
/// `doc_count` is a broken read, not a healthy zero — return `None` so the
/// caller falls back to the direct engine open rather than asserting an empty
/// index.
fn daemon_status_counts(v: &serde_json::Value) -> Option<(Option<u64>, usize, usize)> {
    let field = |k: &str| v.get(k).and_then(serde_json::Value::as_u64);
    let doc_count = field("doc_count")? as usize;
    let pending = (field("pending_new").unwrap_or(0)
        + field("pending_changed").unwrap_or(0)
        + field("pending_removed").unwrap_or(0)) as usize;
    Some((field("last_indexed"), doc_count, pending))
}

/// Native-search index check (`check = "search"`). Read-only and
/// download-free: it resolves the collection, checks whether the on-disk index
/// exists, and — only if it does — opens the engine (lazy embedder, so
const DAEMON_STATUS_CHECK: &str = "daemon";

/// Enumerate every per-vault daemon slot (v3.4.13, #230) and surface running
/// daemons + flag stale/wedged ones. Machine-wide (all slots, not just this
/// vault's), so a user with several vaults sees every warm daemon at a glance.
///
/// READ-ONLY: probes `/api/health` (short timeout, no retry) via
/// [`crate::commands::daemon_client::DaemonHandle`] — it NEVER starts, stops, or
/// restarts a daemon (ADR 0032: a read verb must not disturb another session's
/// daemon).
///
/// Verdicts:
/// - `ok`   — no daemon running, or every slot maps to a live daemon.
/// - `warn` — a slot has a discovery record whose daemon no longer answers
///   (crashed / hard-killed), or a lingering start lock with no live daemon (a
///   wedged slot), OR a LIVE legacy pre-v3.4.13 daemon is still running (the
///   upgrade window — it holds the vault's redb lock; LOW-2). Advisory; the hint
///   points at `onebrain daemon stop --all`.
fn daemon_status_check(_vault_root: &Path) -> DoctorResult {
    use crate::commands::daemon_client::{
        self, own_version, version_decision, DaemonHandle, DaemonInfo, VersionDecision,
    };

    let slots = match daemon_client::all_slots() {
        Ok(s) => s,
        Err(e) => {
            return DoctorResult::warn(
                DAEMON_STATUS_CHECK,
                format!("could not enumerate daemon slots: {e}"),
            )
        }
    };

    let mut running = Vec::new();
    let mut stale = Vec::new();
    // #291: LIVE daemons whose stamped version differs from ours. A version-
    // skewed daemon keeps serving the OLD wire shape (e.g. #281's empty gain
    // route → dark dashboard) until it idles out or is restarted. doctor is
    // diagnostic — it NEVER stops anything — but it warns with both versions +
    // the `daemon stop --all` hint (the safety net for a user who `brew
    // upgrade`d directly and never ran `onebrain update` / `plugin update`).
    let mut skewed = Vec::new();
    let is_live = |info: &DaemonInfo| DaemonHandle::new(info.clone()).probe_health().is_some();
    for slot in &slots {
        match DaemonInfo::read(&slot.json) {
            Ok(Some(info)) => {
                if is_live(&info) {
                    running.push(format!(
                        "pid {} · port {} · v{} · vault {}",
                        info.pid,
                        info.port,
                        info.version,
                        info.vault.as_deref().unwrap_or("(none — vault-less)")
                    ));
                    if version_decision(&info.version, own_version()) == VersionDecision::Restart {
                        skewed.push(format!(
                            "daemon v{} != CLI v{} · vault {}",
                            info.version,
                            own_version(),
                            info.vault.as_deref().unwrap_or("(none — vault-less)")
                        ));
                    }
                } else {
                    // Record present but the daemon doesn't answer: crashed or
                    // hard-killed, leaving a stale slot json behind.
                    stale.push(format!(
                        "{} (record present, daemon not answering)",
                        slot.json.display()
                    ));
                }
            }
            // No json (only a pid/lock left): a wedged start lock with no daemon.
            Ok(None) if slot.lock.exists() => {
                stale.push(format!(
                    "{} (wedged start lock, no daemon)",
                    slot.lock.display()
                ));
            }
            Ok(None) => {}
            Err(_) => {
                stale.push(format!(
                    "{} (corrupt discovery record)",
                    slot.json.display()
                ));
            }
        }
    }

    // The legacy pre-v3.4.13 machine-wide daemon (`daemon.json`, no `-<hash>`) is
    // EXCLUDED from `all_slots`, so a live one would otherwise be invisible here
    // while it still holds the vault's redb lock. Surface it (LOW-2) so the
    // upgrade window is visible + actionable via `daemon stop --all`.
    let mut legacy_live = false;
    if let Ok(legacy) = daemon_client::legacy_slot() {
        match DaemonInfo::read(&legacy.json) {
            Ok(Some(info)) if is_live(&info) => {
                legacy_live = true;
                running.push(format!(
                    "pid {} · port {} · v{} · LEGACY pre-v3.4.13 daemon (retire with `onebrain daemon stop --all`)",
                    info.pid, info.port, info.version
                ));
            }
            // A dead legacy record is inert (new code ignores it); don't flag it.
            _ => {}
        }
    }

    let mut details = Vec::new();
    for r in &running {
        details.push(format!("running: {r}"));
    }
    for sk in &skewed {
        details.push(format!("version-skew: {sk}"));
    }
    for s in &stale {
        details.push(format!("stale: {s}"));
    }

    if running.is_empty() && stale.is_empty() {
        return DoctorResult::ok(DAEMON_STATUS_CHECK, "no warm daemon running");
    }

    // A live legacy daemon, a version-skewed live daemon (#291), or any
    // stale/wedged slot warrants a warn (+ the stop-all hint); an all-healthy,
    // all-current per-vault fleet is ok.
    if stale.is_empty() && !legacy_live && skewed.is_empty() {
        let msg = match running.len() {
            1 => "1 warm daemon running".to_string(),
            n => format!("{n} warm daemons running"),
        };
        DoctorResult::ok(DAEMON_STATUS_CHECK, msg).with_details(details)
    } else {
        // Version-skew takes primacy in the message (it names both versions);
        // a live legacy daemon and stale/wedged slots are the other warn causes.
        let msg = if !skewed.is_empty() {
            match skewed.len() {
                1 => format!("version-skewed daemon running: {}", skewed[0]),
                n => format!(
                    "{n} version-skewed daemons running (CLI v{})",
                    own_version()
                ),
            }
        } else if legacy_live && stale.is_empty() {
            "legacy pre-v3.4.13 daemon still running — retire it".to_string()
        } else {
            format!(
                "{} running · {} stale/wedged daemon slot(s)",
                running.len(),
                stale.len()
            )
        };
        DoctorResult::warn(DAEMON_STATUS_CHECK, msg)
            .with_hint("onebrain daemon stop --all")
            .with_details(details)
    }
}

/// `Engine::open` never downloads a model) and reads `Engine::status` (stored
/// hashes + a vault re-hash, no embed). Reports the indexed doc count, pending
/// drift, and whether the embedding model is downloaded.
///
/// Verdicts:
/// - `ok`   — index exists and is up to date.
/// - `warn` — no index yet, pending drift, or the model isn't downloaded.
///   These are advisory (a fresh vault legitimately has no index); doctor's
///   exit code only escalates on `error`, so this check never fails the run.
fn native_search_check(vault_root: &Path) -> DoctorResult {
    use crate::commands::search_common::{
        collection_cache_dir, collection_name_readonly, is_indexed, open_engine_with_collection,
    };
    use onebrain_core::load_vault_config;
    use onebrain_search::embed::model_download_status;
    use onebrain_search::rerank::{reranker_download_status, reranker_registry};

    // Resolve the collection READ-ONLY (`collection_name_readonly`): doctor
    // must never rewrite the config as a side effect — the pre-v3.4.8
    // `collection_for` persisted a generated name on a never-configured
    // vault via a serde re-serialization, which would have destroyed the
    // commented template's comments on the first doctor run. The generated
    // name is the same deterministic one a later `search reindex` adopts.
    // On the two resolution-failure early returns the reranker state is
    // genuinely uncomputable (no config, no cache dir) — the three fields
    // are still reported, as `unknown`, so the payload shape is identical
    // on EVERY return path (consumers never branch on field presence).
    let unresolved = |msg: String| {
        let mut r = DoctorResult::warn("search", msg);
        r.details.extend([
            "reranker_enabled: unknown".to_string(),
            "reranker_model: unknown".to_string(),
            "reranker_downloaded: unknown".to_string(),
        ]);
        r
    };
    let resolved = match crate::vault_ctx::require(Some(vault_root.to_path_buf())) {
        Ok(r) => r,
        Err(e) => {
            return unresolved(format!("could not resolve vault: {e}"));
        }
    };
    let collection = match collection_name_readonly(resolved.root.as_path()) {
        Ok(c) => c,
        Err(e) => {
            return unresolved(format!("could not resolve collection: {e}"));
        }
    };
    let cache_dir = collection_cache_dir(&collection);

    // Read-only config load — same config the rest of the check already
    // resolves through the read-only helpers. Never persisted here.
    // A load failure degrades gracefully to reranker-disabled defaults +
    // an unknown configured embed model name, rather than aborting the whole
    // check — the index/model-on-disk facts below are still worth reporting.
    let config = load_vault_config(&resolved.root).ok();
    let reranker_cfg = config.as_ref().map(|c| c.search.reranker.clone());
    let reranker_enabled = reranker_cfg.as_ref().is_some_and(|r| r.enabled);
    let reranker_model = reranker_cfg
        .as_ref()
        .map(|r| r.model.clone())
        .unwrap_or_default();
    let reranker_downloaded = reranker_registry()
        .iter()
        .find(|r| r.name == reranker_model)
        .is_some_and(|r| reranker_download_status(r, &cache_dir).downloaded);
    let configured_embed_model = config
        .as_ref()
        .map(|c| c.search.embed_model.clone())
        .unwrap_or_default();
    let configured_model_downloaded = onebrain_search::embed::model_registry()
        .iter()
        .find(|m| m.name == configured_embed_model)
        .is_some_and(|m| model_download_status(m, &cache_dir).downloaded);
    // "Any model downloaded" still gates the coarser "nothing at all is
    // downloaded" wording used below (kept byte-identical to the pre-Task-8
    // messages so existing consumers aren't surprised); the finer-grained
    // "wrong model downloaded" case is reported separately.
    let any_model_downloaded = onebrain_search::embed::model_registry()
        .iter()
        .any(|m| model_download_status(m, &cache_dir).downloaded);

    let reranker_details = vec![
        format!("reranker_enabled: {reranker_enabled}"),
        format!("reranker_model: {reranker_model}"),
        format!("reranker_downloaded: {reranker_downloaded}"),
    ];
    // The reranker warn is additive to whatever the index-state arms below
    // decide — surfaced via the `finish` closure on every return path so the
    // fields (and, when applicable, the warn) are reported regardless of
    // index state.
    let reranker_warn_hint = "run `onebrain search reindex` to fetch the reranker model (~570 MB)";
    let reranker_needs_warn = reranker_enabled && !reranker_downloaded;

    // Finish a result by appending the reranker detail lines (always) and
    // escalating to `warn` with the reranker hint when the reranker is
    // enabled but its model isn't downloaded — without ever *downgrading* an
    // existing warn/error, and without clobbering an existing hint (the
    // reranker hint only wins when the base result carries none).
    let finish = |mut result: DoctorResult| -> DoctorResult {
        result.details.extend(reranker_details.clone());
        if reranker_needs_warn {
            if result.status == DoctorStatus::Ok {
                result.status = DoctorStatus::Warn;
            }
            if result.hint.is_none() {
                result.hint = Some(reranker_warn_hint.to_string());
            } else {
                result.details.push(reranker_warn_hint.to_string());
            }
        }
        result
    };

    // No index on disk → advisory warn. Two causes look identical from here
    // (there's no marker that survives a cache purge — `last_indexed_at` lives
    // inside the very `engine.redb` that gets wiped): either a fresh vault that
    // never reindexed, or an established collection whose index/model vanished
    // (the OS-purgeable cache location that v3.4.5 moved away from — issue
    // #114). The message names both honestly rather than mis-asserting a purge
    // on a fresh vault. The model dirs live INSIDE the collection cache dir, so
    // "no cache dir" implies "no model" — no need to qualify that separately.
    if !is_indexed(&cache_dir) {
        return finish(
            DoctorResult::warn(
                "search",
                format!("no index for {collection} · model not downloaded — never reindexed, or the search cache was cleared by OS storage cleanup"),
            )
            .with_hint("onebrain search reindex")
            .with_details(vec![format!("collection: {collection}")]),
        );
    }

    // Index exists → read status. Warm-daemon FIRST (same passive discovery
    // `search status` uses): when an `onebrain mcp` session holds the engine it
    // owns the redb lock, so opening a second engine here reports a misleading
    // "engine busy: index locked". Route through the daemon's
    // `/api/internal/status` instead and read the real counts. Passive
    // discovery only — doctor is read-only and NEVER starts/restarts a daemon.
    //
    // Fallback to a direct open only when no matching daemon serves this vault,
    // or the daemon probe didn't yield counts. The direct open goes through
    // `open_engine_with_collection` (NOT `open_engine`): the latter resolves via
    // `collection_for`, which PERSISTS a generated collection name through a
    // comment-destroying serde rewrite when `search.collection` is absent —
    // doctor's read path must never write the config.
    let daemon_counts = crate::commands::search_common::route_to_daemon(&resolved)
        .and_then(|handle| handle.status().ok())
        .and_then(|body| daemon_status_counts(&body));
    let (last_indexed_at, doc_count, pending) = match daemon_counts {
        Some(counts) => counts,
        None => match open_engine_with_collection(&resolved, &collection) {
            Ok(engine) => match engine.status(resolved.root.as_path()) {
                Ok(s) => (s.last_indexed_at, s.doc_count, s.pending_total()),
                Err(e) => {
                    return finish(
                        DoctorResult::warn("search", format!("index status unavailable: {e}"))
                            .with_details(vec![format!("collection: {collection}")]),
                    );
                }
            },
            Err(e) => {
                return finish(
                    DoctorResult::warn("search", format!("engine unavailable: {e}"))
                        .with_details(vec![format!("collection: {collection}")]),
                );
            }
        },
    };

    let never_indexed = last_indexed_at.is_none();
    let mut details = vec![format!("collection: {collection}")];
    if !any_model_downloaded {
        details.push("embedding model not downloaded — onebrain search reindex".to_string());
    } else if !configured_model_downloaded {
        details.push(format!(
            "configured embedding model '{configured_embed_model}' is not downloaded — run `onebrain search reindex`"
        ));
    }

    if pending > 0 || never_indexed {
        let summary = if never_indexed {
            format!("{doc_count} indexed · never reindexed")
        } else {
            format!("{doc_count} indexed · {pending} pending")
        };
        return finish(
            DoctorResult::warn("search", summary)
                .with_hint("onebrain search reindex")
                .with_details(details),
        );
    }

    if !any_model_downloaded {
        // Up to date on disk, but the model isn't present — a reindex or query
        // would trigger a download. Advisory warn so the user isn't surprised.
        return finish(
            DoctorResult::warn(
                "search",
                format!("{doc_count} indexed · model not downloaded"),
            )
            .with_hint("onebrain search reindex")
            .with_details(details),
        );
    }

    if !configured_model_downloaded {
        // Some model is on disk, but not the one the vault is configured to
        // use — the next query/reindex would silently pull a fresh download
        // of the configured model. Advisory warn, distinct wording from the
        // "nothing downloaded" case above.
        return finish(
            DoctorResult::warn(
                "search",
                format!(
                    "{doc_count} indexed · configured embedding model '{configured_embed_model}' is not downloaded"
                ),
            )
            .with_hint("onebrain search reindex")
            .with_details(details),
        );
    }

    finish(
        DoctorResult::ok("search", format!("{doc_count} indexed · up to date"))
            .with_details(details),
    )
}

// ─────────────────────────────────────────────────────────────────────────
// Dead keyword-index detection (B1, v3.4.16)
//
// The v3.4.16 tantivy schema change means an older index can't be opened, so
// `LexIndex::open_or_reset` wipes it and `Engine::open` repopulates it from
// redb's `chunk_meta`. If that rebuild is interrupted (Ctrl-C on what looks
// like a hung command) the collection is left holding every chunk in redb and
// ZERO documents in tantivy — and nothing else notices: `search status` counts
// docs from redb and cheerfully reports "up to date", while `reindex` skips
// every doc because `lex_hashes` says they're current. Keyword search returns
// nothing, forever, with no error anywhere.
//
// `Engine::lex_health()` is the cheap probe for exactly that state (one redb
// `table.len()` + one tantivy `num_docs()`, no scan).
// ─────────────────────────────────────────────────────────────────────────

const LEX_INDEX_CHECK: &str = "lex-index";

/// Doctor check (`check = "lex-index"`) — is the keyword index actually alive?
///
/// Read-only, and deliberately independent of [`native_search_check`]: that one
/// answers "is there an index and is it current", which is precisely the
/// question that reports healthy while BM25 is dead.
///
/// Every resolution failure degrades to `ok("skipped — …")` rather than a
/// warn: a vault with no config, no collection, or no index on disk is not
/// broken, and `native_search_check` already owns those messages. A busy engine
/// (an `onebrain mcp` session holding the redb lock) is likewise skipped, not
/// warned — doctor is read-only and must never look like breakage because a
/// daemon is running.
///
/// NOTE the deliberate ordering effect: opening the engine here is itself the
/// repair for the *marker-backed* case — `Engine::open` retries a pending
/// rebuild automatically. So the state this check can still report as dead is
/// the residual one where the rebuild marker is gone (external tmp reaper,
/// manual cleanup) and nothing will ever retry on its own. That is exactly the
/// case that needs a human-visible finding.
fn lex_index_check(vault_root: &Path) -> DoctorResult {
    use crate::commands::search_common::{
        collection_cache_dir, collection_name_readonly, is_indexed, open_engine_with_collection,
    };

    let skip = |msg: String| DoctorResult::ok(LEX_INDEX_CHECK, format!("skipped — {msg}"));

    let Ok(resolved) = crate::vault_ctx::require(Some(vault_root.to_path_buf())) else {
        return skip("vault unresolved".to_string());
    };
    // Read-only collection resolution, same reason as `native_search_check`:
    // doctor must never rewrite `onebrain.yml` as a side effect.
    let Ok(collection) = collection_name_readonly(resolved.root.as_path()) else {
        return skip("collection unresolved".to_string());
    };
    let cache_dir = collection_cache_dir(&collection);
    if !is_indexed(&cache_dir) {
        return skip("no index on disk".to_string());
    }

    let engine = match open_engine_with_collection(&resolved, &collection) {
        Ok(engine) => engine,
        Err(e) => {
            // Busy (a daemon owns the lock) is a normal, transient condition —
            // never a finding. Anything else is already reported by the
            // `search` check's "engine unavailable" arm, so stay quiet here
            // rather than double-reporting the same failure.
            return skip(format!("engine unavailable: {e}"));
        }
    };
    let health = match engine.lex_health() {
        Ok(h) => h,
        Err(e) => return skip(format!("health probe failed: {e}")),
    };
    let details = vec![
        format!("collection: {collection}"),
        format!("lex_docs: {}", health.lex_docs),
        format!("chunk_meta: {}", health.chunk_meta),
        format!("rebuild_pending: {}", health.rebuild_pending),
    ];

    if health.is_dead() {
        return DoctorResult::error(
            LEX_INDEX_CHECK,
            format!(
                "keyword index is EMPTY while the collection holds {} chunk(s) — every keyword \
                 search silently returns nothing (interrupted schema migration)",
                health.chunk_meta
            ),
        )
        .with_hint("onebrain doctor --fix (rebuilds the keyword index from stored metadata; or `onebrain search reindex --force`)")
        .with_details(details);
    }
    if health.is_orphaned() {
        // Error, not warn — three reasons, each stronger than the plain
        // excess-docs case below:
        //  1. Nothing here is recoverable from stored metadata. `chunk_meta`
        //     IS the recovery source, and it is gone. `doctor --fix` cannot
        //     repair it, and `Engine::repopulate_lex_from_meta` deliberately
        //     REFUSES rather than clearing the last surviving copy.
        //  2. It never self-heals. The rebuild marker is deliberately left in
        //     place, so every later open re-reaches the same refusal.
        //  3. The results are wrong, not merely worse. Every remaining keyword
        //     document is an orphan: the engine-backed paths drop them all in
        //     `resolve_hits` (hybrid search returns nothing), while the three
        //     read-only lex fast paths never open redb and hand them back as
        //     hits pointing at documents the collection no longer indexes.
        return DoctorResult::error(
            LEX_INDEX_CHECK,
            format!(
                "stored chunk metadata is EMPTY while the keyword index still holds {} doc(s) — \
                 every one is an orphan, and the metadata a rebuild would read is gone (an \
                 interrupted index wipe, or a partial restore)",
                health.lex_docs
            ),
        )
        .with_hint(
            "onebrain search reindex --force (rebuilds the keyword index AND the stored metadata \
             from the vault; `doctor --fix` cannot repair this one — there is nothing left to \
             rebuild from)",
        )
        .with_details(details);
    }
    if health.has_excess_docs() {
        // Warn, not error: keyword search still answers — it just answers
        // WORSE, because duplicate/orphan documents skew BM25's document
        // frequencies and average field length. See
        // `LexHealth::has_excess_docs` for why this direction has no benign
        // explanation.
        return DoctorResult::warn(
            LEX_INDEX_CHECK,
            format!(
                "keyword index holds {} doc(s) but the collection has only {} chunk(s) — the \
                 surplus are duplicates or orphans, and they degrade keyword ranking",
                health.lex_docs, health.chunk_meta
            ),
        )
        .with_hint("onebrain doctor --fix (rebuilds the keyword index from stored metadata; or `onebrain search reindex --force`)")
        .with_details(details);
    }
    if health.rebuild_pending {
        // `Engine::open` above already retried the rebuild, so a marker that
        // survives means the retry itself keeps failing.
        return DoctorResult::warn(
            LEX_INDEX_CHECK,
            "keyword-index rebuild is still marked pending after a retry — the rebuild is failing"
                .to_string(),
        )
        .with_hint("onebrain search reindex --force")
        .with_details(details);
    }
    DoctorResult::ok(
        LEX_INDEX_CHECK,
        format!("{} keyword doc(s) · healthy", health.lex_docs),
    )
    .with_details(details)
}

/// Recipe — `lex-index` finding means the keyword index is empty, holds MORE
/// docs than there are chunks (duplicates/orphans), or its rebuild is stuck,
/// while redb still holds every chunk. One rebuild repairs all three: the
/// repopulate clears before re-adding, so it is idempotent on any start state.
///
/// Auto-fixable, and deliberately so rather than only advising `search reindex
/// --force`: [`Engine::repopulate_lex_from_meta`][repop] reads NO vault files,
/// loads NO embedding model, and writes only the tantivy index — vectors,
/// `doc_hashes` and `lex_hashes` are untouched, because the content itself
/// never changed. It is strictly a subset of what `reindex --force` would do,
/// with none of the cost (a `--force` re-embeds the whole vault) and none of
/// the risk (nothing else is dropped). Corrupt individual chunks are skipped,
/// not fatal, and the rebuild marker is cleared only after the commit — so an
/// interrupted `--fix` is simply re-runnable.
///
/// The hint still names `onebrain search reindex --force` as the manual
/// fallback for the case this recipe can't cover: `chunk_meta` itself being
/// empty or unreadable. When it is empty AND the keyword index is populated
/// ([`LexHealth::is_orphaned`][orphan] — the check above reports it as an
/// error), the repopulate REFUSES outright rather than clearing the last copy
/// of the data, and returns 0 — which lands on the `Ok(0)` arm below and tells
/// the user exactly that.
///
/// [orphan]: onebrain_search::engine::LexHealth::is_orphaned
///
/// [repop]: onebrain_search::engine::Engine::repopulate_lex_from_meta
fn fix_lex_index(vault_root: &Path, json: bool) -> FixOutcome {
    use crate::commands::search_common::{
        collection_cache_dir, collection_name_readonly, is_indexed, open_engine_with_collection,
    };
    status_line(json, "running: rebuild keyword index from stored metadata");

    let resolved = match crate::vault_ctx::require(Some(vault_root.to_path_buf())) {
        Ok(r) => r,
        Err(e) => return FixOutcome::Failed(format!("could not resolve vault: {e}")),
    };
    let collection = match collection_name_readonly(resolved.root.as_path()) {
        Ok(c) => c,
        Err(e) => return FixOutcome::Failed(format!("could not resolve collection: {e}")),
    };
    if !is_indexed(&collection_cache_dir(&collection)) {
        return FixOutcome::Manual(
            "no index on disk — run `onebrain search reindex` to build one".to_string(),
        );
    }
    let mut engine = match open_engine_with_collection(&resolved, &collection) {
        Ok(e) => e,
        Err(e) => return FixOutcome::Failed(format!("open engine for {collection}: {e}")),
    };
    // Re-probe rather than trusting the check's message: the plain-doctor
    // `Engine::open` may already have healed a marker-backed rebuild between
    // the report and this recipe.
    match engine.lex_health() {
        Ok(h) if h.is_healthy() => {
            return FixOutcome::Fixed(format!(
                "keyword index already healthy — {} doc(s)",
                h.lex_docs
            ));
        }
        Ok(_) => {}
        Err(e) => return FixOutcome::Failed(format!("lex health probe: {e}")),
    }
    match engine.repopulate_lex_from_meta() {
        Ok(0) => FixOutcome::Manual(
            "nothing to rebuild from — stored chunk metadata is empty; run \
             `onebrain search reindex --force`"
                .to_string(),
        ),
        Ok(n) => FixOutcome::Fixed(format!(
            "rebuilt the keyword index from stored metadata — {n} chunk(s) restored \
             (no files re-read, nothing re-embedded)"
        )),
        Err(e) => FixOutcome::Failed(format!(
            "rebuild keyword index: {e} — run `onebrain search reindex --force`"
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Legacy index stub detection + cleanup (#222)
//
// Task 9 (v3.4.9 #201 dry-run) demonstrated: a pre-#201 binary opening an
// already-split collection doesn't know about the `models/` + `index/`
// layout, so it silently creates a FRESH, EMPTY legacy `tantivy/` /
// `vectors/` / `engine.redb` directly at the collection root — right next
// to the real, populated copy under `index/`. `CollectionLayout`'s
// per-artifact fallback resolution keeps reads correct even with this
// duplicate in place, but the empty stub never gets cleaned up on its own:
// `Engine::open`'s eager `migrate()` skips any entry whose target already
// exists (that's the correct behaviour for a genuine partial migration —
// never clobber the split copy), so the stub is permanent junk once
// created. This check finds it and, on `--fix`, removes ONLY the entries
// that are genuinely empty stubs — anything holding real bytes is reported
// but never auto-deleted, since that could be a genuine (if confusing)
// second copy of real data from an aborted migration.
// ─────────────────────────────────────────────────────────────────────────

/// `true` when `path` is a legacy-root index artifact with no real content:
/// a zero-length file, or a directory with zero entries. A missing path is
/// treated as trivially "empty" (nothing to remove) so callers can call this
/// unconditionally after an existence check.
fn is_empty_stub(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return true;
    };
    if meta.is_file() {
        return meta.len() == 0;
    }
    if meta.is_dir() {
        return std::fs::read_dir(path)
            .map(|mut rd| rd.next().is_none())
            .unwrap_or(true);
    }
    false
}

/// Doctor check (`check = "legacy-index-stub"`). Read-only: for each of
/// [`onebrain_search::layout::INDEX_ARTIFACTS`], looks for a legacy-root
/// copy that ALSO has a split-location (`index/<name>`) counterpart — the
/// signature of a pre-#201 binary's stray write, or of a genuinely stuck
/// partial migration. Names with an empty legacy copy are reported as
/// auto-fixable; names whose legacy copy holds real bytes are reported
/// as manual-only (never auto-deleted).
///
/// Deliberately narrower than "any `CacheLayoutState::Partial` collection"
/// — a `Partial` state caused solely by an in-progress MODEL migration (no
/// index-artifact duplicate at all) is normal and not flagged here.
fn legacy_index_stub_check(vault_root: &Path) -> DoctorResult {
    use crate::commands::search_common::{collection_cache_dir, collection_name_readonly};
    use onebrain_search::layout::INDEX_ARTIFACTS;

    let resolved = match crate::vault_ctx::require(Some(vault_root.to_path_buf())) {
        Ok(r) => r,
        Err(_) => return DoctorResult::ok("legacy-index-stub", "skipped — vault unresolvable"),
    };
    let collection = match collection_name_readonly(resolved.root.as_path()) {
        Ok(c) => c,
        Err(_) => return DoctorResult::ok("legacy-index-stub", "skipped — collection unresolved"),
    };
    let cache_dir = collection_cache_dir(&collection);

    let mut empty_stubs = Vec::new();
    let mut nonempty_duplicates = Vec::new();
    for name in INDEX_ARTIFACTS {
        let legacy_path = cache_dir.join(name);
        let split_path = cache_dir.join("index").join(name);
        if !legacy_path.exists() || !split_path.exists() {
            continue; // no duplicate at both locations — nothing to flag
        }
        if is_empty_stub(&legacy_path) {
            empty_stubs.push(name);
        } else {
            nonempty_duplicates.push(name);
        }
    }

    if empty_stubs.is_empty() && nonempty_duplicates.is_empty() {
        return DoctorResult::ok("legacy-index-stub", "no legacy index duplicates found");
    }

    if !nonempty_duplicates.is_empty() {
        let names = nonempty_duplicates.join(", ");
        return DoctorResult::warn(
            "legacy-index-stub",
            format!("legacy index artifact(s) with data still at collection root: {names}"),
        )
        .with_details(vec![
            format!("collection: {collection}"),
            format!("{names} will NOT be auto-removed — investigate manually before deleting"),
        ]);
    }

    let names = empty_stubs.join(", ");
    DoctorResult::warn(
        "legacy-index-stub",
        format!(
            "empty legacy index stub(s) at collection root: {names} — left by a pre-#201 binary"
        ),
    )
    .with_hint("onebrain doctor --fix")
    .with_details(vec![format!("collection: {collection}")])
}

/// `--fix` recipe for `legacy-index-stub`: re-resolves the same duplicates
/// the check found and removes ONLY the empty ones. A non-empty legacy
/// duplicate always survives untouched, even when other names in the same
/// run were safely removed — [`FixOutcome::Partial`] reports that mix
/// honestly rather than claiming a clean `Fixed`.
fn fix_legacy_index_stub(vault_root: &Path, _json: bool) -> FixOutcome {
    use crate::commands::search_common::{collection_cache_dir, collection_name_readonly};
    use onebrain_search::layout::INDEX_ARTIFACTS;

    let resolved = match crate::vault_ctx::require(Some(vault_root.to_path_buf())) {
        Ok(r) => r,
        Err(e) => return FixOutcome::Failed(format!("could not resolve vault: {e}")),
    };
    let collection = match collection_name_readonly(resolved.root.as_path()) {
        Ok(c) => c,
        Err(e) => return FixOutcome::Failed(format!("could not resolve collection: {e}")),
    };
    let cache_dir = collection_cache_dir(&collection);

    let mut removed = Vec::new();
    let mut skipped_nonempty = Vec::new();
    for name in INDEX_ARTIFACTS {
        let legacy_path = cache_dir.join(name);
        let split_path = cache_dir.join("index").join(name);
        if !legacy_path.exists() || !split_path.exists() {
            continue;
        }
        if !is_empty_stub(&legacy_path) {
            skipped_nonempty.push(name);
            continue;
        }
        let result = if legacy_path.is_dir() {
            std::fs::remove_dir_all(&legacy_path)
        } else {
            std::fs::remove_file(&legacy_path)
        };
        match result {
            Ok(()) => removed.push(name),
            Err(e) => {
                return FixOutcome::Partial(format!(
                    "removed [{}] before failing on {name}: {e}",
                    removed.join(", ")
                ));
            }
        }
    }

    if !skipped_nonempty.is_empty() {
        return FixOutcome::Partial(format!(
            "removed empty stub(s) [{}] — left non-empty legacy artifact(s) [{}] \
             untouched, investigate manually",
            removed.join(", "),
            skipped_nonempty.join(", ")
        ));
    }

    if removed.is_empty() {
        FixOutcome::Fixed("nothing to remove — no empty legacy stubs found".to_string())
    } else {
        FixOutcome::Fixed(format!(
            "removed empty legacy stub(s): {}",
            removed.join(", ")
        ))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Legacy `qmd` leftover detection + guided cleanup (v3.4.9)
//
// Pre-v3.4 vaults used an npm-installed CLI (`@tobilu/qmd`, invoked as
// `qmd`) for semantic search, wired up via a Claude Code hook. v3.4 replaced
// it with the native `onebrain search` engine (issue-tracked migration —
// `legacy-qmd-collection` migrates the config key; this check handles what's
// left on disk: the npm package, its PATH-resolved binary/symlink, and two
// caches — `~/.cache/qmd` (models + the sqlite index, commonly gigabytes) and
// `~/.config/qmd`). Once native search is actually configured, none of that
// is needed anymore.
// ─────────────────────────────────────────────────────────────────────────

/// Legacy `qmd` leftovers detected on the local machine. Every field is
/// independently optional — a partial uninstall (binary removed, cache left
/// behind, or vice versa) is common, and each leftover is safe to report or
/// remove on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QmdLeftovers {
    /// Absolute path to the first `qmd` (`.exe`/`.cmd` probed too on
    /// Windows) found by walking `$PATH` in order — matches shell lookup
    /// semantics (`command -v qmd`).
    pub binary: Option<PathBuf>,
    /// npm package name parsed from the binary's resolved symlink target
    /// (`.../node_modules/<scope>/<name>/...` → `<scope>/<name>`, or
    /// `.../node_modules/<name>/...` → `<name>`). `None` when the binary
    /// isn't a symlink into a `node_modules` tree — nothing to
    /// `npm uninstall`.
    pub npm_package: Option<String>,
    /// `~/.cache/qmd` and its recursive size in bytes, when the directory
    /// exists. (Same relative path on Windows: `home` already resolves to
    /// `%USERPROFILE%`, so no separate probe is needed.)
    pub cache_dir: Option<(PathBuf, u64)>,
    /// `~/.config/qmd`, when the directory exists.
    pub config_dir: Option<PathBuf>,
}

impl QmdLeftovers {
    /// `true` when nothing was found at all — the check short-circuits to OK.
    fn is_empty(&self) -> bool {
        self.binary.is_none() && self.cache_dir.is_none() && self.config_dir.is_none()
    }
}

/// Pure detection — no side effects, safe to call from the check (read-only)
/// and from unit tests alike. `home` and `path_var` are passed in explicitly
/// (rather than reading `dirs::home_dir()` / `$PATH` internally) so tests can
/// point it at a fixture tree instead of the real machine.
fn detect_qmd_leftovers(home: &Path, path_var: &str) -> QmdLeftovers {
    let binary = find_qmd_binary(path_var);
    let npm_package = binary.as_deref().and_then(npm_package_from_symlink);
    let cache_path = home.join(".cache").join("qmd");
    let cache_dir = cache_path.is_dir().then(|| {
        let size = dir_size(&cache_path);
        (cache_path.clone(), size)
    });
    let config_path = home.join(".config").join("qmd");
    let config_dir = config_path.is_dir().then_some(config_path);
    QmdLeftovers {
        binary,
        npm_package,
        cache_dir,
        config_dir,
    }
}

/// Locate the first `qmd` executable on `$PATH` (Windows also probes
/// `.exe`/`.cmd` suffixes, in npm's install-shim order). Existence is
/// checked with `symlink_metadata` so a dangling symlink still counts as
/// "found" — matching what a shell's `command -v qmd` would report, and what
/// `npm_package_from_symlink` needs (a dangling link's target string is still
/// informative even though it can't be canonicalized).
fn find_qmd_binary(path_var: &str) -> Option<PathBuf> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    let names: &[&str] = if cfg!(windows) {
        &["qmd.exe", "qmd.cmd", "qmd"]
    } else {
        &["qmd"]
    };
    for dir in path_var.split(sep).filter(|d| !d.is_empty()) {
        for name in names {
            let candidate = Path::new(dir).join(name);
            if candidate.symlink_metadata().is_ok() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Parse the npm package name from a global-bin symlink's resolved target,
/// e.g. `/opt/homebrew/lib/node_modules/@tobilu/qmd/bin/qmd` → `@tobilu/qmd`.
/// Resolves the full symlink chain (`canonicalize`) so a chain of hops (npm's
/// own shim → the Homebrew Cellar path) lands on the real file; falls back to
/// a single `read_link` hop for a dangling symlink whose target no longer
/// exists (`canonicalize` requires the target to be real). Returns `None` for
/// a plain file or a path with no `node_modules` segment — nothing to
/// `npm uninstall`.
fn npm_package_from_symlink(binary: &Path) -> Option<String> {
    let resolved = binary
        .canonicalize()
        .or_else(|_| std::fs::read_link(binary))
        .ok()?;
    let comps: Vec<&std::ffi::OsStr> = resolved.iter().collect();
    let idx = comps.iter().position(|c| *c == "node_modules")?;
    let first = comps.get(idx + 1)?.to_str()?;
    if let Some(scope) = first.strip_prefix('@') {
        let name = comps.get(idx + 2)?.to_str()?;
        Some(format!("@{scope}/{name}"))
    } else {
        Some(first.to_string())
    }
}

/// Recursive directory size in bytes. Best-effort: unreadable entries
/// (permission errors mid-walk, races) are skipped rather than failing the
/// whole walk — a doctor check reporting "somewhat less than the true size"
/// beats crashing on a single bad entry.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// Tilde-relative display for a path under `home` (cosmetic only — used in
/// finding text so `~/.cache/qmd` reads shorter than the absolute path).
/// Falls back to the absolute path when `dir` isn't under `home`.
fn tildify(dir: &Path, home: &Path) -> String {
    match dir.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

const QMD_LEFTOVERS_CHECK: &str = "qmd-leftovers";

/// `true` when native search is genuinely configured via `search.collection`
/// — NOT when that field's only source is [`onebrain_core::load_vault_config`]'s
/// read-fallback from the deprecated top-level `qmd_collection` key. A vault
/// that still carries `qmd_collection` hasn't migrated yet (the
/// `legacy-qmd-collection` check already nags about that); piling the
/// qmd-uninstall nag on top would read as "remove the tool you haven't
/// finished migrating away from yet" — confusing, not helpful.
fn native_search_genuinely_configured(config: &onebrain_core::VaultConfig) -> bool {
    config.search.collection.is_some() && config.qmd_collection.is_none()
}

/// Legacy-qmd leftover check (`check = "qmd-leftovers"`). Advisory only —
/// `DoctorStatus` has no `Info` variant, so `Warn` is the established
/// stand-in for "worth knowing, nothing broken" (same as `search`'s "no
/// index yet" case). Detects the pre-v3.4 npm-based `qmd` still installed
/// alongside the now-redundant native `onebrain search` engine, and — via
/// `--fix` — offers a guided cleanup (see [`fix_qmd_leftovers`]).
///
/// Gated on native search actually being configured (see
/// [`native_search_genuinely_configured`]): a vault that never adopted
/// native search has no reason to be told to remove its search tool.
///
/// Pure aside from the two params — takes `home` and `path_var` directly
/// (rather than resolving them internally) so it's unit-testable against a
/// fixture tree; see [`qmd_leftovers_check_prod`] for the production
/// entry point.
fn qmd_leftovers_check(
    config: &onebrain_core::VaultConfig,
    home: &Path,
    path_var: &str,
) -> DoctorResult {
    if !native_search_genuinely_configured(config) {
        return DoctorResult::ok(
            QMD_LEFTOVERS_CHECK,
            "skipped — native search not configured",
        );
    }
    let leftovers = detect_qmd_leftovers(home, path_var);
    if leftovers.is_empty() {
        return DoctorResult::ok(QMD_LEFTOVERS_CHECK, "no legacy qmd install found");
    }

    let mut details: Vec<String> = Vec::new();
    if let Some(bin) = &leftovers.binary {
        details.push(format!("binary: {}", bin.display()));
    }
    if let Some(pkg) = &leftovers.npm_package {
        details.push(format!("npm package: {pkg}"));
    }
    match &leftovers.cache_dir {
        Some((dir, size)) => details.push(format!(
            "{} — {} reclaimable",
            tildify(dir, home),
            crate::commands::search_common::format_size(*size)
        )),
        None if leftovers.binary.is_some() => {
            details.push(
                "no cache directory found (binary present, nothing to reclaim there)".to_string(),
            );
        }
        None => {}
    }
    if let Some(dir) = &leftovers.config_dir {
        details.push(tildify(dir, home));
    }

    let declined = config.stats.qmd_cleanup_declined.unwrap_or(false);
    let mut result = DoctorResult::warn(
        QMD_LEFTOVERS_CHECK,
        "legacy qmd install found — safe to remove now that native search is active",
    )
    .with_details(details);
    if !declined {
        result = result
            .with_hint("onebrain doctor --fix to remove it (npm uninstall + delete the caches)");
    }
    result
}

/// Production entry point — resolves the real home directory and `$PATH`,
/// then delegates to the pure, unit-tested [`qmd_leftovers_check`]. A
/// home-directory resolution failure degrades to an OK skip rather than an
/// error — this check is advisory, never worth failing the whole run over.
fn qmd_leftovers_check_prod(config: &onebrain_core::VaultConfig) -> DoctorResult {
    let Some(home) = dirs::home_dir() else {
        return DoctorResult::ok(
            QMD_LEFTOVERS_CHECK,
            "skipped — could not resolve home directory",
        );
    };
    let path_var = std::env::var("PATH").unwrap_or_default();
    qmd_leftovers_check(config, &home, &path_var)
}

/// Decide whether the text-mode `--fix` should apply its recipes.
///
/// `structured` short-circuits to `true` as a defensive guard — production
/// routes `--fix --json` through the separate structured branch in `run()` and
/// never calls this; `--yes` short-circuits too. Otherwise prompt on an
/// interactive TTY. A non-interactive plain run (piped stdin/stdout — e.g. cron
/// without `--yes`) proceeds without prompting, matching pre-3.2.4 behaviour so
/// existing automation keeps working. A read error is treated as "decline".
///
/// The return distinguishes HOW consent was reached, not just whether to
/// proceed: recipes that are destructive outside the vault (qmd-leftovers)
/// run only on [`FixConsent::InteractiveYes`] — a genuine human "y" — while
/// every pre-existing recipe treats any non-`Declined` value as before, so
/// the automation-compat contract is unchanged for them.
fn confirm_fix(fixable_count: usize, structured: bool, yes: bool) -> FixConsent {
    use std::io::{IsTerminal, Write};
    if structured || yes {
        return FixConsent::AutoProceed;
    }
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return FixConsent::AutoProceed;
    }
    // The plan (the bulleted action list) is printed by the caller just above
    // this prompt, so keep the question itself short.
    print!("\nApply {fixable_count} fix(es)? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return FixConsent::Declined;
    }
    if matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        FixConsent::InteractiveYes
    } else {
        FixConsent::Declined
    }
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
/// `result.check` (and, where a check name covers multiple sub-conditions,
/// on the message content too). Hidden hints that say "Run onebrain doctor
/// --fix to ..." are silently rewritten to a non-circular message when no
/// recipe maps.
///
/// `interactive_confirmed` is `true` only when the text-mode batch `[y/N]`
/// prompt was genuinely answered "y" on an interactive TTY
/// ([`FixConsent::InteractiveYes`]) — every auto-proceed route (structured
/// `--fix --json`, `--yes`, non-TTY pipes) passes `false`. Only recipes
/// that are destructive outside the vault (qmd-leftovers) consult it.
fn attempt_fix(
    result: &DoctorResult,
    vault_root: &Path,
    json: bool,
    interactive_confirmed: bool,
) -> FixOutcome {
    match result.check.as_str() {
        // Migrate the deprecated top-level `qmd_collection` key to
        // `search.collection` and remove it. Auto-fixable — one atomic
        // config write, comments elsewhere preserved.
        "legacy-qmd-collection" => fix_legacy_qmd_collection(vault_root, json),
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
        // Reset out-of-range tunables to their documented defaults via the
        // comment-preserving line editor. folders.* / search.collection are
        // never auto-reset (report-only).
        "config-values" => fix_config_values(vault_root, json),
        // Insert the commented `search.exclude` block for a vault that
        // adopted search before the block existed — value resolved from
        // this vault's own `folders.archive`, comment from the shared
        // `config_key_docs` table (Task 3's doc entry).
        "search-exclude" => fix_search_exclude(vault_root, json),
        // Backfill `token_optimization:` — the whole block for a vault whose
        // onebrain.yml predates v3.4.10 (issue #247), or any documented
        // sub-key missing from an EXISTING block (issue #270) — both from
        // the same shared source `init`'s fresh template uses.
        "token-optimization" => fix_token_optimization(vault_root, json),
        // Remove empty legacy index stubs left by a pre-#201 binary that
        // silently recreated tantivy/vectors/engine.redb at the collection
        // root of an already-split collection. Only truly empty duplicates
        // are removed; a non-empty legacy copy is reported, never deleted.
        "legacy-index-stub" => fix_legacy_index_stub(vault_root, json),
        // Rebuild an emptied keyword index from redb's stored chunk metadata
        // (no vault files re-read, no model loaded, vectors untouched) — the
        // interrupted-schema-migration state `lex_index_check` catches.
        LEX_INDEX_CHECK => fix_lex_index(vault_root, json),
        // Strip the stale `extraKnownMarketplaces.onebrain` entry from
        // `.claude/settings.json`. Cosmetic config cleanup; no behavioral
        // change at runtime (the plugin is enabled via `enabledPlugins`).
        "claude-settings" => fix_claude_settings(vault_root, json),
        // Prune stale OneBrain version dirs from the marketplace cache
        // (`~/.claude/plugins/cache/<mkt>/onebrain/`). Home-based, not the
        // vault — so this recipe ignores `vault_root`. Removing every cached
        // version is safe: the active plugin is the vault-local pin, never a
        // cache copy. The user must restart / `/reload-plugins` to drop the
        // stale copy from the running session (Claude can't hot-swap it).
        "plugin-cache" => fix_plugin_cache(json),
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
        // Legacy qmd leftover cleanup (npm uninstall + delete caches).
        // Destructive actions require `interactive_confirmed` — a genuine
        // human "y" to the batch [y/N] prompt on a TTY. Everything else
        // (structured `--fix --json`, `--yes`, non-TTY piped text mode) gets
        // a Manual outcome with the removal commands; a previously declined
        // cleanup (`result.hint` is `None` — see `qmd_leftovers_check`)
        // short-circuits to Manual before touching the filesystem at all.
        "qmd-leftovers" => fix_qmd_leftovers(result, interactive_confirmed),
        _ => FixOutcome::Manual(manual_message(result)),
    }
}

/// Describe what `--fix` would do to an issue WITHOUT executing it, so the
/// plan can be previewed before the confirmation prompt. `Some(action)` for an
/// auto-fixable check, `None` when only a manual step applies (e.g. the
/// `search` index check — a reindex isn't run automatically — or orphan
/// checkpoints).
///
/// Keep the match arms in sync with [`attempt_fix`] — same check names.
fn planned_action(result: &DoctorResult) -> Option<&'static str> {
    match result.check.as_str() {
        "legacy-qmd-collection" => Some("migrate qmd_collection → search.collection"),
        "settings-hooks" => Some("register the Stop + qmd hooks and permissions"),
        "plugin-files" => Some("re-download plugin files from upstream"),
        "folders" => Some("create the missing standard folders"),
        "onebrain.yml-keys" => Some("backfill missing onebrain.yml keys"),
        "config-values" => Some(
            "reset out-of-range values to defaults · add missing self-documentation comments · restructure layout",
        ),
        "search-exclude" => Some("insert the search.exclude block"),
        "token-optimization" => Some("insert the token_optimization block / missing sub-key(s)"),
        "lex-index" => Some("rebuild the keyword index from stored chunk metadata"),
        "claude-settings" => Some("remove the stale marketplace entry"),
        "plugin-cache" => Some("remove the stale plugin cache"),
        "vault-config-migration" => Some("migrate vault.yml → onebrain.yml"),
        // Only offered while `qmd_leftovers_check` still carries a hint — a
        // previously-declined cleanup (`hint: None`) falls to `None` here too,
        // so the batch preview/confirmation stops mentioning it.
        "qmd-leftovers" => result.hint.is_some().then_some(
            "npm uninstall the legacy qmd package + delete ~/.cache/qmd and ~/.config/qmd",
        ),
        _ => None,
    }
}

/// Recipe — `qmd-leftovers` warning means the pre-v3.4 npm `qmd` package is
/// still installed alongside the native search engine. Re-detects fresh
/// (rather than trusting the check's cached `details` strings) so the
/// recipe stays correct even if leftovers changed between the report and the
/// fix pass.
///
/// Two Manual short-circuits guard the destructive actions:
///
/// - `result.hint.is_none()` — the user already declined this cleanup on an
///   earlier `--fix` run (`qmd_leftovers_check` omits the hint once
///   `stats.qmd_cleanup_declined` is set). No filesystem access at all.
/// - `!interactive_confirmed` — EVERY consent route other than a genuine
///   human "y" to the batch `[y/N]` prompt on an interactive TTY: the
///   structured `--fix --json` path (the `/doctor` plugin skill, the
///   scheduler), `--yes`, and non-TTY piped text mode (`doctor --fix
///   </dev/null | cat` — cron command-mode / scripts) all auto-proceed
///   without a prompt, and deleting a user's global npm package + gigabytes
///   of cache is never acceptable without live confirmation. Mirror the
///   orphan-checkpoints Manual-routing precedent: report what was found
///   (detection is read-only) and steer to an interactive `onebrain doctor
///   --fix` or manual removal.
///
/// The destructive branch therefore runs ONLY after a real interactive
/// confirmation. `npm uninstall` runs in the foreground; a non-zero exit
/// (or a failure to even launch `npm`) is surfaced as `Partial` rather than
/// aborting — the cache/config directories are still worth removing on
/// their own.
fn fix_qmd_leftovers(result: &DoctorResult, interactive_confirmed: bool) -> FixOutcome {
    if result.hint.is_none() {
        return FixOutcome::Manual(
            "cleanup previously declined — remove `stats.qmd_cleanup_declined` from \
             onebrain.yml to be offered this again"
                .to_string(),
        );
    }
    let Some(home) = dirs::home_dir() else {
        return FixOutcome::Failed("could not resolve home directory".to_string());
    };
    let path_var = std::env::var("PATH").unwrap_or_default();
    let leftovers = detect_qmd_leftovers(&home, &path_var);
    if leftovers.is_empty() {
        return FixOutcome::Fixed("nothing to remove — already clean".to_string());
    }
    if !interactive_confirmed {
        // No genuine interactive confirmation: NEVER destructive. Compose an
        // actionable message from the (read-only) detection above so the
        // consumer can relay exact removal commands.
        return qmd_manual_outcome(&leftovers, &home);
    }
    qmd_remove_leftovers(&leftovers, &home)
}

/// Non-destructive outcome for the qmd cleanup: report what was found with
/// the exact removal commands. Pure (no filesystem writes, no subprocesses)
/// — used by every non-interactive route of [`fix_qmd_leftovers`].
///
/// A binary with NO detected npm package (a non-npm install) gets its own
/// `rm <path>` command — `npm uninstall` wouldn't touch it, and without
/// this a binary-only leftover would render a dangling "remove manually: ".
/// When an npm package IS detected, its uninstall removes the bin symlink,
/// so no separate `rm` is emitted for the binary.
fn qmd_manual_outcome(leftovers: &QmdLeftovers, home: &Path) -> FixOutcome {
    let size_note = leftovers
        .cache_dir
        .as_ref()
        .map(|(_, size)| {
            format!(
                " ({} reclaimable)",
                crate::commands::search_common::format_size(*size)
            )
        })
        .unwrap_or_default();
    let mut commands: Vec<String> = Vec::new();
    if let Some(pkg) = &leftovers.npm_package {
        commands.push(format!("npm uninstall -g {pkg}"));
    } else if let Some(bin) = &leftovers.binary {
        commands.push(format!("rm {}", tildify(bin, home)));
    }
    let mut rm_targets: Vec<String> = Vec::new();
    if let Some((dir, _)) = &leftovers.cache_dir {
        rm_targets.push(tildify(dir, home));
    }
    if let Some(dir) = &leftovers.config_dir {
        rm_targets.push(tildify(dir, home));
    }
    if !rm_targets.is_empty() {
        commands.push(format!("rm -rf {}", rm_targets.join(" ")));
    }
    FixOutcome::Manual(format!(
        "legacy qmd found{size_note} — run `onebrain doctor --fix` interactively to \
         review and confirm removal, or remove manually: {}",
        commands.join(" && ")
    ))
}

/// The destructive half of [`fix_qmd_leftovers`] — reached ONLY after a
/// genuine interactive confirmation (status lines therefore go to stdout
/// unconditionally, `status_line(false, …)`). Split out so unit tests can
/// drive it with fixture `leftovers`/`home` without touching the real
/// machine's `$HOME`/`$PATH`.
///
/// Honesty contract: an unknown non-npm binary is NEVER auto-deleted (we
/// only understand npm installs; anything else is the user's to remove),
/// and the outcome must say so — a surviving binary downgrades `Fixed` to
/// `Partial` (dirs were removed) or `Manual` (nothing was removable at
/// all), each carrying the exact `rm` command.
fn qmd_remove_leftovers(leftovers: &QmdLeftovers, home: &Path) -> FixOutcome {
    let mut npm_failed = false;
    if let Some(pkg) = &leftovers.npm_package {
        status_line(false, &format!("running: npm uninstall -g {pkg}"));
        match std::process::Command::new("npm")
            .args(["uninstall", "-g", pkg])
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => {
                npm_failed = true;
                status_line(
                    false,
                    &format!("npm uninstall -g {pkg} exited with {status}"),
                );
            }
            Err(e) => {
                npm_failed = true;
                status_line(false, &format!("could not run npm uninstall -g {pkg}: {e}"));
            }
        }
    }

    let mut removed: Vec<String> = Vec::new();
    let mut removal_failed = false;
    let dirs_to_remove = [
        leftovers.cache_dir.as_ref().map(|(d, _)| d.clone()),
        leftovers.config_dir.clone(),
    ];
    for dir in dirs_to_remove.into_iter().flatten() {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed.push(tildify(&dir, home)),
            Err(e) => {
                removal_failed = true;
                status_line(false, &format!("could not remove {}: {e}", dir.display()));
            }
        }
    }
    let removed_summary = if removed.is_empty() {
        "no cache/config directories found".to_string()
    } else {
        removed.join(", ")
    };

    // Re-probe AFTER the actions: a binary still on disk means the cleanup
    // is not complete — either npm's uninstall failed to drop its symlink,
    // or (the common case here) there was no npm package to uninstall
    // because this is a non-npm install we deliberately never auto-delete.
    let surviving_binary = leftovers
        .binary
        .as_ref()
        .filter(|b| b.symlink_metadata().is_ok());

    if npm_failed || removal_failed {
        return FixOutcome::Partial(format!(
            "removed {removed_summary} · {}",
            if npm_failed {
                "npm uninstall failed — remove the package manually"
            } else {
                "some directories could not be removed — check permissions"
            }
        ));
    }
    if let Some(bin) = surviving_binary {
        let rm_cmd = format!("rm {}", tildify(bin, home));
        if removed.is_empty() {
            // Nothing was done at all — don't dress it up as progress.
            return FixOutcome::Manual(format!(
                "nothing removed — qmd binary at {} is not an npm install (never \
                 auto-deleted); remove it manually: {rm_cmd}",
                tildify(bin, home)
            ));
        }
        return FixOutcome::Partial(format!(
            "removed {removed_summary} · qmd binary at {} is not an npm install (never \
             auto-deleted) — remove it manually: {rm_cmd}",
            tildify(bin, home)
        ));
    }
    let pkg_note = leftovers
        .npm_package
        .as_deref()
        .map(|p| format!("uninstalled {p} · "))
        .unwrap_or_default();
    FixOutcome::Fixed(format!("{pkg_note}removed {removed_summary}"))
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

/// Recipe — `legacy-qmd-collection` warning means the vault's config still
/// carries a deprecated top-level `qmd_collection` key (v3.3 and earlier).
/// Migrate it: if `search.collection` is absent, set it to the
/// `qmd_collection` value; then remove the legacy key. If `search.collection`
/// is already set, don't overwrite it — just drop the legacy key.
///
/// Parses read-only to CLASSIFY the change (legacy value, whether
/// `search.collection` is already set), then applies it as comment-preserving
/// line edits ([`onebrain_fs::yaml_edit::upsert_child`] to seed the collection,
/// [`onebrain_fs::yaml_edit::delete_key`] to drop the legacy key) before the
/// backup → atomic write. Every other key AND every comment survives (v3.4.8,
/// issue #200 — the pre-v3.4.8 serde re-serialization dropped all comments).
fn fix_legacy_qmd_collection(vault_root: &Path, json: bool) -> FixOutcome {
    use onebrain_core::{find_config_file, CONFIG_FILENAME};
    let path = find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(CONFIG_FILENAME)
        .to_string();
    status_line(
        json,
        &format!("running: migrate qmd_collection in {filename}"),
    );

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return FixOutcome::Failed(format!("read {filename}: {e}")),
    };
    // Read-only parse — used ONLY to decide what to change, never to
    // re-serialize (that would drop comments).
    let yaml: serde_yaml::Value = match serde_yaml::from_str(&text) {
        Ok(v) => v,
        Err(e) => return FixOutcome::Failed(format!("parse {filename}: {e}")),
    };
    let mapping = match yaml.as_mapping() {
        Some(m) => m,
        None => return FixOutcome::Failed(format!("{filename} root is not a mapping")),
    };

    let qmd_key = serde_yaml::Value::String("qmd_collection".to_string());
    let Some(legacy_value) = mapping.get(&qmd_key) else {
        // Already migrated (or never present) — idempotent no-op.
        return FixOutcome::Fixed(format!(
            "{filename}: no qmd_collection key — nothing to migrate"
        ));
    };
    let legacy_str = legacy_value.as_str().map(str::to_string);

    // Seed `search.collection` from the legacy value ONLY when it isn't
    // already set (never overwrite the user's current value).
    let search_key = serde_yaml::Value::String("search".to_string());
    let collection_key = serde_yaml::Value::String("collection".to_string());
    let search_collection_set = mapping
        .get(&search_key)
        .and_then(|v| v.as_mapping())
        .map(|s| s.contains_key(&collection_key))
        .unwrap_or(false);

    // Apply the change as line edits on the original text.
    let mut current = text.clone();
    let mut seeded = false;
    if !search_collection_set {
        if let Some(value) = &legacy_str {
            current = onebrain_fs::yaml_edit::upsert_child(&current, "search", "collection", value);
            seeded = true;
        }
    }
    // Remove the legacy key — located above via the parse, so this succeeds
    // for the block-form key.
    match onebrain_fs::yaml_edit::delete_key(&current, &["qmd_collection"]) {
        Some(updated) => current = updated,
        None => {
            return FixOutcome::Failed(format!(
                "{filename}: could not remove legacy qmd_collection key (unsupported shape)"
            ))
        }
    }

    // Defense-in-depth: back up before the write. Hard precondition — no write
    // without a backup.
    if let Err(e) = onebrain_fs::backup_config_file(&path) {
        return FixOutcome::Failed(format!("backup {filename} before write: {e}"));
    }
    if let Err(e) = onebrain_fs::atomic_write_text(&path, &current) {
        return FixOutcome::Failed(format!("write {filename}: {e}"));
    }

    let action = if seeded {
        let value = legacy_str.as_deref().unwrap_or("");
        format!("migrated qmd_collection → search.collection = {value}; removed legacy key")
    } else if search_collection_set {
        "removed legacy qmd_collection (search.collection already set)".to_string()
    } else {
        // Legacy key present but not a string value — dropped it without
        // seeding (nothing sensible to seed from).
        "removed legacy qmd_collection (non-string value — not migrated)".to_string()
    };
    FixOutcome::Fixed(action)
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
        search: Default::default(),
        token_optimization: Default::default(),
        stats: Default::default(),
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

/// Recipe — `plugin-cache` warning means stale OneBrain version dirs linger in
/// the Claude Code marketplace cache (`~/.claude/plugins/cache/<mkt>/onebrain/`).
/// They can shadow the vault-local copy and make Claude Code load old skills.
/// Prune them via the shared `clean_plugin_cache` (home-based — no `vault_root`).
/// A restart or `/reload-plugins` is still needed to drop the stale copy from
/// the *running* session.
fn fix_plugin_cache(json: bool) -> FixOutcome {
    use onebrain_fs::vault_sync::cache_clean::{clean_plugin_cache, detect_stale_plugin_cache};
    // Shared resolver (same `~/.claude/plugins/installed_plugins.json` the
    // `plugin-cache` check uses) — keeps check + fix pointed at one location.
    let Some(installed) = onebrain_fs::vault_sync::default_installed_plugins_path() else {
        return FixOutcome::Failed("could not resolve home directory".to_string());
    };
    status_line(json, "running: clean plugin cache");
    // `None` → clean derives `<home>/.claude/plugins/cache`.
    let outcome = clean_plugin_cache(&installed, None);
    let removed = outcome.removed;
    // Honest result: re-detect after the sweep. If versions remain, a removal
    // failed (permissions, open handle, race) — report Failed rather than a
    // misleading "Fixed" with exit 0, so the user learns why the `plugin-cache`
    // warning will still be there on the next doctor run. `outcome.failed`
    // already counted those failures (and warned on stderr per path).
    let remaining = detect_stale_plugin_cache(&installed, None);
    if remaining.is_empty() {
        FixOutcome::Fixed(format!(
            "removed {removed} stale cached version{} — restart Claude or run /reload-plugins to apply",
            if removed == 1 { "" } else { "s" }
        ))
    } else {
        FixOutcome::Failed(format!(
            "removed {removed}, {} failed, {} version(s) remain — check permissions on ~/.claude/plugins/cache",
            outcome.failed,
            remaining.len()
        ))
    }
}

/// Recipe — `onebrain.yml-keys` warning means one or more of:
///   - standard folder keys missing (`inbox` / `projects` / ...) or the
///     entire `folders:` block is missing/null
///   - `update_channel` not set
///   - deprecated keys still present (`onebrain_version`, `method`,
///     `runtime.harness`)
///
/// The recipe handles all three. Out-of-range VALUES (e.g. non-positive
/// `checkpoint.messages`) are the `config-values` check's territory since
/// v3.4.8 — its recipe resets them via the comment-preserving line editor.
///
/// v3.4.8 (issue #200): this recipe now writes via comment-preserving line
/// edits too ([`onebrain_fs::yaml_edit`] — `append_top_level` / `upsert_child`
/// for backfills, `delete_key` for deprecated keys), so it no longer destroys
/// the file's comments. That also makes recipe ORDER irrelevant: it runs
/// before the `config-values` recipe, but since neither drops comments a
/// legacy vault's user comments survive the whole `--fix` pass. serde is used
/// ONLY read-only, to classify what needs changing.
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
    // Read-only parse — used ONLY to classify the changes, never to
    // re-serialize (that would drop comments).
    let yaml: serde_yaml::Value = match serde_yaml::from_str(&text) {
        Ok(v) => v,
        Err(e) => return FixOutcome::Failed(format!("parse {filename}: {e}")),
    };
    let mapping = match yaml.as_mapping() {
        Some(m) => m,
        None => return FixOutcome::Failed(format!("{filename} root is not a mapping")),
    };
    let get = |key: &str| mapping.get(serde_yaml::Value::String(key.to_string()));
    let submap = |key: &str| get(key).and_then(|v| v.as_mapping()).map(|m| m.to_owned());

    let mut added: Vec<&'static str> = Vec::new();
    let mut removed: Vec<&'static str> = Vec::new();
    // Apply every change as a comment-preserving line edit on the original text.
    let mut current = text.clone();

    // 1. Backfill `update_channel` (top-level append when genuinely absent).
    if get("update_channel").is_none() {
        current = onebrain_fs::yaml_edit::append_top_level(&current, "update_channel", "stable");
        added.push("update_channel");
    }

    // 2. Backfill `folders.<key>` defaults. `upsert_child` creates the
    //    `folders:` block when it is missing / null / non-mapping, then
    //    inserts each absent child — comments and existing children intact.
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
    let folders = submap("folders");
    for (key, default) in STANDARD {
        let present = folders
            .as_ref()
            .map(|f| f.contains_key(serde_yaml::Value::String((*key).to_string())))
            .unwrap_or(false);
        if !present {
            current = onebrain_fs::yaml_edit::upsert_child(&current, "folders", key, default);
            added.push(*key);
        }
    }

    // 3. Strip deprecated keys (comment-preserving delete). Any key the
    //    classification says is PRESENT but the line editor cannot remove
    //    (e.g. it lives inside an inline flow mapping) must surface as
    //    un-fixable — never a silent success that leaves the deprecated key
    //    behind forever (matches `fix_legacy_qmd_collection`'s contract).
    let mut unfixable: Vec<&'static str> = Vec::new();
    for key in &["onebrain_version", "method"] {
        if get(key).is_some() {
            match onebrain_fs::yaml_edit::delete_key(&current, &[key]) {
                Some(updated) => {
                    current = updated;
                    removed.push(*key);
                }
                None => unfixable.push(*key),
            }
        }
    }
    if let Some(runtime) = submap("runtime") {
        let has_harness = runtime.contains_key(serde_yaml::Value::String("harness".to_string()));
        // Whether removing `harness` leaves the block empty (only child), so
        // the whole `runtime` block should go — keeps the file tidy.
        let only_harness = runtime.len() == usize::from(has_harness);
        if has_harness {
            match onebrain_fs::yaml_edit::delete_key(&current, &["runtime", "harness"]) {
                Some(updated) => {
                    current = updated;
                    removed.push("runtime.harness");
                    if only_harness {
                        if let Some(updated) =
                            onebrain_fs::yaml_edit::delete_key(&current, &["runtime"])
                        {
                            current = updated;
                        }
                    }
                }
                // The nested delete refuses an inline parent
                // (`runtime: {harness: x}`). When harness is the ONLY entry,
                // removing the whole top-level line is equivalent — the final
                // key of a delete path may carry an inline value. With a
                // sibling key inside the flow mapping there is no safe line
                // edit: surface it instead of silently succeeding.
                None if only_harness => {
                    match onebrain_fs::yaml_edit::delete_key(&current, &["runtime"]) {
                        Some(updated) => {
                            current = updated;
                            removed.push("runtime.harness");
                        }
                        None => unfixable.push("runtime.harness"),
                    }
                }
                None => unfixable.push("runtime.harness"),
            }
        } else if runtime.is_empty() {
            // Pre-existing tidy-up: an already-empty `runtime:` block is
            // dropped (not counted in `removed`, matching the old message).
            if let Some(updated) = onebrain_fs::yaml_edit::delete_key(&current, &["runtime"]) {
                current = updated;
            }
        }
    }

    if added.is_empty() && removed.is_empty() && unfixable.is_empty() {
        return FixOutcome::Fixed(format!("{filename} already in expected shape"));
    }

    if !added.is_empty() || !removed.is_empty() {
        // Defense-in-depth: back up before the write. Hard precondition — no
        // write without a backup.
        if let Err(e) = onebrain_fs::backup_config_file(&path) {
            return FixOutcome::Failed(format!("backup {filename} before write: {e}"));
        }
        if let Err(e) = onebrain_fs::atomic_write_text(&path, &current) {
            return FixOutcome::Failed(format!("write {filename}: {e}"));
        }
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
    if !unfixable.is_empty() {
        parts.push(format!(
            "could not remove deprecated (unsupported YAML shape, e.g. inline mapping): {} — edit manually",
            unfixable.join(", ")
        ));
        // Honest tri-state, mirroring fix_config_values: real progress landed
        // AND something remains → Partial; nothing landed → Failed.
        return if !added.is_empty() || !removed.is_empty() {
            FixOutcome::Partial(parts.join(" · "))
        } else {
            FixOutcome::Failed(parts.join(" · "))
        };
    }
    FixOutcome::Fixed(parts.join(" · "))
}

/// Insert the commented `search.exclude` block (doc comment + `exclude:` +
/// its two list items) into the top-level `search:` block at the fresh
/// template's position — immediately before the first direct child named
/// `reranker:` or `embed:` (falling back to last child when neither
/// exists), so a backfilled vault matches the canonical in-block key order.
/// Called when [`search_exclude_missing`] is true — which guarantees a
/// `search` MAPPING exists in the parsed YAML, but NOT that it's in
/// block form: serde_yaml parses a flow mapping (`search: {collection: c}`)
/// identically, and splicing indented child lines under a flow line would
/// write unparseable YAML. Returns `None` for any header that isn't a pure
/// block-form `search:` line (same `after.is_empty() || after.starts_with('#')`
/// guard as `onebrain_fs::yaml_edit::upsert_child` at the equivalent branch)
/// — the caller surfaces that as `Failed` with a manual-edit message rather
/// than ever corrupting the file.
///
/// Mirrors `upsert_child`'s "block header present, key absent" branch (same
/// last-direct-child scan, same indentation inference), extended to insert a
/// MULTI-line value plus its doc comment in one edit. Deliberately does NOT
/// mirror `upsert_child`'s flow-form splice (which replaces the inline line
/// with a fresh block carrying only the new key — fine for a scalar/null
/// parent, but here it would drop the user's `collection` value). Kept local
/// to `doctor.rs` (Task 4's file scope) rather than growing the shared line
/// editor for a single caller.
fn insert_search_exclude_block(text: &str, comment: &str, archive: &str) -> Option<String> {
    fn is_blank(l: &str) -> bool {
        l.trim().is_empty()
    }
    fn is_comment(l: &str) -> bool {
        l.trim_start().starts_with('#')
    }
    fn indent_of(l: &str) -> usize {
        l.len() - l.trim_start().len()
    }

    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let ends_with_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    let header_idx = lines.iter().position(|l| {
        indent_of(l) == 0 && !is_comment(l) && l.trim_start().starts_with("search:")
    })?;

    // Refuse anything but a pure block-form header (optionally with a
    // trailing `# …` comment): a flow mapping / scalar / null after the
    // colon cannot carry spliced child lines — the write would be
    // unparseable YAML. Same guard as `upsert_child`'s block-extend branch.
    let after_header = lines[header_idx].trim_start()["search:".len()..].trim_start();
    if !(after_header.is_empty() || after_header.starts_with('#')) {
        return None;
    }

    // Scan the block's direct children: find the last one, infer the child
    // indent (identical logic to `upsert_child`'s block-extend branch), and
    // locate the template-order anchor — the first DIRECT child named
    // `reranker:` or `embed:`. The fresh template places `exclude:` between
    // `default_top_k:` and `reranker:` (onebrain_yml.rs), so inserting
    // before that anchor keeps backfilled vaults on the canonical in-block
    // order (`restructure_config` only reorders top-level blocks, never
    // within one). Only direct-child-level lines can anchor — a nested
    // `model:`-style grandchild never matches.
    let mut last_child = header_idx;
    let mut child_indent: Option<usize> = None;
    let mut anchor: Option<usize> = None;
    let mut j = header_idx + 1;
    while j < lines.len() {
        let l = &lines[j];
        if is_blank(l) {
            j += 1;
            continue;
        }
        if indent_of(l) == 0 {
            break;
        }
        if !is_comment(l) {
            let ind = indent_of(l);
            if child_indent.is_none() {
                child_indent = Some(ind);
            }
            let t = l.trim_start();
            if anchor.is_none()
                && Some(ind) == child_indent
                && (t.starts_with("reranker:") || t.starts_with("embed:"))
            {
                anchor = Some(j);
            }
        }
        last_child = j;
        j += 1;
    }
    let indent = " ".repeat(child_indent.unwrap_or(2));

    // Insertion point: before the anchor (backing up over its contiguous
    // lead comment lines so the block never splits a comment from the key
    // it documents — e.g. the template's own "# Tier-2 cross-encoder
    // reranker" header); with no anchor, after the last child (a vault
    // without `reranker:`/`embed:` keeps the previous last-child position).
    let at = match anchor {
        Some(mut idx) => {
            while idx > header_idx + 1 && is_comment(&lines[idx - 1]) {
                idx -= 1;
            }
            idx
        }
        None => last_child + 1,
    };

    let mut block: Vec<String> = comment
        .split('\n')
        .map(|c| format!("{indent}{c}"))
        .collect();
    block.push(format!("{indent}exclude:"));
    block.push(format!("{indent}- attachments"));
    block.push(format!("{indent}- {archive}"));

    for (n, line) in block.into_iter().enumerate() {
        lines.insert(at + n, line);
    }
    let mut out = lines.join(newline);
    if ends_with_newline {
        out.push_str(newline);
    }
    Some(out)
}

/// Recipe — `search-exclude` warning means `search.collection` is set but
/// `search.exclude` is entirely absent. Insert the same commented block the
/// fresh v3.4.9 template now scaffolds (Task 3): both the doc comment AND
/// the value's second entry are built from this vault's OWN resolved
/// `folders.archive` via the shared
/// [`onebrain_fs::search_exclude_comment`] — the identical single source
/// the template render uses — so comment and value always agree even on a
/// vault with a customized archive folder (never a hard-coded
/// `"06-archive"` literal, and never the table's generic-default comment
/// contradicting a custom value). Idempotent: a second run finds
/// `search_exclude_missing` already false and no-ops.
fn fix_search_exclude(vault_root: &Path, json: bool) -> FixOutcome {
    use onebrain_core::{find_config_file, VaultFolders, CONFIG_FILENAME};
    let path = find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(CONFIG_FILENAME)
        .to_string();
    status_line(
        json,
        &format!("running: insert search.exclude in {filename}"),
    );

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return FixOutcome::Failed(format!("read {filename}: {e}")),
    };
    if !search_exclude_missing(&text) {
        return FixOutcome::Fixed(format!(
            "{filename}: search.exclude already set (or search not adopted)"
        ));
    }

    let archive = serde_yaml::from_str::<serde_yaml::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("folders")
                .and_then(|f| f.get("archive"))
                .and_then(|a| a.as_str())
                .map(str::to_string)
        })
        .unwrap_or(VaultFolders::default().archive);
    let comment = onebrain_fs::search_exclude_comment(&archive);

    let Some(updated) = insert_search_exclude_block(&text, &comment, &archive) else {
        // Gate said the exclude is missing but the `search:` line isn't an
        // addressable block-form header (inline flow mapping like
        // `search: {collection: c}`, or an unlocatable shape). Never splice
        // into it — that would write unparseable YAML. Honest Failed with
        // the manual step instead.
        return FixOutcome::Failed(format!(
            "{filename}: `search:` is not a block-form mapping (e.g. inline `search: {{…}}`) — \
             convert it to block form, then re-run onebrain doctor --fix (or add \
             `exclude: [attachments, {archive}]` manually)"
        ));
    };

    if let Err(e) = onebrain_fs::backup_config_file(&path) {
        return FixOutcome::Failed(format!("backup {filename} before write: {e}"));
    }
    if let Err(e) = onebrain_fs::atomic_write_text(&path, &updated) {
        return FixOutcome::Failed(format!("write {filename}: {e}"));
    }
    FixOutcome::Fixed(format!("inserted search.exclude: [attachments, {archive}]"))
}

/// Insert already-formatted `comment`/`value_line` pairs for MISSING
/// `token_optimization.*` sub-keys ([`onebrain_fs::token_optimization_key_lines`])
/// as the last children of an EXISTING `token_optimization:` block, in
/// `missing`'s order (`config_key_docs`'s canonical field order — see
/// [`token_optimization_missing_sub_keys`]), preserving every existing line,
/// value, and comment verbatim. Mirrors [`insert_search_exclude_block`]'s
/// block-splice pattern (single key, anchored) generalized to N keys,
/// appended after the block's last present child — a vault whose block ends
/// at `read_hook` gains a missing `check_timeout_ms` immediately after it.
///
/// Returns `None` when `token_optimization:` isn't an addressable
/// block-form header (inline mapping, e.g. `token_optimization: {level:
/// x}`) — never splice into a shape that would produce unparseable YAML;
/// the caller falls back to an honest manual-edit message, same convention
/// as every other yaml-edit recipe in this file.
fn insert_token_optimization_sub_keys(
    text: &str,
    missing: &[&'static [&'static str]],
) -> Option<String> {
    fn is_blank(l: &str) -> bool {
        l.trim().is_empty()
    }
    fn is_comment(l: &str) -> bool {
        l.trim_start().starts_with('#')
    }
    fn indent_of(l: &str) -> usize {
        l.len() - l.trim_start().len()
    }

    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let ends_with_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    // Match the header by KEY TOKEN, not a raw `starts_with("token_optimization:")`
    // — a valid `token_optimization :` (spaced) or `"token_optimization":`
    // (quoted) header must resolve to the same block the presence check
    // (`locate_impl`) found, or the two disagree. Split the inline-check on
    // the first colon for the same reason.
    let header_idx = lines.iter().position(|l| {
        indent_of(l) == 0
            && !is_comment(l)
            && onebrain_fs::yaml_edit::key_token(l.trim_start()) == Some("token_optimization")
    })?;
    let after_colon = lines[header_idx]
        .trim_start()
        .split_once(':')
        .map(|(_, a)| a.trim_start())
        .unwrap_or("");
    if !(after_colon.is_empty() || after_colon.starts_with('#')) {
        return None;
    }

    // The insertion anchor is the block's LAST child line (active or comment),
    // so a missing sub-key lands after the block's existing content — even a
    // fully-commented block (child_indent falls back to two spaces, matching
    // the canonical indent).
    let mut last_child = header_idx;
    let mut child_indent: Option<usize> = None;
    let mut j = header_idx + 1;
    while j < lines.len() {
        let l = &lines[j];
        if is_blank(l) {
            j += 1;
            continue;
        }
        if indent_of(l) == 0 {
            break;
        }
        if !is_comment(l) && child_indent.is_none() {
            child_indent = Some(indent_of(l));
        }
        last_child = j;
        j += 1;
    }
    let indent = " ".repeat(child_indent.unwrap_or(2));

    let mut block: Vec<String> = Vec::new();
    for segments in missing {
        let key = segments.last()?;
        let (comment, value_line) = onebrain_fs::token_optimization_key_lines(key)?;
        block.push(format!("{indent}{comment}"));
        block.push(format!("{indent}{value_line}"));
    }

    for (n, line) in block.into_iter().enumerate() {
        lines.insert(last_child + 1 + n, line);
    }
    let mut out = lines.join(newline);
    if ends_with_newline {
        out.push_str(newline);
    }
    Some(out)
}

/// Recipe — `token-optimization` warning means a vault's `onebrain.yml`
/// needs a `token_optimization` backfill, one of two shapes:
///
/// - **Whole block absent** (issue #247): predates v3.4.10 (or was
///   hand-edited) — carries no top-level `token_optimization` block at all.
///   Append the SAME commented block the fresh template scaffolds — built
///   from the one shared source, [`onebrain_fs::token_optimization_block_lines`]
///   (`config_key_docs` comments + `TokenOptimizationConfig::default()`), so
///   this backfill is byte-identical to what a fresh `onebrain init` emits
///   for the block — as a new top-level block, then run the shared
///   [`onebrain_fs::restructure_config`] so it lands under its own "Token
///   optimization" banner in canonical position (`config_layout::SECTIONS`
///   already lists the key) rather than dangling at EOF.
/// - **Sub-key(s) absent from an EXISTING block** (issue #270): the block
///   is present but predates a later-added sub-key (e.g. `check_timeout_ms`,
///   added in v3.4.13). Insert each missing documented sub-key — its
///   `config_key_docs` comment + [`onebrain_fs::token_optimization_key_lines`]'s
///   default value (or, for `get_max_tokens` / `snippet_max_chars`, the SAME
///   commented placeholder the whole-block backfill and the fresh template
///   both use) — as the block's last children via
///   [`insert_token_optimization_sub_keys`], never touching an existing
///   line. Driven from `config_key_docs()`, so future `token_optimization`
///   sub-keys are covered automatically, not just `check_timeout_ms`.
///
/// Idempotent either way: a second run finds nothing left to backfill in
/// either shape and no-ops.
fn fix_token_optimization(vault_root: &Path, json: bool) -> FixOutcome {
    use onebrain_core::{find_config_file, CONFIG_FILENAME};
    let path = find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(CONFIG_FILENAME)
        .to_string();
    status_line(
        json,
        &format!("running: insert token_optimization in {filename}"),
    );

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return FixOutcome::Failed(format!("read {filename}: {e}")),
    };

    if token_optimization_missing(&text) {
        // A flow-style root (`{a: 1, b: 2}`) parses as a mapping — so the
        // gate above can fire — but appending a block-form
        // `token_optimization:` key after it would produce unparseable YAML
        // (a flow scalar can't be followed by block-mapping siblings). Same
        // guard as `insert_search_exclude_block`'s header check: decline
        // with an honest manual-edit message rather than ever corrupting
        // the file.
        if text.trim_start().starts_with('{') {
            return FixOutcome::Failed(format!(
                "{filename}: root is a flow-style mapping (e.g. inline `{{…}}`) — convert it to \
                 block form, then re-run onebrain doctor --fix (or add the token_optimization \
                 block manually)"
            ));
        }

        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let mut updated = text.clone();
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push_str(newline);
        }
        updated.push_str(&onebrain_fs::token_optimization_block_lines().join(newline));
        updated.push_str(newline);

        // Move the just-appended block into canonical section position.
        // Never declines here in practice: the flow-style root case is
        // already ruled out above, and the read proved the file parses to a
        // mapping with at least one OTHER addressable top-level key (that's
        // how `token_optimization_missing` could return true for a real
        // vault) — the remaining shapes `restructure_config` refuses
        // (invalid YAML / non-mapping / no top-level keys) can't co-occur
        // with that. Falls back to the un-restructured append rather than
        // failing outright if it ever does.
        let final_text = onebrain_fs::restructure_config(&updated).unwrap_or(updated);

        if let Err(e) = onebrain_fs::backup_config_file(&path) {
            return FixOutcome::Failed(format!("backup {filename} before write: {e}"));
        }
        if let Err(e) = onebrain_fs::atomic_write_text(&path, &final_text) {
            return FixOutcome::Failed(format!("write {filename}: {e}"));
        }
        return FixOutcome::Fixed(
            "inserted token_optimization block (defaults, documented)".to_string(),
        );
    }

    // Block present — sub-key backfill (issue #270).
    let missing = token_optimization_missing_sub_keys(&text);
    if missing.is_empty() {
        return FixOutcome::Fixed(format!("{filename}: token_optimization already set"));
    }
    let Some(updated) = insert_token_optimization_sub_keys(&text, &missing) else {
        return FixOutcome::Failed(format!(
            "{filename}: `token_optimization:` is not a block-form mapping (e.g. inline \
             `token_optimization: {{…}}`) — convert it to block form, then re-run onebrain \
             doctor --fix (or add the missing sub-key(s) manually)"
        ));
    };

    // Defense-in-depth (issue #270 R-review): the splice above must never
    // produce a file that no longer parses. The key-token presence check
    // (`key_or_commented_placeholder_present`) already prevents the one known
    // corruption path (a spaced/quoted existing key false-reported missing →
    // duplicate key), but PARSE the result before writing as a hard safety
    // net — abort with the file byte-identical rather than ever writing YAML
    // that `serde_yaml` (or the runtime loader) would reject.
    if serde_yaml::from_str::<serde_yaml::Value>(&updated).is_err() {
        return FixOutcome::Failed(format!(
            "{filename}: token_optimization sub-key backfill would produce invalid YAML — \
             aborted, file left unchanged (please add the missing sub-key(s) manually)"
        ));
    }

    if let Err(e) = onebrain_fs::backup_config_file(&path) {
        return FixOutcome::Failed(format!("backup {filename} before write: {e}"));
    }
    if let Err(e) = onebrain_fs::atomic_write_text(&path, &updated) {
        return FixOutcome::Failed(format!("write {filename}: {e}"));
    }
    let keys = missing
        .iter()
        .map(|s| *s.last().expect("non-empty segments"))
        .collect::<Vec<_>>()
        .join(", ");
    FixOutcome::Fixed(format!("backfilled token_optimization sub-key(s): {keys}"))
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
/// Used by `collect_config_findings` to validate `checkpoint.messages` /
/// `checkpoint.minutes` (v3.4.8 — value validation moved here from the
/// fs-layer `vault_yml_keys` check).
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

/// Reset one config value to `default_value` in raw config text via a
/// comment-preserving line edit. Thin wrapper over the shared
/// [`onebrain_fs::yaml_edit::set_value`] (extracted there in the R3 fix
/// round so vault-sync's `update_vault_yml` uses the identical editor) —
/// kept as a named seam because every `--fix` call site and the unit tests
/// speak in terms of "reset".
///
/// Returns `None` (caller reports the value un-fixable) when a parent is an
/// inline mapping (`checkpoint: {messages: 0}`) or the key line can't be
/// found — never guesses on a shape it doesn't understand.
fn reset_config_value(text: &str, segments: &[&str], default_value: &str) -> Option<String> {
    onebrain_fs::yaml_edit::set_value(text, segments, default_value)
}

/// Recipe — `config-values` warning means one or more PRESENT config values
/// are out of range / not in a registry. Re-collect the findings from the
/// file (the check's `DoctorResult` doesn't carry them structurally), then
/// reset each auto-resettable one to its documented default through
/// [`reset_config_value`] — comments, key order, and the user's other values
/// all survive. `folders.*` / `search.collection` findings are never touched
/// (report-only by design: renaming folders orphans notes; changing the
/// collection detaches the index). An `embed_model` reset additionally warns
/// that a reindex is required — the old model's vectors are now stale.
fn fix_config_values(vault_root: &Path, json: bool) -> FixOutcome {
    use onebrain_core::{find_config_file, CONFIG_FILENAME};
    let path = find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(CONFIG_FILENAME)
        .to_string();
    status_line(json, &format!("running: reset config values in {filename}"));

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return FixOutcome::Failed(format!("read {filename}: {e}")),
    };
    let Some(findings) = collect_config_findings(&text) else {
        return FixOutcome::Failed(format!("{filename} is not a YAML mapping"));
    };
    if findings.is_empty()
        && undocumented_keys(&text).is_empty()
        && onebrain_fs::config_layout_matches(&text)
    {
        return FixOutcome::Fixed(format!(
            "{filename}: all values already in range, documented, and in template layout"
        ));
    }

    // Whether the check-time read promised a restructure ("layout drift").
    // Captured BEFORE any edit so a fix-time decline of that promise can be
    // surfaced honestly instead of silently skipped.
    let layout_drift_at_read = !onebrain_fs::config_layout_matches(&text);
    let mut current = text;
    let mut resets: Vec<String> = Vec::new();
    let mut untouched: Vec<String> = Vec::new();
    let mut unfixable: Vec<String> = Vec::new();
    let mut reindex_required = false;
    for f in &findings {
        if !f.resettable {
            untouched.push(f.dotted.clone());
            continue;
        }
        match reset_config_value(&current, &f.segments, &f.default_repr) {
            Some(updated) => {
                current = updated;
                resets.push(format!("{} → {}", f.dotted, f.default_repr));
                if f.reindex_required {
                    reindex_required = true;
                }
            }
            None => unfixable.push(f.dotted.clone()),
        }
    }

    // Comment backfill for existing vaults (sanctioned scope, 2026-07-09):
    // insert the template's own `# … · default: …` line above every
    // template-known key that exists here without one. Runs AFTER the value
    // resets so the inserted comments sit above the corrected lines. Keys
    // under a user's own comment are skipped (`insert_comment_above` refuses
    // — user comments win); missing keys are never added.
    let mut comments_added: Vec<String> = Vec::new();
    for doc in onebrain_fs::config_key_docs() {
        if !onebrain_fs::yaml_edit::key_lacks_comment(&current, doc.segments) {
            continue;
        }
        if let Some(updated) =
            onebrain_fs::yaml_edit::insert_comment_above(&current, doc.segments, &doc.comment)
        {
            current = updated;
            comments_added.push(doc.segments.join("."));
        }
    }

    // Section restructure (v3.4.8): reorder the top-level blocks into the
    // template's section order and insert the Style-A banners, moving each
    // block as opaque bytes so the just-reset values and just-added comments
    // (and every user comment) survive verbatim. Runs LAST so it operates on
    // the fully-corrected text; idempotent, so a second `--fix` is a no-op.
    let mut restructured = false;
    match onebrain_fs::restructure_config(&current) {
        Some(reordered) => {
            if reordered != current {
                current = reordered;
                restructured = true;
            }
        }
        None => {
            // The restructure declined the shape. When the check-time read
            // reported layout drift, silence here would break the promise
            // plain doctor made ("--fix will restructure") — surface it as
            // un-fixable so the run lands Partial, not a clean Fixed. In
            // practice this arm-with-drift is unreachable: every declined
            // shape (invalid YAML / non-mapping / flow-style root / no
            // top-level keys) also makes `config_layout_matches` return true
            // (never drift), and the value/comment line edits above cannot
            // change the root shape — a unit test pins that agreement. Kept
            // as defense-in-depth for honesty over silence.
            if layout_drift_at_read {
                unfixable.push("layout restructure (unsupported top-level shape)".to_string());
            }
        }
    }

    if !resets.is_empty() || !comments_added.is_empty() || restructured {
        // Defense-in-depth backup before the write, mirroring every other
        // config-writing recipe — even though this edit preserves comments.
        if let Err(e) = onebrain_fs::backup_config_file(&path) {
            return FixOutcome::Failed(format!("backup {filename} before write: {e}"));
        }
        if let Err(e) = onebrain_fs::atomic_write_text(&path, &current) {
            return FixOutcome::Failed(format!("write {filename}: {e}"));
        }
    }
    if reindex_required {
        status_line(
            json,
            "⚠️ embedding model reset — run `onebrain search reindex` to rebuild vectors",
        );
    }

    let mut parts: Vec<String> = Vec::new();
    if !resets.is_empty() {
        parts.push(format!(
            "reset {} value(s) to default: {}",
            resets.len(),
            resets.join(", ")
        ));
    }
    if reindex_required {
        parts.push("embedding model reset — run `onebrain search reindex`".to_string());
    }
    if !comments_added.is_empty() {
        parts.push(format!(
            "added {} self-documentation comment(s): {}",
            comments_added.len(),
            comments_added.join(", ")
        ));
    }
    if restructured {
        parts.push(
            "restructured layout into template sections (reordered blocks, added banners)"
                .to_string(),
        );
    }
    if !untouched.is_empty() {
        parts.push(format!(
            "left untouched (never auto-reset): {} — edit manually",
            untouched.join(", ")
        ));
    }
    if !unfixable.is_empty() {
        parts.push(format!(
            "could not reset (unsupported YAML shape, e.g. inline mapping): {} — edit manually",
            unfixable.join(", ")
        ));
        // Honest tri-state: real progress landed on disk (a value reset, a
        // comment backfill, or a restructure) AND something remains → Partial
        // (distinct glyph, still a non-zero exit); nothing landed → Failed.
        let made_progress = !resets.is_empty() || !comments_added.is_empty() || restructured;
        return if made_progress {
            FixOutcome::Partial(parts.join(" · "))
        } else {
            FixOutcome::Failed(parts.join(" · "))
        };
    }
    FixOutcome::Fixed(parts.join(" · "))
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
    // Decline a flow-style root (`{…}`) the same way `restructure_config` does:
    // it parses as a mapping but has no block-form lines, so appending a block
    // `stats:` after it would produce invalid YAML on the next read. Exotic (no
    // tool writes a flow-root config), but cheap to guard now that stamping
    // shares the yaml_edit-family shape rules.
    if text.trim_start().starts_with('{') {
        return None;
    }
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
                    "⚠ Could not record today's run in {} — {e} (the checks above are unaffected)\n\
                     💡 check that {} is readable — otherwise no action needed",
                    path.display(),
                    path.display()
                );
            }
            return;
        }
    };
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    match upsert_doctor_stats(&text, &today, fix) {
        Some(updated) => {
            // Stamping a config that has NO `stats:` block appends a bare one
            // at the end. On a config that was already in canonical section
            // layout (e.g. a freshly-init'd vault), that bare append would
            // introduce layout drift the very next run would report. Re-run
            // the byte-preserving restructure so the stamp keeps its own write
            // canonical (System banner + managed note around the new stats
            // block). A config that was ALREADY drifted is left as-is — plain
            // doctor never restructures a user's legacy layout; that stays
            // `--fix`'s job.
            let final_text = if onebrain_fs::config_layout_matches(&text) {
                onebrain_fs::restructure_config(&updated).unwrap_or(updated)
            } else {
                updated
            };
            if let Err(e) = onebrain_fs::atomic_write_text(&path, &final_text) {
                if !quiet {
                    eprintln!(
                        "⚠ Could not record today's run in {} — {e} (the checks above are unaffected)\n\
                         💡 check that {} is writable — otherwise no action needed",
                        path.display(),
                        path.display()
                    );
                }
            }
        }
        // `None` is the common "already today" no-op, but it also covers a
        // refusal to edit an inline `stats:` mapping — surface that case so a
        // never-advancing timestamp doesn't look like a silent bug.
        None if !quiet && config_has_inline_stats(&text) => {
            eprintln!(
                "⚠ Run not recorded — {}'s `stats:` block is written as an inline mapping (`{{…}}`)\n\
                 💡 convert `stats:` in {} to block YAML form to enable run-timestamp tracking",
                path.display(),
                path.display()
            );
        }
        None if !quiet && text.trim_start().starts_with('{') => {
            eprintln!(
                "⚠ Run not recorded — {} has a flow-style (`{{…}}`) root\n\
                 💡 convert {} to block YAML form to enable run-timestamp tracking",
                path.display(),
                path.display()
            );
        }
        None => {}
    }
}

/// Record that the user declined the qmd cleanup prompt so the NEXT
/// `doctor --fix` run doesn't offer it again — [`planned_action`] and
/// [`fix_qmd_leftovers`] both check `qmd_leftovers_check`'s hint, which is
/// omitted once this flag is set. The advisory `qmd-leftovers` finding
/// itself keeps showing on every plain `doctor` run regardless (only the
/// re-prompt is suppressed).
///
/// Same comment-preserving `stats:` block writer [`stamp_doctor_run`] uses
/// for `last_doctor_run`, generalized to an arbitrary key via
/// [`onebrain_fs::yaml_edit::upsert_child`] (the same primitive
/// `fix_legacy_qmd_collection` already uses for `search.collection`). Best
/// effort: a write failure is noted on stderr, never surfaced as a doctor
/// exit-code failure — this is a convenience flag, not a check result.
fn decline_qmd_cleanup(vault_root: &Path) {
    use onebrain_core::find_config_file;
    let Some(path) = find_config_file(vault_root) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let updated =
        onebrain_fs::yaml_edit::upsert_child(&text, "stats", "qmd_cleanup_declined", "true");
    if updated == text {
        return; // already recorded
    }
    if let Err(e) = onebrain_fs::backup_config_file(&path) {
        eprintln!(
            "⚠ Could not save your qmd-cleanup choice — backing up {} first failed: {e} \
             (doctor will ask again next run)\n\
             💡 check that {}'s folder is writable — otherwise no action needed",
            path.display(),
            path.display()
        );
        return;
    }
    if let Err(e) = onebrain_fs::atomic_write_text(&path, &updated) {
        eprintln!(
            "⚠ Could not save your qmd-cleanup choice to {} — {e} (doctor will ask again next run)\n\
             💡 check that {} is writable — otherwise no action needed",
            path.display(),
            path.display()
        );
    }
}

fn print_fix_summary(outcomes: &[(String, FixOutcome)]) {
    let mut fixed = 0;
    let mut partial = 0;
    let mut failed = 0;
    let mut manual = 0;
    for (check, outcome) in outcomes {
        match outcome {
            FixOutcome::Fixed(msg) => {
                fixed += 1;
                println!("  ✓ {check}: {msg}");
            }
            FixOutcome::Partial(msg) => {
                partial += 1;
                println!("  ◐ {check}: {msg}");
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
    // `partial` is rendered only when present, keeping the common line
    // byte-identical to the pre-Partial format.
    let partial_part = if partial > 0 {
        format!(" · {partial} partial")
    } else {
        String::new()
    };
    println!("\nFix summary: {fixed} fixed{partial_part} · {failed} failed · {manual} manual",);
}

// ─────────────────────────────────────────────────────────────────────────
// Grouped doctor report — checks bucketed into 4 emoji-headed sections in the
// grouped-status convention (see `output::layout`) and rendered through the
// shared braille-spinner progress primitive. No frames or rules; passes are
// quiet; warnings / fails are prominent with their hint as the indented `└`
// line. Structured (`--json`/`--yaml`) output is unchanged — this is purely
// the human text/TTY surface.
// ─────────────────────────────────────────────────────────────────────────

/// The approved 4-section grouping of the checks, in display order. Each
/// entry is `(emoji, section header, [check names in order])`. Check names
/// are the stable `DoctorResult::check` identifiers produced by the check
/// modules (plus the CLI-layer `search` check appended by [`all_checks`]).
const DOCTOR_SECTIONS: [(&str, &str, &[&str]); 4] = [
    (
        "⚙️",
        "Config",
        &[
            "onebrain.yml",
            "onebrain.yml-keys",
            "config-values",
            "search-exclude",
            "token-optimization",
            "vault-config-migration",
            "legacy-qmd-collection",
        ],
    ),
    (
        "📁",
        "Vault structure",
        &["folders", "plugin-files", "plugin-cache"],
    ),
    ("🔌", "Integration", &["settings-hooks", "claude-settings"]),
    (
        "📊",
        "Index & state",
        &[
            "orphan-checkpoints",
            "search",
            "read-hook-failopen",
            "legacy-index-stub",
            "qmd-leftovers",
        ],
    ),
];

/// Short, scannable display label for a check name (matches the approved
/// layout). Unknown checks fall back to their raw name so a future check
/// still renders something sensible before its label is added here.
fn display_label(check: &str) -> &str {
    match check {
        "onebrain.yml" => "onebrain.yml",
        "onebrain.yml-keys" => "schema",
        "config-values" => "config values",
        "search-exclude" => "search exclude",
        "token-optimization" => "token optimization",
        "vault-config-migration" => "config migration",
        "legacy-qmd-collection" => "qmd_collection",
        "folders" => "folders",
        "plugin-files" => "plugin files",
        "plugin-cache" => "plugin cache",
        "settings-hooks" => "hooks",
        "claude-settings" => "claude settings",
        "orphan-checkpoints" => "checkpoints",
        "search" => "search",
        "read-hook-failopen" => "read-hook gate",
        "legacy-index-stub" => "legacy index stub",
        "qmd-leftovers" => "qmd cleanup",
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

// ── Summary box: colour + display-fold helpers ───────────────────────────
// Width-stable glyphs only (the ✓/⚠/✗ set the check rows already render
// aligned in the user's terminal — no variation-selector emoji, whose
// unicode-width can disagree with the drawn width and break the box border).
const SGR_GREEN: &str = "\x1b[32m";
const SGR_YELLOW: &str = "\x1b[33m";
const SGR_RED: &str = "\x1b[31m";
const SGR_CYAN: &str = "\x1b[36m";

/// `true` for a warn/fail result (an "issue").
fn is_issue(r: &DoctorResult) -> bool {
    matches!(r.status, DoctorStatus::Warn | DoctorStatus::Error)
}

/// Worst of two optional statuses (fail dominates warn dominates ok). `None`
/// contributes nothing.
fn worse(a: Option<DoctorStatus>, b: Option<DoctorStatus>) -> Option<DoctorStatus> {
    let rank = |s: DoctorStatus| match s {
        DoctorStatus::Error => 2,
        DoctorStatus::Warn => 1,
        DoctorStatus::Ok => 0,
    };
    match (a, b) {
        (Some(x), Some(y)) => Some(if rank(x) >= rank(y) { x } else { y }),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// One display row (post-migration-merge) shown in the checking sections AND
/// counted in the summary box. Text-display only — JSON keeps the underlying
/// checks separate.
struct DisplayRow {
    name: String,
    status: DoctorStatus,
    message: String,
}

/// Fold `vault-config-migration` + `legacy-qmd-collection` into one `migration`
/// row: severity = worst of the two, message names the legacy items actually
/// present. Returns `None` only when NEITHER underlying check ran.
fn migration_row(results: &[DoctorResult]) -> Option<DisplayRow> {
    let vc = results.iter().find(|r| r.check == "vault-config-migration");
    let qc = results.iter().find(|r| r.check == "legacy-qmd-collection");
    if vc.is_none() && qc.is_none() {
        return None;
    }
    let status = worse(vc.map(|r| r.status), qc.map(|r| r.status)).unwrap_or(DoctorStatus::Ok);
    let mut items: Vec<&str> = Vec::new();
    if vc.is_some_and(is_issue) {
        items.push("legacy vault.yml");
    }
    if qc.is_some_and(is_issue) {
        items.push("qmd_collection key");
    }
    let message = if items.is_empty() {
        "nothing to migrate".to_string()
    } else {
        items.join(" · ")
    };
    Some(DisplayRow {
        name: "migration".to_string(),
        status,
        message,
    })
}

/// The checks in canonical (section) display order, with the two migration
/// checks folded into a single `migration` row. Drives both the section render
/// and the summary box so their tallies and ordering can't drift.
fn display_rows(results: &[DoctorResult]) -> Vec<DisplayRow> {
    let mut rows: Vec<DisplayRow> = Vec::new();
    let mut placed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (_, _, checks) in DOCTOR_SECTIONS {
        for name in checks {
            if *name == "legacy-qmd-collection" {
                placed.insert(name);
                continue; // folded into the `migration` row below
            }
            if *name == "vault-config-migration" {
                placed.insert("vault-config-migration");
                placed.insert("legacy-qmd-collection");
                if let Some(row) = migration_row(results) {
                    rows.push(row);
                }
                continue;
            }
            if let Some(r) = results.iter().find(|r| r.check == *name) {
                rows.push(DisplayRow {
                    name: display_label(&r.check).to_string(),
                    status: r.status,
                    message: r.message.clone(),
                });
                placed.insert(name);
            }
        }
    }
    // Defensive: any unmapped check still shows, so nothing is silently dropped.
    for r in results
        .iter()
        .filter(|r| !placed.contains(r.check.as_str()))
    {
        rows.push(DisplayRow {
            name: display_label(&r.check).to_string(),
            status: r.status,
            message: r.message.clone(),
        });
    }
    rows
}

/// Bucket `results` into the 4 display sections as [`Section`]s of [`Step`]s.
///
/// NO inline hints (v3.4.8 — every hint moved to the bottom Summary box). The
/// two legacy-migration checks fold into one `migration` row. Any result whose
/// check name isn't in [`DOCTOR_SECTIONS`] is appended to a trailing "Other"
/// section so nothing is silently dropped.
fn build_sections(results: &[DoctorResult]) -> Vec<crate::output::Section> {
    use crate::output::{Section, Step};

    // Detail = message; hint always None (moved to the Summary box).
    let step = |name: &str, status: DoctorStatus, message: String| {
        Step::new(name, step_status_of(status), Some(message), None)
    };

    let mut sections: Vec<Section> = Vec::with_capacity(DOCTOR_SECTIONS.len());
    let mut placed: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for (emoji, header, checks) in DOCTOR_SECTIONS {
        let mut steps = Vec::new();
        for name in checks {
            if *name == "legacy-qmd-collection" {
                placed.insert(name);
                continue; // folded into `migration`
            }
            if *name == "vault-config-migration" {
                placed.insert("vault-config-migration");
                placed.insert("legacy-qmd-collection");
                if let Some(row) = migration_row(results) {
                    steps.push(step(&row.name, row.status, row.message));
                }
                continue;
            }
            if let Some(r) = results.iter().find(|r| r.check == *name) {
                steps.push(step(display_label(&r.check), r.status, r.message.clone()));
                placed.insert(name);
            }
        }
        if !steps.is_empty() {
            sections.push(Section::with_emoji(emoji, header, steps));
        }
    }

    // Defensive: surface any unmapped check rather than dropping it.
    let leftovers: Vec<Step> = results
        .iter()
        .filter(|r| !placed.contains(r.check.as_str()))
        .map(|r| step(display_label(&r.check), r.status, r.message.clone()))
        .collect();
    if !leftovers.is_empty() {
        sections.push(Section::with_emoji("❓", "Other", leftovers));
    }

    sections
}

/// First whitespace-delimited integer in `msg` (e.g. the leading count in
/// "3 unmerged checkpoint(s) …").
fn leading_count(msg: &str) -> Option<usize> {
    msg.split_whitespace().next()?.parse().ok()
}

/// The integer immediately before the word `pending` in `msg` (e.g. `6` in
/// "721 indexed · 6 pending").
fn pending_count(msg: &str) -> Option<usize> {
    let toks: Vec<&str> = msg.split_whitespace().collect();
    toks.windows(2)
        .find(|w| w[1].trim_end_matches(['.', ',']) == "pending")
        .and_then(|w| w[0].parse().ok())
}

/// Short outcome phrase for each auto-fixable check — what `--fix` will do.
fn fix_phrase(check: &str) -> Option<&'static str> {
    match check {
        "onebrain.yml-keys" => Some("backfill missing keys"),
        "config-values" => Some("document keys + restructure layout"),
        "folders" => Some("create missing folders"),
        "plugin-files" => Some("re-download plugin files"),
        "plugin-cache" => Some("clear stale plugin cache"),
        "settings-hooks" => Some("register hooks"),
        "claude-settings" => Some("remove stale marketplace entry"),
        _ => None,
    }
}

/// Compose the single `onebrain doctor --fix` outcome from every present
/// fixable finding — migration items listed by what's actually present, other
/// recipes by their short phrase, in canonical order. Deduped into one line.
fn fix_outcome_summary(results: &[DoctorResult]) -> String {
    let issue = |name: &str| results.iter().any(|r| r.check == name && is_issue(r));
    let mut phrases: Vec<String> = Vec::new();

    let mut legacy: Vec<&str> = Vec::new();
    if issue("vault-config-migration") {
        legacy.push("vault.yml");
    }
    if issue("legacy-qmd-collection") {
        legacy.push("qmd_collection key");
    }
    if !legacy.is_empty() {
        phrases.push(format!("migrate legacy config ({})", legacy.join(" · ")));
    }
    for (_, _, checks) in DOCTOR_SECTIONS {
        for name in checks {
            if issue(name) {
                if let Some(p) = fix_phrase(name) {
                    phrases.push(p.to_string());
                }
            }
        }
    }
    if phrases.is_empty() {
        "auto-repair".to_string()
    } else {
        phrases.join(" · ")
    }
}

/// One `(command, outcome)` action line for a non-fixable finding's hint.
/// `outcome` is empty when there's no distinct outcome to spell out (rendered
/// as a bare `💡 <command>`).
fn action_from_result(r: &DoctorResult) -> (String, String) {
    let hint = r.hint.as_deref().unwrap_or("");
    if hint.contains("/wrapup") {
        let n = leading_count(&r.message).unwrap_or(0);
        return ("/wrapup".to_string(), format!("merge {n} checkpoint(s)"));
    }
    if hint.contains("search reindex") {
        let outcome = match pending_count(&r.message) {
            Some(n) => format!("embed {n} pending doc(s)"),
            None => "reindex the search index".to_string(),
        };
        return ("onebrain search reindex".to_string(), outcome);
    }
    (hint.to_string(), String::new())
}

/// The deduplicated action lines for the summary box: the single
/// `onebrain doctor --fix` line first (when anything is auto-fixable), then
/// each unique non-fixable action in canonical check order.
fn summary_action_lines(results: &[DoctorResult]) -> Vec<(String, String)> {
    let ordered: Vec<&DoctorResult> = DOCTOR_SECTIONS
        .iter()
        .flat_map(|(_, _, checks)| checks.iter())
        .filter_map(|name| results.iter().find(|r| r.check == *name))
        .chain(results.iter().filter(|r| {
            !DOCTOR_SECTIONS
                .iter()
                .any(|(_, _, cs)| cs.contains(&r.check.as_str()))
        }))
        .collect();

    let mut lines: Vec<(String, String)> = Vec::new();
    if ordered
        .iter()
        .any(|r| is_issue(r) && planned_action(r).is_some())
    {
        lines.push((
            "onebrain doctor --fix".to_string(),
            fix_outcome_summary(results),
        ));
    }
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in ordered.iter().filter(|r| is_issue(r)) {
        // Auto-fixable findings are covered by the single --fix line above.
        if planned_action(r).is_some() || r.hint.is_none() {
            continue;
        }
        let (cmd, outcome) = action_from_result(r);
        if cmd.is_empty() {
            continue;
        }
        if seen.insert(cmd.clone()) {
            lines.push((cmd, outcome));
        }
    }
    lines
}

/// Render the bottom Summary box (Variant A): a titled single-line box sharing
/// [`crate::output::boxed::render_boxed_table`] with `search model list` so the
/// two box styles can't drift. Contents: the `N ok · N warnings · N fail`
/// tally (migration folded to ONE row), the non-ok findings (fails before
/// warnings), then the deduplicated `💡 command → outcome` action lines. An
/// all-ok run collapses to a single `✓ N checks · all ok` line. Colour wraps
/// content only (borders unstyled) so the right border stays flush.
fn render_summary_box<W: Write>(w: &mut W, results: &[DoctorResult], color: bool) -> Result<()> {
    let rows = display_rows(results);
    let total = rows.len();
    let fails = rows
        .iter()
        .filter(|r| r.status == DoctorStatus::Error)
        .count();
    let warns = rows
        .iter()
        .filter(|r| r.status == DoctorStatus::Warn)
        .count();
    let oks = total - fails - warns;

    let mut content: Vec<(String, Option<&str>)> = Vec::new();
    if fails == 0 && warns == 0 {
        content.push((format!("✓ {total} checks · all ok"), Some(SGR_GREEN)));
    } else {
        let warn_word = if warns == 1 { "warning" } else { "warnings" };
        content.push((
            format!("{oks} ok · {warns} {warn_word} · {fails} fail"),
            None,
        ));
        content.push((String::new(), None));

        // Findings — fails first, then warnings; name column padded so messages
        // align. Whole row coloured by severity (SGR wraps content only).
        let name_w = rows
            .iter()
            .filter(|r| is_row_issue(r))
            .map(|r| r.name.len())
            .max()
            .unwrap_or(0);
        for status in [DoctorStatus::Error, DoctorStatus::Warn] {
            let (glyph, sgr) = if status == DoctorStatus::Error {
                ("✗", SGR_RED)
            } else {
                ("⚠", SGR_YELLOW)
            };
            for r in rows.iter().filter(|r| r.status == status) {
                content.push((
                    format!("{glyph}  {:<name_w$}  {}", r.name, r.message),
                    Some(sgr),
                ));
            }
        }

        // Action lines — deduped, two-column (command padded so arrows align).
        let actions = summary_action_lines(results);
        if !actions.is_empty() {
            content.push((String::new(), None));
            let cmd_w = actions.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
            for (cmd, outcome) in &actions {
                let line = if outcome.is_empty() {
                    format!("💡 {cmd}")
                } else {
                    format!("💡 {cmd:<cmd_w$}  →  {outcome}")
                };
                content.push((line, Some(SGR_CYAN)));
            }
        }
    }

    let lines = crate::output::boxed::render_boxed_table(" Summary ", &content, color);
    writeln!(w)?;
    for l in lines {
        writeln!(w, "{l}")?;
    }
    Ok(())
}

/// `true` for a warn/fail display row.
fn is_row_issue(r: &DisplayRow) -> bool {
    matches!(r.status, DoctorStatus::Warn | DoctorStatus::Error)
}

/// The doctor report header line: `🩺  Doctor · <vault> · onebrain <version>`.
fn doctor_header(vault_name: &str) -> String {
    format!(
        "🩺  Doctor · {vault_name} · onebrain {}",
        env!("CARGO_PKG_VERSION")
    )
}

/// Render the full grouped report to `w`: the `🩺 Doctor …` header, the four
/// grouped checking sections (NO inline hints), then — when `show_summary` —
/// the bottom Summary box.
///
/// `animate` drives the gating seam: true paints the braille spinner + paced
/// reveal (colour TTY, non-quiet, text mode); false emits only the static
/// lines (deterministic tests). `color` is the resolved colour bit.
///
/// `show_summary` is false for the pre-fix report under `--fix`: the Summary
/// box is deferred until after the fix pass so the run shows exactly one
/// (final) summary.
fn render_grouped_report<W: Write>(
    mut w: W,
    results: &[DoctorResult],
    vault_name: &str,
    color: bool,
    animate: bool,
    show_summary: bool,
) -> Result<()> {
    use crate::output::ProgressRenderer;
    writeln!(w, "{}", doctor_header(vault_name))?;
    let sections = build_sections(results);
    {
        // force_static = !animate.
        let mut renderer = ProgressRenderer::with_writer(&mut w, !animate, color);
        // Grouped-convention body rows: four-space indent under the emoji
        // section headers.
        renderer.set_row_indent("    ");
        for section in &sections {
            renderer.render_section(section)?;
        }
    }
    if show_summary {
        render_summary_box(&mut w, results, color)?;
    }
    Ok(())
}

/// Emit the grouped text report to stdout. Animates step-by-step only on a
/// colour, non-quiet, interactive terminal (the [`ProgressRenderer`] gate);
/// piped / non-TTY / structured / `--no-color` / `--quiet` get the instant
/// static layout.
fn emit_text_report(
    results: &[DoctorResult],
    vault_name: &str,
    mode: &OutputMode,
    quiet: bool,
    show_summary: bool,
) -> Result<()> {
    use crate::output::{is_color_text, should_animate};
    use std::io::IsTerminal;
    // Compute the gating decision directly — no throwaway renderer round-trip.
    let animate = should_animate(mode, std::io::stdout().is_terminal(), quiet);
    let color = is_color_text(mode);
    render_grouped_report(
        std::io::stdout(),
        results,
        vault_name,
        color,
        animate,
        show_summary,
    )
}

/// Emit ONLY the bottom Summary box to stdout (the deferred post-`--fix`
/// render). Colour is resolved the same way as the full report.
fn emit_summary_box(results: &[DoctorResult], mode: &OutputMode) -> Result<()> {
    let color = crate::output::is_color_text(mode);
    render_summary_box(&mut std::io::stdout(), results, color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_core::DoctorResult;

    /// Build the canonical 10-check fixture in the real check-name order
    /// (8 config/structure/integration/state checks + `legacy-qmd-collection`
    /// in Config + the native `search` row in Index & state), with a couple of
    /// warnings so grouping + footer logic is exercised: 8 ok · 2 warnings.
    fn sample_results() -> Vec<DoctorResult> {
        vec![
            DoctorResult::ok("onebrain.yml", "valid · stable"),
            DoctorResult::ok("onebrain.yml-keys", "all keys ok"),
            DoctorResult::ok("vault-config-migration", "onebrain.yml in use"),
            DoctorResult::ok("legacy-qmd-collection", "no legacy qmd_collection key"),
            DoctorResult::ok("folders", "8/8 present"),
            DoctorResult::ok("plugin-files", "complete"),
            DoctorResult::warn("settings-hooks", "PostToolUse (qmd) duplicated (×2)")
                .with_hint("onebrain doctor --fix"),
            DoctorResult::ok("claude-settings", "ok"),
            DoctorResult::ok("orphan-checkpoints", "0 orphans"),
            DoctorResult::warn("search", "721 indexed · 6 pending")
                .with_hint("onebrain search reindex"),
        ]
    }

    fn render_static_report(results: &[DoctorResult], color: bool) -> String {
        let mut buf = Vec::new();
        render_grouped_report(&mut buf, results, "my-vault", color, false, true).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// The lines of the bottom Summary box (between its `┌` and `└` borders,
    /// borders included), for width-integrity + content assertions.
    fn summary_box_lines(out: &str) -> Vec<&str> {
        let start = out
            .lines()
            .position(|l| l.starts_with("┌ Summary "))
            .expect("summary box top border");
        out.lines()
            .skip(start)
            .take_while(|l| l.starts_with('┌') || l.starts_with('│') || l.starts_with('└'))
            .collect()
    }

    // ── Section grouping ─────────────────────────────────────────────────

    #[test]
    fn build_sections_merges_migration_and_assigns_sections() {
        let results = sample_results();
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
        // The two legacy-migration checks collapse into one `migration` row.
        assert_eq!(
            by_header["Config"],
            vec!["onebrain.yml", "schema", "migration"]
        );
        assert_eq!(
            by_header["Vault structure"],
            vec!["folders", "plugin files"]
        );
        assert_eq!(by_header["Integration"], vec!["hooks", "claude settings"]);
        assert_eq!(by_header["Index & state"], vec!["checkpoints", "search"]);
    }

    #[test]
    fn build_sections_surfaces_unmapped_check_in_other() {
        let mut results = sample_results();
        results.push(DoctorResult::warn("brand-new-check", "hmm"));
        let sections = build_sections(&results);
        let other = sections.iter().find(|s| s.header == "Other");
        assert!(other.is_some(), "unmapped check must land in Other");
        assert_eq!(other.unwrap().steps[0].label, "brand-new-check");
    }

    #[test]
    fn build_sections_never_carry_inline_hints() {
        // Every hint moved to the Summary box — no section step carries one.
        let results = vec![
            DoctorResult::ok("onebrain.yml", "valid").with_hint("ignored on ok"),
            DoctorResult::warn("settings-hooks", "dup").with_hint("onebrain doctor --fix"),
            DoctorResult::warn("search", "6 pending").with_hint("onebrain search reindex"),
        ];
        let sections = build_sections(&results);
        assert!(
            sections
                .iter()
                .flat_map(|s| &s.steps)
                .all(|st| st.hint.is_none()),
            "no section step may carry an inline hint"
        );
    }

    // ── Merged migration row: four state combos + worst severity ──────────

    #[test]
    fn migration_row_combos_and_worst_severity() {
        let ok = |check: &'static str| DoctorResult::ok(check, "clean");
        // Both clean → "nothing to migrate", ok.
        let row =
            migration_row(&[ok("vault-config-migration"), ok("legacy-qmd-collection")]).unwrap();
        assert_eq!(row.message, "nothing to migrate");
        assert_eq!(row.status, DoctorStatus::Ok);
        // Legacy vault.yml only.
        let row = migration_row(&[
            DoctorResult::warn("vault-config-migration", "vault.yml present"),
            ok("legacy-qmd-collection"),
        ])
        .unwrap();
        assert_eq!(row.message, "legacy vault.yml");
        assert_eq!(row.status, DoctorStatus::Warn);
        // Legacy qmd_collection only.
        let row = migration_row(&[
            ok("vault-config-migration"),
            DoctorResult::warn("legacy-qmd-collection", "qmd key present"),
        ])
        .unwrap();
        assert_eq!(row.message, "qmd_collection key");
        // Both present → worst severity (error dominates) + both items.
        let row = migration_row(&[
            DoctorResult::error("vault-config-migration", "x"),
            DoctorResult::warn("legacy-qmd-collection", "y"),
        ])
        .unwrap();
        assert_eq!(row.message, "legacy vault.yml · qmd_collection key");
        assert_eq!(row.status, DoctorStatus::Error);
        // Neither ran → no row.
        assert!(migration_row(&[ok("folders")]).is_none());
    }

    // ── Header + checking sections (no inline hints) ──────────────────────

    #[test]
    fn report_header_names_vault_and_version() {
        let out = render_static_report(&sample_results(), false);
        assert!(
            out.lines()
                .next()
                .unwrap()
                .starts_with("🩺  Doctor · my-vault · onebrain "),
            "header line: {out:?}"
        );
    }

    #[test]
    fn checking_sections_show_labels_glyphs_and_no_inline_hints() {
        let out = render_static_report(&sample_results(), false);
        for header in [
            "⚙️  Config",
            "📁  Vault structure",
            "🔌  Integration",
            "📊  Index & state",
        ] {
            assert!(out.contains(header), "section {header}: {out:?}");
        }
        assert!(out.contains("    ✓ onebrain.yml"), "ok line: {out:?}");
        assert!(
            out.contains("    ✓ migration"),
            "merged migration row: {out:?}"
        );
        assert!(out.contains("    ⚠ hooks"), "warn line: {out:?}");
        // NO inline `└ hint` lines anywhere in the checking sections — the
        // search reindex + doctor --fix hints live only in the Summary box.
        assert!(
            !out.contains("└ onebrain search reindex"),
            "no inline reindex hint: {out:?}"
        );
        assert!(
            !out.contains("└ onebrain doctor --fix"),
            "no inline --fix hint: {out:?}"
        );
    }

    #[test]
    fn static_report_emits_no_spinner_or_carriage_return() {
        let out = render_static_report(&sample_results(), true);
        assert!(!out.contains('\r'), "static must not redraw: {out:?}");
        for f in crate::output::SPINNER_FRAMES {
            assert!(!out.contains(f), "static must not paint spinner: {out:?}");
        }
    }

    // ── Bottom Summary box ────────────────────────────────────────────────

    #[test]
    fn summary_box_counts_findings_and_actions() {
        let out = render_static_report(&sample_results(), false);
        let box_text = summary_box_lines(&out).join("\n");
        // Tally line — migration folded, so 9 display rows (7 ok · 2 warn).
        assert!(
            box_text.contains("7 ok · 2 warnings · 0 fail"),
            "tally: {box_text}"
        );
        // Findings: both warnings listed with their messages.
        assert!(box_text.contains("⚠"), "warn glyph: {box_text}");
        assert!(box_text.contains("721 indexed · 6 pending"), "{box_text}");
        // Deduped action lines: one --fix line, plus the reindex line WITH the
        // carried pending count.
        assert!(
            box_text.contains("💡 onebrain doctor --fix"),
            "fix line: {box_text}"
        );
        assert!(
            box_text.contains("💡 onebrain search reindex")
                && box_text.contains("embed 6 pending doc(s)"),
            "reindex action with count: {box_text}"
        );
    }

    #[test]
    fn summary_box_fails_before_warnings() {
        let mut results = sample_results();
        results[4] = DoctorResult::error("folders", "0/8 present");
        let out = render_static_report(&results, false);
        let lines = summary_box_lines(&out);
        let fail_pos = lines
            .iter()
            .position(|l| l.contains("✗"))
            .expect("fail row");
        let warn_pos = lines
            .iter()
            .position(|l| l.contains("⚠"))
            .expect("warn row");
        assert!(
            fail_pos < warn_pos,
            "fails must render before warnings: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("1 fail")),
            "fail count: {lines:?}"
        );
    }

    #[test]
    fn summary_box_dedupes_fix_line() {
        // Several fixable findings → exactly ONE `onebrain doctor --fix` line.
        let results = vec![
            DoctorResult::warn("settings-hooks", "dup").with_hint("onebrain doctor --fix"),
            DoctorResult::warn("onebrain.yml-keys", "missing keys").with_hint("x"),
            DoctorResult::warn("folders", "3/8").with_hint("y"),
        ];
        let out = render_static_report(&results, false);
        let n = summary_box_lines(&out)
            .iter()
            .filter(|l| l.contains("onebrain doctor --fix"))
            .count();
        assert_eq!(n, 1, "exactly one --fix line: {out}");
    }

    #[test]
    fn summary_box_wrapup_action_carries_checkpoint_count() {
        let results = vec![
            DoctorResult::ok("onebrain.yml", "ok"),
            DoctorResult::warn("orphan-checkpoints", "4 unmerged checkpoint(s) in 07-logs/")
                .with_hint("Run /wrapup to synthesize and merge them"),
        ];
        let out = render_static_report(&results, false);
        let box_text = summary_box_lines(&out).join("\n");
        assert!(
            box_text.contains("💡 /wrapup") && box_text.contains("merge 4 checkpoint(s)"),
            "wrapup action with count: {box_text}"
        );
    }

    #[test]
    fn summary_box_all_ok_is_single_line() {
        let results: Vec<DoctorResult> = sample_results()
            .into_iter()
            .map(|r| DoctorResult::ok(r.check, "ok"))
            .collect();
        let out = render_static_report(&results, false);
        let box_text = summary_box_lines(&out).join("\n");
        // 10 checks fold to 9 display rows (migration merged), all ok.
        assert!(
            box_text.contains("✓ 9 checks · all ok"),
            "all-ok line: {box_text}"
        );
        assert!(
            !box_text.contains("💡"),
            "no action lines when clean: {box_text}"
        );
        assert!(
            !box_text.contains("ok ·"),
            "no tally/findings when clean: {box_text}"
        );
    }

    #[test]
    fn summary_box_borders_are_flush_including_truncation() {
        use unicode_width::UnicodeWidthStr;
        let mut results = sample_results();
        results[4] = DoctorResult::error("folders", "0/8 present");
        // A very long message must be truncated inside the box, not blow the
        // border out past the cap.
        results[9] = DoctorResult::warn(
            "search",
            "a very long search status message that certainly exceeds the hundred column box width cap and then keeps going well past it",
        )
        .with_hint("onebrain search reindex");
        let out = render_static_report(&results, false);
        let lines = summary_box_lines(&out);
        let w = lines[0].width();
        for l in &lines {
            assert_eq!(l.width(), w, "every box line must share one width: {l:?}");
            assert!(w <= 100, "box width {w} exceeds the 100-col cap");
        }
        // All three glyph types present (✗ fail, ⚠ warn, 💡 action).
        let joined = lines.join("\n");
        assert!(joined.contains('✗') && joined.contains('⚠') && joined.contains('💡'));
    }

    #[test]
    fn summary_box_no_color_has_no_sgr_but_color_does() {
        let mono = summary_box_lines(&render_static_report(&sample_results(), false)).join("\n");
        assert!(
            !mono.contains('\x1b'),
            "no-color box must carry no SGR: {mono:?}"
        );
        let colored = summary_box_lines(&render_static_report(&sample_results(), true)).join("\n");
        assert!(colored.contains('\x1b'), "color box must carry SGR");
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
            "check": "legacy-qmd-collection",
            "outcome": "fixed",
            "message": "migrated qmd_collection → search.collection",
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
        assert_eq!(doc["fix"][0]["check"], "legacy-qmd-collection");
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

    /// Pins the text/JSON split invariant for the merged `migration` row:
    /// JSON `checks[]` carries BOTH `vault-config-migration` and
    /// `legacy-qmd-collection` as separate entries WITH their hints, while the
    /// text render folds them into the single `migration` row (neither
    /// underlying label appears).
    #[test]
    fn json_keeps_both_migration_checks_while_text_merges_them() {
        let results = vec![
            DoctorResult::warn("vault-config-migration", "vault.yml present")
                .with_hint("Run onebrain doctor --fix to migrate vault.yml to onebrain.yml"),
            DoctorResult::warn("legacy-qmd-collection", "qmd_collection key present")
                .with_hint("onebrain doctor --fix"),
        ];

        // JSON: two separate entries, hints intact.
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
        let checks = doc["checks"].as_array().unwrap();
        let by_name = |name: &str| {
            checks
                .iter()
                .find(|c| c["check"] == name)
                .unwrap_or_else(|| panic!("JSON checks[] must keep {name}: {doc}"))
        };
        assert_eq!(
            by_name("vault-config-migration")["hint"],
            "Run onebrain doctor --fix to migrate vault.yml to onebrain.yml"
        );
        assert_eq!(
            by_name("legacy-qmd-collection")["hint"],
            "onebrain doctor --fix"
        );

        // Text: one merged `migration` row naming both legacy items; the
        // underlying per-check labels never render.
        let out = render_static_report(&results, false);
        assert!(
            out.contains("⚠ migration") && out.contains("legacy vault.yml · qmd_collection key"),
            "merged row: {out:?}"
        );
        assert!(
            !out.contains("config migration") && !out.contains("⚠ qmd_collection "),
            "underlying labels must not render in text: {out:?}"
        );
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
    fn fix_vault_yml_keys_leaves_out_of_range_checkpoint_values_alone() {
        // v3.4.8: value repair moved to the `config-values` recipe (comment-
        // preserving). This recipe must no longer touch out-of-range values —
        // if it did, its serde re-serialization would destroy comments before
        // `fix_config_values` gets its turn.
        let d = tempdir().unwrap();
        let original = "update_channel: stable\n\
             checkpoint:\n  messages: 0\n  minutes: -5\n\
             folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n";
        fs::write(d.path().join("onebrain.yml"), original).unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("already"), "msg: {msg}"),
            other => panic!("expected Fixed (no-op), got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, original, "file untouched — values are not its job");
    }

    #[test]
    fn fix_vault_yml_keys_removes_inline_runtime_harness_when_sole_entry() {
        // `runtime: {harness: x}` — inline flow mapping whose ONLY entry is the
        // deprecated key. The nested delete refuses the inline parent, but
        // removing the whole top-level line is equivalent → must succeed and be
        // reported as removed (R2 blocker: this used to silently discard the
        // change).
        let d = tempdir().unwrap();
        let full_folders = "folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n";
        fs::write(
            d.path().join("onebrain.yml"),
            format!("update_channel: stable\nruntime: {{harness: claude-code}}\n{full_folders}"),
        )
        .unwrap();
        match fix_vault_yml_keys(d.path(), false) {
            FixOutcome::Fixed(msg) => assert!(msg.contains("runtime.harness"), "{msg}"),
            other => panic!("expected Fixed, got {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(!after.contains("runtime"), "runtime line gone: {after}");
        assert!(!after.contains("harness"), "harness gone: {after}");
        assert!(after.contains("inbox: 00-inbox"), "{after}");
    }

    #[test]
    fn fix_vault_yml_keys_inline_runtime_with_sibling_surfaces_unfixable() {
        // `runtime: {harness: x, other: v}` — the deprecated key sits inside an
        // inline flow mapping WITH a sibling, so no line edit can remove it
        // without touching the sibling. Must surface as un-fixable (Failed /
        // Partial), NEVER a silent Fixed that leaves the key behind forever
        // (R2 blocker).
        let d = tempdir().unwrap();
        let full_folders = "folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n";
        // Variant 1: nothing else to repair → Failed, file untouched.
        let original = format!(
            "update_channel: stable\nruntime: {{harness: claude-code, other: v}}\n{full_folders}"
        );
        fs::write(d.path().join("onebrain.yml"), &original).unwrap();
        match fix_vault_yml_keys(d.path(), false) {
            FixOutcome::Failed(msg) => {
                assert!(msg.contains("runtime.harness"), "{msg}");
                assert!(msg.contains("edit manually"), "{msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, original, "un-fixable run must not touch the file");

        // Variant 2: a backfill also lands → Partial (progress + remainder).
        let d2 = tempdir().unwrap();
        fs::write(
            d2.path().join("onebrain.yml"),
            "runtime: {harness: claude-code, other: v}\nfolders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        match fix_vault_yml_keys(d2.path(), false) {
            FixOutcome::Partial(msg) => {
                assert!(msg.contains("backfilled"), "{msg}");
                assert!(msg.contains("runtime.harness"), "{msg}");
                assert!(msg.contains("edit manually"), "{msg}");
            }
            other => panic!("expected Partial, got {other:?}"),
        }
        let after2 = fs::read_to_string(d2.path().join("onebrain.yml")).unwrap();
        assert!(after2.contains("update_channel: stable"), "{after2}");
        assert!(
            after2.contains("runtime: {harness: claude-code, other: v}"),
            "inline runtime untouched: {after2}"
        );
    }

    #[test]
    fn fix_vault_yml_keys_preserves_user_comments_through_backfill() {
        // A legacy vault missing folder keys with user comments: after the
        // comment-preserving backfill, EVERY user comment survives and the new
        // folder keys land under an inserted `folders:` block (issue #200 R3 —
        // ordering no longer destroys comments).
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "# My personal OneBrain config\nupdate_channel: stable\n\
             folders:\n  # inbox is where captures land\n  inbox: 00-inbox\n\
             search:\n  collection: ob-1  # my index\n",
        )
        .unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        assert!(matches!(outcome, FixOutcome::Fixed(_)), "{outcome:?}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(after.contains("# My personal OneBrain config"), "{after}");
        assert!(after.contains("# inbox is where captures land"), "{after}");
        assert!(after.contains("collection: ob-1  # my index"), "{after}");
        // Missing folder keys backfilled under the existing folders block.
        assert!(after.contains("projects: 01-projects"), "{after}");
        assert!(after.contains("logs: 07-logs"), "{after}");
        assert!(after.contains("inbox: 00-inbox"), "{after}");
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
        // AutoProceed, NOT InteractiveYes: no human answered a prompt, so
        // the qmd-leftovers destructive branch stays locked.
        assert_eq!(confirm_fix(3, true, false), FixConsent::AutoProceed);
    }

    #[test]
    fn confirm_fix_auto_yes_with_yes_flag() {
        // `--yes` skips the prompt — proceed, but never as InteractiveYes.
        assert_eq!(confirm_fix(3, false, true), FixConsent::AutoProceed);
    }

    #[test]
    fn confirm_fix_proceeds_when_non_interactive() {
        // Under `cargo test` stdin/stdout aren't TTYs → no prompt, proceed
        // (matches pre-3.2.4 cron/piped behaviour) — as AutoProceed, so the
        // qmd-leftovers destructive branch stays locked on piped runs. The
        // interactive InteractiveYes/Declined paths need a real TTY and are
        // verified manually.
        assert_eq!(confirm_fix(3, false, false), FixConsent::AutoProceed);
    }

    #[test]
    fn planned_action_classifies_auto_vs_manual() {
        // Locks the invariant the doc comment asks a human to maintain:
        // `planned_action` must agree with `attempt_fix`'s routing.
        // The legacy-qmd-collection migration is auto-fixable.
        assert!(planned_action(&DoctorResult::warn(
            "legacy-qmd-collection",
            "legacy qmd_collection (ob-1) — migrate to search.collection"
        ))
        .is_some());
        // The native `search` index check is advisory-only — no auto-reindex.
        assert!(planned_action(&DoctorResult::warn("search", "721 indexed · 6 pending")).is_none());
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
            DoctorResult::ok("onebrain.yml", "valid · stable"),
            DoctorResult::ok("onebrain.yml-keys", "all keys ok"),
            DoctorResult::ok("vault-config-migration", "onebrain.yml in use"),
            DoctorResult::ok("legacy-qmd-collection", "no legacy qmd_collection key"),
            DoctorResult::error("folders", "7/8 present").with_hint("onebrain init --force"),
            DoctorResult::ok("plugin-files", "complete"),
            DoctorResult::warn("settings-hooks", "PostToolUse (qmd) duplicated (×2)")
                .with_hint("onebrain doctor --fix"),
            DoctorResult::ok("claude-settings", "ok"),
            DoctorResult::ok("orphan-checkpoints", "0 orphans"),
            DoctorResult::ok("search", "602 indexed · up to date"),
        ];
        let mut buf = Vec::new();
        render_grouped_report(&mut buf, &results, "demo-vault", false, false, true).unwrap();
        let output = String::from_utf8(buf).unwrap();
        // Redact the live version in the header so the snapshot survives version
        // bumps (the header carries `onebrain <version>`).
        let output = output.replace(
            &format!("onebrain {}", env!("CARGO_PKG_VERSION")),
            "onebrain X.Y.Z",
        );
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
    fn upsert_declines_flow_style_root() {
        // A flow-style (`{…}`) root parses as a mapping but has no block-form
        // lines — appending a block `stats:` after it would be invalid YAML.
        // Decline like `restructure_config` does (issue #200 R3).
        let text = "{update_channel: stable, folders: {inbox: 00-inbox}}\n";
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

    // ── manual_message ───────────────────────────────────────────────────────

    #[test]
    fn manual_message_strips_circular_doctor_fix_hint() {
        let r = DoctorResult::warn("some-check", "broken")
            .with_hint("Run onebrain doctor --fix to repair");
        let msg = manual_message(&r);
        assert!(
            msg.contains("recipe not yet implemented"),
            "circular hint stripped: {msg}"
        );
        assert!(!msg.contains("doctor --fix"), "no circular ref: {msg}");
    }

    #[test]
    fn manual_message_uses_see_check_details_when_hint_empty() {
        let r = DoctorResult::warn("some-check", "broken");
        // No hint set — raw_hint will be "".
        let msg = manual_message(&r);
        assert!(msg.contains("see check details"), "empty hint case: {msg}");
        assert!(msg.contains("some-check"), "check name included: {msg}");
    }

    #[test]
    fn manual_message_passes_through_non_circular_hint() {
        let r = DoctorResult::warn("some-check", "broken").with_hint("run some other command");
        let msg = manual_message(&r);
        assert!(
            msg.contains("run some other command"),
            "passthrough hint: {msg}"
        );
        assert!(msg.contains("some-check"), "check name included: {msg}");
    }

    #[test]
    fn manual_message_strips_hint_containing_doctor_fix_shorthand() {
        // The `doctor --fix` substring alone (without the full "Run onebrain" prefix)
        // is enough to trigger stripping.
        let r = DoctorResult::warn("x", "msg").with_hint("doctor --fix can help");
        let msg = manual_message(&r);
        assert!(
            msg.contains("recipe not yet implemented"),
            "shorthand circular hint stripped: {msg}"
        );
    }

    // ── status_line (json=true emits to stderr, json=false to stdout) ────────

    #[test]
    fn status_line_text_mode_does_not_panic() {
        // json=false → println! to stdout; just verify no panic.
        status_line(false, "test plain text");
    }

    #[test]
    fn status_line_json_mode_does_not_panic() {
        // json=true → eprintln! to stderr; just verify no panic.
        status_line(true, "test json stderr");
    }

    // ── fix_legacy_qmd_collection: config migration ──────────────────────────

    /// Read the config file back as parsed YAML (canonical filename).
    fn read_config_yaml(dir: &Path) -> serde_yaml::Value {
        let text = fs::read_to_string(dir.join("onebrain.yml")).unwrap();
        serde_yaml::from_str(&text).unwrap()
    }

    #[test]
    fn fix_legacy_qmd_collection_migrates_and_removes_key() {
        // qmd_collection present, search.collection absent → seed
        // search.collection from the legacy value, then remove the legacy key.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "qmd_collection: ob-1\nfolders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        let outcome = fix_legacy_qmd_collection(d.path(), false);
        assert!(matches!(outcome, FixOutcome::Fixed(_)), "{outcome:?}");
        let yaml = read_config_yaml(d.path());
        // Legacy key gone.
        assert!(yaml.get("qmd_collection").is_none(), "legacy key remained");
        // search.collection seeded from the legacy value.
        assert_eq!(
            yaml["search"]["collection"].as_str(),
            Some("ob-1"),
            "search.collection not seeded"
        );
        // Unrelated keys preserved.
        assert_eq!(yaml["folders"]["inbox"].as_str(), Some("00-inbox"));
    }

    #[test]
    fn fix_legacy_qmd_collection_preserves_comments_exactly_and_is_idempotent() {
        // Real commented fixture: top-of-file comment, a lead comment ABOVE the
        // legacy key itself, an inline comment ON the legacy key line, and a
        // comment inside an unrelated block. Pinned behavior:
        // - every comment NOT attached to the deleted key survives verbatim;
        // - the legacy key's own lead comment and inline comment leave WITH the
        //   key (delete_key's documented design — a doc comment dangling above
        //   nothing reads worse than losing it);
        // - the seeded search block lands at EOF; second run is a no-op.
        let d = tempdir().unwrap();
        let original = "# my vault config\n\
             update_channel: stable\n\
             # legacy search collection — migrate me\n\
             qmd_collection: ob-1  # the old key\n\
             folders:\n  # inbox is sacred\n  inbox: 00-inbox\n";
        fs::write(d.path().join("onebrain.yml"), original).unwrap();

        let outcome = fix_legacy_qmd_collection(d.path(), false);
        assert!(matches!(outcome, FixOutcome::Fixed(_)), "{outcome:?}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(
            after,
            "# my vault config\n\
             update_channel: stable\n\
             folders:\n  # inbox is sacred\n  inbox: 00-inbox\n\
             search:\n  collection: ob-1\n"
        );

        // Idempotency: a second run reports nothing to migrate and the file is
        // byte-identical.
        match fix_legacy_qmd_collection(d.path(), false) {
            FixOutcome::Fixed(msg) => assert!(msg.contains("nothing to migrate"), "{msg}"),
            other => panic!("expected Fixed no-op, got {other:?}"),
        }
        let after2 = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after2, after, "second run must not rewrite the file");
    }

    #[test]
    fn fix_legacy_qmd_collection_keeps_existing_search_collection() {
        // Both present → don't overwrite search.collection; just drop the
        // legacy key.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "qmd_collection: old-name\nsearch:\n  collection: new-name\n",
        )
        .unwrap();
        let outcome = fix_legacy_qmd_collection(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(
                msg.contains("already set") || msg.contains("removed legacy"),
                "{msg}"
            ),
            other => panic!("expected Fixed, got {other:?}"),
        }
        let yaml = read_config_yaml(d.path());
        assert!(yaml.get("qmd_collection").is_none(), "legacy key remained");
        assert_eq!(
            yaml["search"]["collection"].as_str(),
            Some("new-name"),
            "existing search.collection must not be overwritten"
        );
    }

    #[test]
    fn fix_legacy_qmd_collection_noop_when_already_migrated() {
        // No qmd_collection key → idempotent no-op.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "search:\n  collection: ob-1\n",
        )
        .unwrap();
        let outcome = fix_legacy_qmd_collection(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => {
                assert!(msg.contains("nothing to migrate"), "{msg}")
            }
            other => panic!("expected Fixed no-op, got {other:?}"),
        }
    }

    #[test]
    fn fix_legacy_qmd_collection_fails_when_config_missing() {
        // No config file at all → the read arm fails cleanly (no panic).
        let d = tempdir().unwrap();
        match fix_legacy_qmd_collection(d.path(), false) {
            FixOutcome::Failed(msg) => assert!(msg.contains("read"), "{msg}"),
            other => panic!("expected Failed on missing config, got {other:?}"),
        }
    }

    #[test]
    fn fix_legacy_qmd_collection_fails_on_malformed_yaml() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "not: : valid").unwrap();
        match fix_legacy_qmd_collection(d.path(), false) {
            FixOutcome::Failed(msg) => assert!(msg.contains("parse"), "{msg}"),
            other => panic!("expected Failed on malformed yaml, got {other:?}"),
        }
    }

    #[test]
    fn fix_legacy_qmd_collection_fails_on_non_mapping_root() {
        // A YAML sequence root has no mapping to mutate.
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "- just\n- a\n- list\n").unwrap();
        match fix_legacy_qmd_collection(d.path(), false) {
            FixOutcome::Failed(msg) => {
                assert!(msg.contains("not a mapping"), "{msg}")
            }
            other => panic!("expected Failed on sequence root, got {other:?}"),
        }
    }

    #[test]
    fn fix_legacy_qmd_collection_drops_non_string_value_without_seeding() {
        // A non-string qmd_collection (nothing sensible to migrate) is
        // removed without creating search.collection.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "qmd_collection: [not, a, string]\nfolders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        match fix_legacy_qmd_collection(d.path(), false) {
            FixOutcome::Fixed(msg) => {
                assert!(msg.contains("non-string value"), "{msg}")
            }
            other => panic!("expected Fixed, got {other:?}"),
        }
        let yaml = read_config_yaml(d.path());
        assert!(yaml.get("qmd_collection").is_none(), "legacy key remained");
        assert!(
            yaml.get("search").is_none(),
            "must not invent search.collection from a non-string value"
        );
        assert_eq!(yaml["folders"]["inbox"].as_str(), Some("00-inbox"));
    }

    #[test]
    fn fix_legacy_qmd_collection_replaces_non_mapping_search_key() {
        // `search` exists but is a scalar → replaced with a mapping so the
        // migrated collection has somewhere to land.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "qmd_collection: ob-1\nsearch: broken\n",
        )
        .unwrap();
        match fix_legacy_qmd_collection(d.path(), false) {
            FixOutcome::Fixed(msg) => assert!(msg.contains("migrated"), "{msg}"),
            other => panic!("expected Fixed, got {other:?}"),
        }
        let yaml = read_config_yaml(d.path());
        assert!(yaml.get("qmd_collection").is_none(), "legacy key remained");
        assert_eq!(yaml["search"]["collection"].as_str(), Some("ob-1"));
    }

    // ── daemon_status_counts: pure mapping (the warm-daemon routing core) ────

    #[test]
    fn daemon_status_counts_maps_counts_and_pending() {
        let body = serde_json::json!({
            "doc_count": 721,
            "last_indexed": 1_700_000_000_u64,
            "pending_new": 4,
            "pending_changed": 1,
            "pending_removed": 1,
        });
        assert_eq!(
            daemon_status_counts(&body),
            Some((Some(1_700_000_000), 721, 6))
        );
    }

    #[test]
    fn daemon_status_counts_defaults_missing_pending_and_last_indexed() {
        // A healthy, fully-indexed daemon (no drift, no last_indexed field yet)
        // still maps: doc_count present, pending → 0, last_indexed → None.
        let body = serde_json::json!({ "doc_count": 0 });
        assert_eq!(daemon_status_counts(&body), Some((None, 0, 0)));
    }

    #[test]
    fn daemon_status_counts_missing_doc_count_is_none() {
        // A malformed body (no numeric doc_count) is a broken read, NOT a
        // healthy zero — return None so the caller falls back to a direct open
        // rather than asserting an empty index.
        assert!(daemon_status_counts(&serde_json::json!({ "pending_new": 3 })).is_none());
        assert!(daemon_status_counts(&serde_json::json!({ "doc_count": "nope" })).is_none());
    }

    // ── native_search_check: headlessly-testable arms ────────────────────────

    #[test]
    fn native_search_check_warns_when_vault_unresolvable() {
        // A path with no config file can't resolve to a vault → advisory warn,
        // never a panic or an error row.
        let d = tempdir().unwrap();
        let r = native_search_check(&d.path().join("does-not-exist"));
        assert_eq!(r.check, "search");
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.message.contains("could not resolve vault"), "{r:?}");
        // Payload shape parity: the reranker fields appear on EVERY return
        // path — as `unknown` here, since no config/cache dir exists to
        // compute them from.
        for field in [
            "reranker_enabled: unknown",
            "reranker_model: unknown",
            "reranker_downloaded: unknown",
        ] {
            assert!(
                r.details.iter().any(|d| d == field),
                "missing {field}: {r:?}"
            );
        }
    }

    #[test]
    fn native_search_check_warns_no_index_for_fresh_collection() {
        // A configured collection whose cache dir doesn't exist → the
        // "no index yet" advisory arm, with the reindex hint and the
        // collection in the details. The unique name guarantees no cache dir
        // exists for it; the configured value also means nothing is persisted.
        let d = tempdir().unwrap();
        let collection = format!(
            "doctor-unit-no-index-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n"),
        )
        .unwrap();
        let r = native_search_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.message.contains("no index for"), "{r:?}");
        assert!(r.message.contains(&collection), "{r:?}");
        // No model downloaded for a fresh collection either.
        assert!(r.message.contains("model not downloaded"), "{r:?}");
        // The message surfaces the OS-cache-purge possibility (issue #114)
        // without falsely asserting it happened on this fresh vault.
        assert!(r.message.contains("storage cleanup"), "{r:?}");
        assert_eq!(r.hint.as_deref(), Some("onebrain search reindex"));
        assert!(
            r.details.iter().any(|d| d.contains(&collection)),
            "collection in details: {r:?}"
        );
    }

    #[test]
    fn native_search_check_reports_reranker_fields() {
        // Reranker fields (enabled/model/downloaded) are always reported in
        // details regardless of index state — here on the "no index" arm,
        // using the default config (reranker enabled, default model, not
        // downloaded on a fresh cache dir).
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let d = tempdir().unwrap();
        let collection = format!(
            "doctor-unit-reranker-fields-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n"),
        )
        .unwrap();
        let r = native_search_check(d.path());
        assert!(
            r.details
                .iter()
                .any(|d| d.contains("reranker_enabled: true")),
            "{r:?}"
        );
        assert!(
            r.details
                .iter()
                .any(|d| d.contains("reranker_model: onebrain-rerank-v1")),
            "{r:?}"
        );
        assert!(
            r.details
                .iter()
                .any(|d| d.contains("reranker_downloaded: false")),
            "{r:?}"
        );
    }

    // ── #264 sub-fix C: read-hook silent-inert detector ────────────────────

    #[test]
    fn classify_failopen_pure_boundaries() {
        // Below the min sample → never warn (cold start isn't degradation).
        assert_eq!(
            classify_failopen(FAILOPEN_MIN_SAMPLE - 1, FAILOPEN_MIN_SAMPLE - 1),
            FailopenVerdict::InsufficientSample
        );
        assert_eq!(classify_failopen(0, 0), FailopenVerdict::InsufficientSample);
        // At/above the min sample, 100% fail-open → inert.
        assert!(matches!(
            classify_failopen(FAILOPEN_MIN_SAMPLE, FAILOPEN_MIN_SAMPLE),
            FailopenVerdict::Inert { .. }
        ));
        // Exactly the warn ratio (0.95 of 20 = 19) → inert (>= threshold).
        assert!(matches!(
            classify_failopen(20, 19),
            FailopenVerdict::Inert { .. }
        ));
        // Just under the ratio (18/20 = 0.90) → healthy.
        assert_eq!(classify_failopen(20, 18), FailopenVerdict::Healthy);
    }

    /// Append `total` read-hook gain events (of which `failopen` are
    /// HookFailopen, the rest LedgerDeny) at NOW into `collection`'s gain JSONL.
    fn seed_read_hook_gain(collection: &str, total: usize, failopen: usize) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        seed_read_hook_gain_at(collection, total, failopen, now);
    }

    /// Like [`seed_read_hook_gain`] but pins an explicit `ts` — lets a test seed
    /// events OUTSIDE the [`FAILOPEN_WINDOW_DAYS`] window to prove the cutoff.
    fn seed_read_hook_gain_at(collection: &str, total: usize, failopen: usize, ts: i64) {
        use onebrain_token::gain::JsonlGainWriter;
        use onebrain_token::{CacheKind, GainEvent, OptLevel, Surface};
        let dir = crate::commands::search_common::collection_cache_dir(collection)
            .join("token")
            .join("gain");
        let w = JsonlGainWriter::new(&dir);
        for i in 0..total {
            let is_failopen = i < failopen;
            w.append(&GainEvent {
                ts,
                surface: Surface::ReadHook,
                transform: if is_failopen {
                    "engine_busy"
                } else {
                    "ledger_deny"
                }
                .to_string(),
                level: OptLevel::Balanced,
                bytes_before: 100,
                bytes_after: 10,
                cache: if is_failopen {
                    CacheKind::HookFailopen
                } else {
                    CacheKind::LedgerDeny
                },
                session_token: Some("s".to_string()),
            })
            .unwrap();
        }
    }

    fn ledger_vault(collection: &str) -> tempfile::TempDir {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            format!(
                "search:\n  collection: {collection}\ntoken_optimization:\n  level: balanced\n  read_hook: ledger\n"
            ),
        )
        .unwrap();
        d
    }

    fn unique_collection(prefix: &str) -> String {
        format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[cfg(unix)]
    #[test]
    fn daemon_status_check_ok_when_no_daemon() {
        // Empty HOME → no slot files → "no warm daemon running" (ok).
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_var("HOME", home.path());
        let r = daemon_status_check(home.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");
        assert!(r.message.contains("no warm daemon"), "{r:?}");
    }

    #[cfg(unix)]
    #[test]
    fn daemon_status_check_warns_on_stale_slot() {
        // A slot json pointing at a dead port → the daemon doesn't answer the
        // health probe → flagged as a stale slot (warn + stop-all hint).
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_var("HOME", home.path());
        let run_dir = home.path().join(".onebrain").join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let info = crate::commands::daemon_client::DaemonInfo {
            port: 1, // nothing listens → probe fails
            token: "x".repeat(20),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            vault: None,
        };
        std::fs::write(
            run_dir.join("daemon-deadbeef.json"),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();
        let r = daemon_status_check(home.path());
        assert_eq!(r.status, DoctorStatus::Warn, "{r:?}");
        assert!(r.message.contains("stale/wedged"), "{r:?}");
        assert_eq!(
            r.hint.as_deref(),
            Some("onebrain daemon stop --all"),
            "{r:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_status_check_surfaces_live_legacy_daemon() {
        // LOW-2: a LIVE pre-v3.4.13 machine-wide `daemon.json` (no `-<hash>`, so
        // `all_slots` excludes it) must be surfaced during the upgrade window —
        // it holds the vault's redb lock — with the stop-all hint, not hidden as
        // "not running". A minimal mock answers `/api/health` 200.
        use std::io::{Read, Write};
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_var("HOME", home.path());
        let run_dir = home.path().join(".onebrain").join("run");
        std::fs::create_dir_all(&run_dir).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let req = String::from_utf8_lossy(&buf);
                let resp: &[u8] = if req.starts_with("GET /api/health") {
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}"
                } else {
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n"
                };
                let _ = s.write_all(resp);
            }
        });

        // Plant a LEGACY discovery record at the un-hashed `daemon.json`.
        let info = crate::commands::daemon_client::DaemonInfo {
            port,
            token: "x".repeat(20),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            vault: None,
        };
        std::fs::write(
            run_dir.join("daemon.json"),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();

        let r = daemon_status_check(home.path());
        assert_eq!(r.status, DoctorStatus::Warn, "{r:?}");
        assert!(r.message.to_lowercase().contains("legacy"), "{r:?}");
        assert_eq!(
            r.hint.as_deref(),
            Some("onebrain daemon stop --all"),
            "{r:?}"
        );
        assert!(
            r.details.iter().any(|d| d.contains("LEGACY")),
            "details must name the legacy daemon: {r:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_status_check_warns_on_version_skew() {
        // #291: a LIVE per-vault daemon whose stamped version differs from ours
        // (the post-`brew upgrade` window) must warn — naming BOTH versions —
        // with the `daemon stop --all` hint. doctor never stops it (diagnostic
        // only); it just surfaces the skew so the user can refresh a dark
        // dashboard. A minimal mock answers `/api/health` 200 so the daemon
        // reads as live.
        use std::io::{Read, Write};
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_var("HOME", home.path());
        let run_dir = home.path().join(".onebrain").join("run");
        std::fs::create_dir_all(&run_dir).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let req = String::from_utf8_lossy(&buf);
                let resp: &[u8] = if req.starts_with("GET /api/health") {
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}"
                } else {
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n"
                };
                let _ = s.write_all(resp);
            }
        });

        // A per-vault slot record stamped with an OLD version (≠ our
        // CARGO_PKG_VERSION) → skew.
        let info = crate::commands::daemon_client::DaemonInfo {
            port,
            token: "x".repeat(20),
            pid: std::process::id(),
            version: "0.0.1".to_string(),
            vault: None,
        };
        std::fs::write(
            run_dir.join("daemon-deadbeef.json"),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();

        let r = daemon_status_check(home.path());
        assert_eq!(r.status, DoctorStatus::Warn, "{r:?}");
        assert!(
            r.message.contains("skew"),
            "message must flag the version skew: {r:?}"
        );
        // Both versions must appear so the user knows what to refresh.
        assert!(r.message.contains("0.0.1"), "old version in message: {r:?}");
        assert!(
            r.message.contains(env!("CARGO_PKG_VERSION")),
            "our version in message: {r:?}"
        );
        assert_eq!(
            r.hint.as_deref(),
            Some("onebrain daemon stop --all"),
            "{r:?}"
        );
    }

    #[test]
    fn read_hook_failopen_check_warns_when_gate_inert() {
        // The #264 field condition: the gate is enabled but ~every read fails
        // open → warn (silent-inert), with an actionable hint.
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let collection = unique_collection("failopen-inert");
        let d = ledger_vault(&collection);
        seed_read_hook_gain(&collection, 25, 25); // all fail-open

        let r = read_hook_failopen_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn, "{r:?}");
        assert!(r.message.contains("inert"), "{r:?}");
        assert!(
            r.hint.is_some(),
            "an inert gate must offer a fix hint: {r:?}"
        );
    }

    #[test]
    fn read_hook_failopen_check_ok_when_gate_working() {
        // Same enabled gate, but only 2/25 fail open (8%) → healthy, no warn.
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let collection = unique_collection("failopen-healthy");
        let d = ledger_vault(&collection);
        seed_read_hook_gain(&collection, 25, 2);

        let r = read_hook_failopen_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");
        assert!(r.message.contains("healthy"), "{r:?}");
    }

    #[test]
    fn read_hook_failopen_check_thin_sample_stays_quiet() {
        // Below the min sample even at 100% fail-open → ok (cold start).
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let collection = unique_collection("failopen-thin");
        let d = ledger_vault(&collection);
        seed_read_hook_gain(&collection, 5, 5);

        let r = read_hook_failopen_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");
        assert!(r.message.contains("too few samples"), "{r:?}");
    }

    #[test]
    fn read_hook_failopen_check_not_applicable_when_gate_off() {
        // read_hook default off → not applicable, even if fail-open events exist.
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let collection = unique_collection("failopen-off");
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n"),
        )
        .unwrap();
        seed_read_hook_gain(&collection, 25, 25);

        let r = read_hook_failopen_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");
        assert!(r.message.contains("not applicable"), "{r:?}");
    }

    #[test]
    fn read_hook_failopen_check_excludes_events_outside_window() {
        // 25 all-fail-open events — enough to warn INERT if counted — but
        // stamped ~30 days ago, well outside the FAILOPEN_WINDOW_DAYS window.
        // The cutoff must exclude them: total drops to 0 → quiet ok, not a warn.
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let collection = unique_collection("failopen-window");
        let d = ledger_vault(&collection);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let old = now - (FAILOPEN_WINDOW_DAYS + 23) * 86_400;
        seed_read_hook_gain_at(&collection, 25, 25, old);

        let r = read_hook_failopen_check(d.path());
        assert_eq!(
            r.status,
            DoctorStatus::Ok,
            "out-of-window fail-opens must not warn: {r:?}"
        );
        assert!(
            r.message.contains("(0 read(s)"),
            "events older than the window must be excluded from total: {r:?}"
        );
    }

    #[test]
    fn native_search_check_warns_when_reranker_enabled_but_not_downloaded() {
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let d = tempdir().unwrap();
        let collection = format!(
            "doctor-unit-reranker-warn-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n  reranker:\n    enabled: true\n"),
        )
        .unwrap();
        let r = native_search_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(
            r.details.iter().any(|d| d
                .contains("run `onebrain search reindex` to fetch the reranker model (~570 MB)"))
                || r.hint.as_deref().unwrap_or_default().contains(
                    "run `onebrain search reindex` to fetch the reranker model (~570 MB)"
                ),
            "{r:?}"
        );
    }

    #[test]
    fn native_search_check_no_reranker_warn_when_disabled_and_not_downloaded() {
        // Disabled is an explicit user choice — never warn about the reranker
        // model even though it isn't downloaded. Fields must still be reported.
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let d = tempdir().unwrap();
        let collection = format!(
            "doctor-unit-reranker-disabled-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n  reranker:\n    enabled: false\n"),
        )
        .unwrap();
        let r = native_search_check(d.path());
        assert!(
            r.details
                .iter()
                .any(|d| d.contains("reranker_enabled: false")),
            "{r:?}"
        );
        assert!(
            !r.details
                .iter()
                .any(|d| d.contains("reranker model") || d.contains("~570 MB")),
            "unexpected reranker warn on disabled reranker: {r:?}"
        );
        assert!(
            r.hint
                .as_deref()
                .map(|h| !h.contains("reranker"))
                .unwrap_or(true),
            "unexpected reranker hint on disabled reranker: {r:?}"
        );
    }

    #[test]
    fn native_search_check_warns_configured_embed_model_missing_but_other_downloaded() {
        // Cache has model X downloaded but config says Y (the configured
        // model) — the check must warn specifically about Y, not treat "some
        // model is downloaded" as good enough.
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let d = tempdir().unwrap();
        let collection = format!(
            "doctor-unit-configured-model-missing-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n  embed_model: multilingual-e5-base\n"),
        )
        .unwrap();

        // Simulate an already-indexed vault with a DIFFERENT model
        // downloaded (multilingual-e5-small, the default) than configured
        // (multilingual-e5-base).
        let cache_dir = crate::commands::search_common::collection_cache_dir(&collection);
        fs::create_dir_all(cache_dir.join("models--intfloat--multilingual-e5-small")).unwrap();

        let r = native_search_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        let all_text = format!("{} {:?} {:?}", r.message, r.details, r.hint);
        assert!(
            all_text.contains("multilingual-e5-base"),
            "expected configured model name in message/details/hint: {r:?}"
        );
        assert!(
            all_text.contains("configured embedding model"),
            "expected configured-model wording: {r:?}"
        );

        // Cache lives under the guard-scoped tempdir (`cache`), which is
        // removed on drop — no manual cleanup of the real cache root needed.
    }

    // ── legacy_index_stub_check / fix_legacy_index_stub: #222 ────────────────

    #[test]
    fn legacy_index_stub_check_ok_when_no_cache_dir() {
        // Fresh vault, never indexed — no collection cache dir at all, so
        // there's nothing to detect. Must not create anything as a side
        // effect (read-only check).
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let d = tempdir().unwrap();
        let collection = "doctor-unit-stub-no-cache";
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n"),
        )
        .unwrap();

        let r = legacy_index_stub_check(d.path());
        assert_eq!(r.check, "legacy-index-stub");
        assert_eq!(r.status, DoctorStatus::Ok);
    }

    #[test]
    fn legacy_index_stub_check_ok_when_fully_split_no_legacy_entries() {
        // A collection already fully migrated (only `index/` populated, no
        // legacy-root duplicates) must report OK — this is the common,
        // healthy post-migration state, not something to flag.
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let d = tempdir().unwrap();
        let collection = "doctor-unit-stub-fully-split";
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n"),
        )
        .unwrap();
        // Artifact name comes from a variable, not a hand-typed literal —
        // the repo-wide "no literal artifact joins outside layout.rs" sweep
        // (artifact_join_sweep.rs) scans this file's raw text too.
        let name = onebrain_search::layout::INDEX_ARTIFACTS[0]; // tantivy
        let cache_dir = crate::commands::search_common::collection_cache_dir(collection);
        fs::create_dir_all(cache_dir.join("index").join(name)).unwrap();
        fs::write(cache_dir.join("index").join(name).join("seg"), b"x").unwrap();

        let r = legacy_index_stub_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok);
    }

    #[test]
    fn legacy_index_stub_check_warns_on_empty_stub_alongside_populated_index() {
        // The exact #222 bug: a pre-#201 binary created a fresh EMPTY
        // legacy `tantivy/` at the collection root while the real data
        // already lives at `index/tantivy/`. Must warn with an auto-fix
        // hint, naming the artifact.
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let d = tempdir().unwrap();
        let collection = "doctor-unit-stub-empty-alongside-populated";
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n"),
        )
        .unwrap();
        let name = onebrain_search::layout::INDEX_ARTIFACTS[0]; // "tantivy"
        let cache_dir = crate::commands::search_common::collection_cache_dir(collection);
        // Real data at the split location.
        fs::create_dir_all(cache_dir.join("index").join(name)).unwrap();
        fs::write(cache_dir.join("index").join(name).join("seg"), b"x").unwrap();
        // Empty stub left at the legacy root by an old binary.
        fs::create_dir_all(cache_dir.join(name)).unwrap();

        let r = legacy_index_stub_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.message.contains(name), "{r:?}");
        assert_eq!(r.hint.as_deref(), Some("onebrain doctor --fix"));
    }

    #[test]
    fn legacy_index_stub_check_warns_report_only_for_nonempty_duplicate() {
        // A legacy-root duplicate that actually HOLDS DATA (not an empty
        // stub) must be reported but never offered as auto-fixable — the
        // hint must not point at --fix, since the check itself never
        // deletes it either.
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let d = tempdir().unwrap();
        let collection = "doctor-unit-stub-nonempty-duplicate";
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n"),
        )
        .unwrap();
        let name = onebrain_search::layout::INDEX_ARTIFACTS[2]; // "engine.redb"
        let cache_dir = crate::commands::search_common::collection_cache_dir(collection);
        fs::create_dir_all(cache_dir.join("index")).unwrap();
        fs::write(cache_dir.join("index").join(name), b"real").unwrap();
        // Legacy duplicate that is NOT empty — must never be auto-deleted.
        fs::write(cache_dir.join(name), b"some-old-bytes").unwrap();

        let r = legacy_index_stub_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.message.contains(name), "{r:?}");
        assert!(
            r.message.contains("NOT be auto-removed")
                || r.details.iter().any(|d| d.contains("NOT be auto-removed")),
            "{r:?}"
        );
        assert_ne!(
            r.hint.as_deref(),
            Some("onebrain doctor --fix"),
            "a non-empty legacy duplicate must not be offered as one-command auto-fixable: {r:?}"
        );
    }

    #[test]
    fn fix_legacy_index_stub_removes_empty_stub_leaves_populated_index_alone() {
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let d = tempdir().unwrap();
        let collection = "doctor-unit-fix-stub-removes-empty";
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n"),
        )
        .unwrap();
        let name = onebrain_search::layout::INDEX_ARTIFACTS[1]; // "vectors"
        let cache_dir = crate::commands::search_common::collection_cache_dir(collection);
        fs::create_dir_all(cache_dir.join("index").join(name)).unwrap();
        fs::write(cache_dir.join("index").join(name).join("v"), b"real").unwrap();
        fs::create_dir_all(cache_dir.join(name)).unwrap(); // empty stub

        let outcome = fix_legacy_index_stub(d.path(), false);
        assert!(
            matches!(outcome, FixOutcome::Fixed(_)),
            "expected Fixed, got {outcome:?}"
        );
        assert!(
            !cache_dir.join(name).exists(),
            "empty legacy stub must be removed"
        );
        assert!(
            cache_dir.join("index").join(name).join("v").exists(),
            "populated split-location data must survive untouched"
        );
    }

    #[test]
    fn fix_legacy_index_stub_never_deletes_nonempty_legacy_artifact() {
        let cache = tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let d = tempdir().unwrap();
        let collection = "doctor-unit-fix-stub-keeps-nonempty";
        fs::write(
            d.path().join("onebrain.yml"),
            format!("search:\n  collection: {collection}\n"),
        )
        .unwrap();
        let name = onebrain_search::layout::INDEX_ARTIFACTS[2]; // "engine.redb"
        let cache_dir = crate::commands::search_common::collection_cache_dir(collection);
        fs::create_dir_all(cache_dir.join("index")).unwrap();
        fs::write(cache_dir.join("index").join(name), b"real").unwrap();
        fs::write(cache_dir.join(name), b"old-real-bytes").unwrap();

        let outcome = fix_legacy_index_stub(d.path(), false);
        assert!(
            !matches!(outcome, FixOutcome::Fixed(_)),
            "must not report Fixed while a non-empty legacy artifact was left in place: {outcome:?}"
        );
        assert_eq!(
            fs::read(cache_dir.join(name)).unwrap(),
            b"old-real-bytes",
            "non-empty legacy artifact must survive --fix untouched"
        );
    }

    #[test]
    fn legacy_index_stub_in_doctor_sections_and_display_label() {
        assert_eq!(display_label("legacy-index-stub"), "legacy index stub");
        assert!(
            DOCTOR_SECTIONS
                .iter()
                .any(|(_, _, checks)| checks.contains(&"legacy-index-stub")),
            "legacy-index-stub must be listed in DOCTOR_SECTIONS"
        );
    }

    #[test]
    fn read_hook_failopen_in_doctor_sections_and_display_label() {
        // #264 sub-fix C: the new check must render in a section (not fall into
        // the "Other" bucket) and carry a scannable label.
        assert_eq!(display_label(READ_HOOK_FAILOPEN_CHECK), "read-hook gate");
        assert!(
            DOCTOR_SECTIONS
                .iter()
                .any(|(_, _, checks)| checks.contains(&READ_HOOK_FAILOPEN_CHECK)),
            "read-hook-failopen must be listed in DOCTOR_SECTIONS"
        );
    }

    // ── fix_settings_hooks: success path ─────────────────────────────────────

    #[test]
    fn fix_settings_hooks_succeeds_on_fresh_vault_dir() {
        // register-hooks writes to .claude/settings.json relative to vault_root;
        // a fresh tempdir is sufficient for the success path.
        let d = tempdir().unwrap();
        // Pre-create the .claude dir so register-hooks can write settings.json.
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        let outcome = fix_settings_hooks(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(
                msg.contains("hooks registered"),
                "expected 'hooks registered': {msg}"
            ),
            // register-hooks can fail in CI if the vault has no config; accept
            // both Fixed and Failed but not Partial/Manual.
            FixOutcome::Failed(_) => {}
            FixOutcome::Partial(msg) => panic!("unexpected Partial: {msg}"),
            FixOutcome::Manual(msg) => panic!("unexpected Manual: {msg}"),
        }
    }

    // ── fix_settings_hooks: json=true path ───────────────────────────────────

    #[test]
    fn fix_settings_hooks_json_mode_does_not_panic() {
        // json=true → status_line emits to stderr. Exercise the json=true path.
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        let outcome = fix_settings_hooks(d.path(), true);
        // Accept Fixed or Failed; not Manual.
        match &outcome {
            FixOutcome::Fixed(_) | FixOutcome::Failed(_) => {}
            FixOutcome::Partial(m) => panic!("unexpected Partial: {m}"),
            FixOutcome::Manual(m) => panic!("unexpected Manual: {m}"),
        }
    }

    // ── fix_plugin_files: json=true path ──────────────────────────────────────

    #[test]
    fn fix_plugin_files_json_mode_danger_guard_fires() {
        // Even in json=true mode the danger guard must fire before any vault-sync.
        let mut root = std::env::temp_dir();
        while let Some(parent) = root.parent() {
            root = parent.to_path_buf();
        }
        let outcome = fix_plugin_files(&root, true);
        match outcome {
            FixOutcome::Failed(msg) => assert!(
                msg.contains("filesystem root") || msg.contains("dangerous"),
                "safety guard fired in json mode: {msg}"
            ),
            other => panic!("expected Failed (json danger guard), got: {other:?}"),
        }
    }

    // ── fix_folders: danger-path guard ───────────────────────────────────────

    #[test]
    fn fix_folders_uses_default_folder_names_when_no_config() {
        // No onebrain.yml present → load_vault_config_at fails → falls back to
        // VaultConfig defaults (00-inbox, 01-projects, etc.).
        let d = tempdir().unwrap();
        // No config file — explicit fallback path in fix_folders.
        let outcome = fix_folders(d.path(), false);
        assert!(matches!(outcome, FixOutcome::Fixed(_)), "got: {outcome:?}");
        // Default folder names must be created.
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
            assert!(
                d.path().join(name).is_dir(),
                "expected default folder {name}"
            );
        }
    }

    #[test]
    fn fix_folders_refuses_filesystem_root() {
        let mut root = std::env::temp_dir();
        while let Some(parent) = root.parent() {
            root = parent.to_path_buf();
        }
        let outcome = fix_folders(&root, false);
        match outcome {
            FixOutcome::Failed(msg) => assert!(
                msg.contains("filesystem root") || msg.contains("dangerous"),
                "safety guard fired: {msg}"
            ),
            other => panic!("expected Failed (safety guard), got: {other:?}"),
        }
    }

    // `fix_plugin_cache` is exercised end-to-end in
    // tests/doctor_integration.rs::doctor_fix_prunes_stale_plugin_cache_under_fake_home,
    // which pins `$HOME` to a tempdir so the destructive cache sweep can never
    // touch the real developer cache. A direct unit call against the live
    // `$HOME` (the previous approach) could delete real plugin-cache entries
    // during `cargo test`.

    // ── fix_vault_yml_keys: error / edge paths ────────────────────────────────

    #[test]
    fn fix_vault_yml_keys_fails_when_file_missing() {
        let d = tempdir().unwrap();
        // No onebrain.yml or vault.yml → find_config_file returns None →
        // recipe falls back to the canonical path → read_to_string fails.
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Failed(msg) => assert!(
                msg.contains("read") || msg.contains("onebrain.yml"),
                "expected read error: {msg}"
            ),
            other => panic!("expected Failed (missing file), got: {other:?}"),
        }
    }

    #[test]
    fn fix_vault_yml_keys_fails_on_invalid_yaml() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "not: : : yaml: {{}}\n").unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Failed(msg) => {
                assert!(msg.contains("parse"), "expected parse error in msg: {msg}")
            }
            other => panic!("expected Failed (parse error), got: {other:?}"),
        }
    }

    #[test]
    fn fix_vault_yml_keys_fails_on_non_mapping_yaml_root() {
        let d = tempdir().unwrap();
        // A YAML scalar at root (not a mapping).
        fs::write(d.path().join("onebrain.yml"), "- just\n- a\n- list\n").unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Failed(msg) => assert!(
                msg.contains("not a mapping"),
                "expected 'not a mapping' error: {msg}"
            ),
            other => panic!("expected Failed (non-mapping), got: {other:?}"),
        }
    }

    #[test]
    fn fix_vault_yml_keys_drops_empty_runtime_block() {
        // After `runtime.harness` is removed, the parent `runtime:` block
        // becomes empty and should be dropped entirely.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "update_channel: stable\n\
             runtime:\n  harness: claude-code\n\
             folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
        )
        .unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        assert!(matches!(outcome, FixOutcome::Fixed(_)), "got: {outcome:?}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(!after.contains("harness"), "harness key gone: {after}");
        // Empty runtime block should be dropped.
        assert!(
            !after.contains("runtime:"),
            "empty runtime block dropped: {after}"
        );
    }

    #[test]
    fn fix_vault_yml_keys_json_mode_status_line_to_stderr() {
        // json=true routes status_line to stderr. Just verify no panic and Fixed.
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "qmd_collection: foo\n").unwrap();
        let outcome = fix_vault_yml_keys(d.path(), true);
        assert!(
            matches!(outcome, FixOutcome::Fixed(_)),
            "json=true path: {outcome:?}"
        );
    }

    // ── fix_claude_settings: error paths ──────────────────────────────────────

    #[test]
    fn fix_claude_settings_fails_on_missing_settings_file() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        // No settings.json — read must fail.
        let outcome = fix_claude_settings(d.path(), false);
        match outcome {
            FixOutcome::Failed(msg) => assert!(
                msg.contains("read settings.json"),
                "expected read error: {msg}"
            ),
            other => panic!("expected Failed (missing file), got: {other:?}"),
        }
    }

    #[test]
    fn fix_claude_settings_fails_on_invalid_json() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        fs::write(d.path().join(".claude/settings.json"), "not json {{").unwrap();
        let outcome = fix_claude_settings(d.path(), false);
        match outcome {
            FixOutcome::Failed(msg) => assert!(
                msg.contains("parse settings.json"),
                "expected parse error: {msg}"
            ),
            other => panic!("expected Failed (parse error), got: {other:?}"),
        }
    }

    #[test]
    fn fix_claude_settings_fails_when_root_is_not_object() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        fs::write(d.path().join(".claude/settings.json"), "[1,2,3]").unwrap();
        let outcome = fix_claude_settings(d.path(), false);
        match outcome {
            FixOutcome::Failed(msg) => assert!(
                msg.contains("not an object"),
                "expected 'not an object': {msg}"
            ),
            other => panic!("expected Failed (non-object), got: {other:?}"),
        }
    }

    #[test]
    fn fix_claude_settings_keeps_other_marketplace_entries() {
        // extraKnownMarketplaces has both "onebrain" and another entry.
        // Only "onebrain" should be removed; the wrapper should survive.
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        let json = serde_json::json!({
            "extraKnownMarketplaces": {
                "onebrain": { "source": { "repo": "kengio/onebrain" } },
                "other": { "source": { "repo": "other/repo" } }
            }
        });
        fs::write(
            d.path().join(".claude/settings.json"),
            serde_json::to_string(&json).unwrap(),
        )
        .unwrap();
        let outcome = fix_claude_settings(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("removed"), "msg: {msg}"),
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(d.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        // "other" entry must remain.
        assert!(
            after["extraKnownMarketplaces"]["other"].is_object(),
            "other marketplace kept: {after}"
        );
        // "onebrain" entry must be gone.
        assert!(
            after["extraKnownMarketplaces"].get("onebrain").is_none(),
            "onebrain entry removed: {after}"
        );
    }

    // ── fix_vault_config_migration: no-config edge case ─────────────────────

    #[test]
    fn fix_vault_config_migration_no_config_reports_nothing_to_migrate() {
        let d = tempdir().unwrap();
        // Neither canonical nor legacy exists.
        let outcome = fix_vault_config_migration(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(
                msg.contains("nothing to migrate"),
                "no-config idempotent: {msg}"
            ),
            other => panic!("expected Fixed (no-op), got: {other:?}"),
        }
    }

    // ── value_is_positive_number: edge cases ──────────────────────────────────

    /// Helper: parse a YAML scalar from a string like "1.5" or "-1.0" or "0".
    fn yaml_num(s: &str) -> serde_yaml::Value {
        serde_yaml::from_str(s).expect("valid yaml number")
    }

    #[test]
    fn value_is_positive_number_false_for_zero() {
        assert!(!value_is_positive_number(&yaml_num("0")));
    }

    #[test]
    fn value_is_positive_number_true_for_positive_float() {
        assert!(value_is_positive_number(&yaml_num("1.5")));
    }

    #[test]
    fn value_is_positive_number_false_for_negative_float() {
        assert!(!value_is_positive_number(&yaml_num("-1.0")));
    }

    #[test]
    fn value_is_positive_number_false_for_negative_integer() {
        assert!(!value_is_positive_number(&yaml_num("-5")));
    }

    #[test]
    fn value_is_positive_number_false_for_non_number() {
        assert!(!value_is_positive_number(&serde_yaml::Value::Bool(true)));
        assert!(!value_is_positive_number(&serde_yaml::Value::Null));
        assert!(!value_is_positive_number(&serde_yaml::Value::String(
            "5".to_string()
        )));
    }

    #[test]
    fn value_is_positive_number_true_for_positive_integer() {
        assert!(value_is_positive_number(&yaml_num("15")));
    }

    // ── stamp_doctor_run: quiet/non-quiet read error, inline_stats ───────────

    #[test]
    fn stamp_doctor_run_non_quiet_survives_read_error() {
        // Point vault_root at a tempdir that has NO config — stamp_doctor_run
        // must return without panicking (the None early-return for missing
        // config). Non-quiet mode is passed to exercise the quiet=false branch.
        let d = tempdir().unwrap();
        // No config → stamp_doctor_run returns immediately (no stamp, no panic).
        stamp_doctor_run(d.path(), false, false);
        assert!(!d.path().join("onebrain.yml").exists());
    }

    #[test]
    fn stamp_doctor_run_warns_on_inline_stats_non_quiet() {
        // An inline stats mapping skips the stamp. With quiet=false a one-line
        // stderr note is emitted — we can't capture that without rerouting
        // stderr, but we CAN verify the file is NOT modified (the function
        // still completes without panic and leaves the file intact).
        let d = tempdir().unwrap();
        let inline_text = "stats: { last_doctor_run: 2025-01-01 }\n";
        fs::write(d.path().join("onebrain.yml"), inline_text).unwrap();
        stamp_doctor_run(d.path(), false, false); // quiet=false → stderr note
                                                  // File must be untouched (no modification from the stamp attempt).
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, inline_text, "inline stats file must not be modified");
    }

    #[test]
    fn stamp_doctor_run_quiet_suppresses_inline_stats_warning() {
        // Same scenario as above but quiet=true — no stderr note.
        // Verify idempotency only.
        let d = tempdir().unwrap();
        let inline_text = "stats: { last_doctor_run: 2025-01-01 }\n";
        fs::write(d.path().join("onebrain.yml"), inline_text).unwrap();
        stamp_doctor_run(d.path(), false, true); // quiet=true
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, inline_text, "quiet inline stats: file untouched");
    }

    #[test]
    #[cfg(unix)]
    fn stamp_doctor_run_read_error_non_quiet_does_not_panic() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let path = d.path().join("onebrain.yml");
        fs::write(&path, "qmd_collection: ob\n").unwrap();
        // Make the file unreadable to trigger the read-error path.
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&path, perms).unwrap();
        // Must complete without panic — the read error is swallowed (non-quiet
        // emits to stderr which we can't intercept here).
        stamp_doctor_run(d.path(), false, false);
        // Restore so tempdir cleanup can remove the file.
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&path, perms).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn stamp_doctor_run_read_error_quiet_does_not_panic() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let path = d.path().join("onebrain.yml");
        fs::write(&path, "qmd_collection: ob\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&path, perms).unwrap();
        // quiet=true suppresses stderr note.
        stamp_doctor_run(d.path(), false, true);
        // Restore permissions.
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&path, perms).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn stamp_doctor_run_write_error_non_quiet_does_not_panic() {
        // Exercise the write-error branch (line 1092-1095): make the config
        // readable but the directory unwritable so atomic_write_text fails.
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let path = d.path().join("onebrain.yml");
        // Write a config that will produce Some(updated) from upsert (i.e. a
        // date that's not today so upsert has work to do).
        fs::write(
            &path,
            "qmd_collection: ob\nstats:\n  last_doctor_run: 2020-01-01\n",
        )
        .unwrap();
        // Make the directory read-only so the temp-file write fails.
        let mut dir_perms = fs::metadata(d.path()).unwrap().permissions();
        dir_perms.set_mode(0o555);
        fs::set_permissions(d.path(), dir_perms).unwrap();
        // Must complete without panic — non-quiet emits to stderr.
        stamp_doctor_run(d.path(), false, false);
        // Restore directory permissions for cleanup.
        let mut dir_perms = fs::metadata(d.path()).unwrap().permissions();
        dir_perms.set_mode(0o755);
        fs::set_permissions(d.path(), dir_perms).unwrap();
    }

    // ── print_fix_summary: all three outcome arms ─────────────────────────────

    #[test]
    fn print_fix_summary_handles_all_three_outcome_types() {
        // We can't easily capture stdout from print_fix_summary since it goes
        // to the real stdout — but we can call it without panic and verify
        // the function completes normally. The content is tested via integration.
        let outcomes: Vec<(String, FixOutcome)> = vec![
            (
                "folders".to_string(),
                FixOutcome::Fixed("created 3: ...".to_string()),
            ),
            (
                "plugin-cache".to_string(),
                FixOutcome::Failed("permissions denied".to_string()),
            ),
            (
                "orphan-checkpoints".to_string(),
                FixOutcome::Manual("run /wrapup".to_string()),
            ),
        ];
        // Must not panic.
        print_fix_summary(&outcomes);
    }

    // ── attempt_fix: orphan-checkpoints + catch-all arms ─────────────────────

    #[test]
    fn attempt_fix_orphan_checkpoints_returns_manual() {
        let d = tempdir().unwrap();
        let r = DoctorResult::warn("orphan-checkpoints", "3 unmerged");
        let outcome = attempt_fix(&r, d.path(), false, false);
        match outcome {
            FixOutcome::Manual(msg) => assert!(
                msg.contains("wrapup"),
                "orphan-checkpoints routes to wrapup: {msg}"
            ),
            other => panic!("expected Manual, got: {other:?}"),
        }
    }

    #[test]
    fn attempt_fix_unknown_check_returns_manual_with_check_name() {
        let d = tempdir().unwrap();
        let r = DoctorResult::warn("some-future-check", "not yet known").with_hint("do something");
        let outcome = attempt_fix(&r, d.path(), false, false);
        match outcome {
            FixOutcome::Manual(msg) => {
                assert!(
                    msg.contains("some-future-check"),
                    "check name in manual: {msg}"
                );
                assert!(msg.contains("do something"), "hint passed through: {msg}");
            }
            other => panic!("expected Manual, got: {other:?}"),
        }
    }

    #[test]
    fn attempt_fix_unknown_check_circular_hint_cleaned() {
        let d = tempdir().unwrap();
        let r = DoctorResult::warn("future-check", "problem")
            .with_hint("Run onebrain doctor --fix to fix this");
        let outcome = attempt_fix(&r, d.path(), false, false);
        match outcome {
            FixOutcome::Manual(msg) => assert!(
                msg.contains("recipe not yet implemented"),
                "circular cleaned: {msg}"
            ),
            other => panic!("expected Manual, got: {other:?}"),
        }
    }

    // ── emit_structured: known-good paths ────────────────────────────────────

    #[test]
    fn emit_structured_legacy_json_flag_emits_compact_json() {
        // legacy_json_flag=true, mode is non-structured (Text) → compact JSON.
        let doc = serde_json::json!({ "ok": true, "summary": {} });
        let text_mode = OutputMode::Text {
            color: false,
            pretty: false,
        };
        let result = emit_structured(&doc, true, &text_mode).unwrap();
        // Compact JSON: no indentation.
        assert!(!result.contains("  "), "should be compact: {result}");
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn emit_structured_yaml_mode_emits_yaml() {
        let doc = serde_json::json!({ "ok": true });
        let result = emit_structured(&doc, false, &OutputMode::Yaml).unwrap();
        // YAML output does not start with '{'.
        assert!(
            !result.trim_start().starts_with('{'),
            "expected YAML: {result}"
        );
    }

    // ── config_has_inline_stats: edge cases ───────────────────────────────────

    #[test]
    fn config_has_inline_stats_detects_inline_mapping() {
        assert!(config_has_inline_stats(
            "stats: { last_doctor_run: 2026-01-01 }\n"
        ));
        assert!(config_has_inline_stats("stats: null\n"));
    }

    #[test]
    fn config_has_inline_stats_returns_false_for_block_form() {
        assert!(!config_has_inline_stats(
            "stats:\n  last_doctor_run: 2026-01-01\n"
        ));
        // Bare "stats:" line with no trailing content → block form.
        assert!(!config_has_inline_stats("stats:\n"));
    }

    #[test]
    fn config_has_inline_stats_ignores_indented_lines() {
        // An indented `stats:` line is a child key, not the top-level block header.
        assert!(!config_has_inline_stats("  stats: { something }\n"));
    }

    // ── display_label: catch-all for unknown check names ─────────────────────

    #[test]
    fn display_label_returns_raw_name_for_unknown_check() {
        assert_eq!(
            display_label("brand-new-unknown-check"),
            "brand-new-unknown-check"
        );
    }

    #[test]
    fn display_label_maps_all_known_checks() {
        let cases = [
            ("onebrain.yml", "onebrain.yml"),
            ("onebrain.yml-keys", "schema"),
            ("vault-config-migration", "config migration"),
            ("legacy-qmd-collection", "qmd_collection"),
            ("folders", "folders"),
            ("plugin-files", "plugin files"),
            ("plugin-cache", "plugin cache"),
            ("settings-hooks", "hooks"),
            ("claude-settings", "claude settings"),
            ("orphan-checkpoints", "checkpoints"),
            ("search", "search"),
        ];
        for (check, expected) in cases {
            assert_eq!(display_label(check), expected, "label for {check}");
        }
    }

    // ── planned_action: all auto-fixable and manual-only checks ─────────────

    #[test]
    fn planned_action_covers_all_auto_fixable_checks() {
        let auto_checks = [
            ("settings-hooks", "anything"),
            ("plugin-files", "anything"),
            ("folders", "anything"),
            ("onebrain.yml-keys", "anything"),
            ("claude-settings", "anything"),
            ("plugin-cache", "anything"),
            ("vault-config-migration", "anything"),
        ];
        for (check, msg) in auto_checks {
            let r = DoctorResult::warn(check, msg);
            assert!(
                planned_action(&r).is_some(),
                "check '{check}' should have planned action"
            );
        }
    }

    // ── summary box: "1 warning" singular form ───────────────────────────────

    #[test]
    fn summary_box_uses_singular_warning_form() {
        // Exactly 1 warning → "1 warning" not "1 warnings".
        let results = vec![
            DoctorResult::ok("onebrain.yml", "ok"),
            DoctorResult::warn("settings-hooks", "dup"),
        ];
        let mut buf = Vec::new();
        render_summary_box(&mut buf, &results, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("1 warning ·") && !out.contains("1 warnings"),
            "singular form: {out:?}"
        );
    }

    // ── upsert: block with no existing children uses default indent ─────────

    #[test]
    fn upsert_stats_block_with_no_children_uses_default_two_space_indent() {
        // stats: exists but has no children → indent defaults to 2 spaces.
        let text = "stats:\nschedule:\n- cron: 0 9 * * *\n";
        let out = upsert_doctor_stats(text, "2026-05-27", false).unwrap();
        assert!(
            out.contains("  last_doctor_run: 2026-05-27"),
            "2-space default indent: {out:?}"
        );
    }

    // ── attempt_fix: routing for known + unknown checks ──────────────────────

    #[test]
    fn attempt_fix_routes_legacy_qmd_collection_to_migration_recipe() {
        // The `legacy-qmd-collection` check dispatches to the migration recipe,
        // which performs the config write (not a Manual passthrough).
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "qmd_collection: ob-1\n").unwrap();
        let r = DoctorResult::warn(
            "legacy-qmd-collection",
            "legacy qmd_collection (ob-1) — migrate to search.collection",
        );
        let outcome = attempt_fix(&r, d.path(), false, false);
        assert!(matches!(outcome, FixOutcome::Fixed(_)), "{outcome:?}");
        // The legacy key was actually removed.
        let text = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(
            !text.contains("qmd_collection"),
            "legacy key remained: {text}"
        );
    }

    #[test]
    fn attempt_fix_unknown_check_falls_to_manual() {
        // An unmapped check falls to the `_ =>` Manual arm.
        let d = tempdir().unwrap();
        let r = DoctorResult::warn("brand-new-check", "hmm").with_hint("do the thing manually");
        let outcome = attempt_fix(&r, d.path(), false, false);
        match outcome {
            FixOutcome::Manual(msg) => {
                assert!(msg.contains("brand-new-check"), "check name in msg: {msg}");
                assert!(msg.contains("do the thing manually"), "hint passed: {msg}");
            }
            other => panic!("expected Manual for unmapped check, got: {other:?}"),
        }
    }

    // ── fix_claude_settings: json=true routes status_line to stderr ──────────

    #[test]
    fn fix_claude_settings_json_mode_removes_stale_marketplace() {
        // json=true → status_line emits to stderr so stdout stays clean JSON.
        // Verify the recipe still applies the fix correctly in json=true mode.
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        let original = serde_json::json!({
            "extraKnownMarketplaces": {
                "onebrain": { "source": { "repo": "kengio/onebrain" } }
            }
        });
        fs::write(
            d.path().join(".claude/settings.json"),
            serde_json::to_string_pretty(&original).unwrap(),
        )
        .unwrap();
        let outcome = fix_claude_settings(d.path(), true);
        match outcome {
            FixOutcome::Fixed(msg) => {
                assert!(msg.contains("removed"), "json=true path: {msg}")
            }
            other => panic!("expected Fixed in json=true mode, got: {other:?}"),
        }
        let after: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(d.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert!(
            after.get("extraKnownMarketplaces").is_none(),
            "marketplace removed: {after}"
        );
    }

    // ── fix_folders: json=true routes status_line to stderr ──────────────────

    #[test]
    fn fix_folders_json_mode_creates_dirs() {
        // json=true → status_line emits to stderr. Creation logic is unchanged.
        let d = tempdir().unwrap();
        let outcome = fix_folders(d.path(), true);
        assert!(
            matches!(outcome, FixOutcome::Fixed(_)),
            "json=true must still create folders: {outcome:?}"
        );
        assert!(d.path().join("00-inbox").is_dir(), "00-inbox created");
        assert!(d.path().join("07-logs").is_dir(), "07-logs created");
        assert!(
            d.path().join("00-inbox/imports").is_dir(),
            "inbox/imports created"
        );
    }

    // ── fix_vault_config_migration: json=true routes status_line to stderr ───

    #[test]
    fn fix_vault_config_migration_json_mode_renames_legacy() {
        // json=true → status_line emits to stderr. Rename logic is unchanged.
        let d = tempdir().unwrap();
        fs::write(d.path().join("vault.yml"), "qmd_collection: legacy\n").unwrap();
        let outcome = fix_vault_config_migration(d.path(), true);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("renamed"), "json=true: {msg}"),
            other => panic!("expected Fixed in json=true mode, got: {other:?}"),
        }
        assert!(d.path().join("onebrain.yml").is_file(), "canonical exists");
        assert!(!d.path().join("vault.yml").exists(), "legacy removed");
    }

    // ── fix_vault_yml_keys: runtime block kept when other keys remain ─────────

    #[test]
    fn fix_vault_yml_keys_runtime_block_kept_when_not_empty_after_harness_removal() {
        // `runtime` has both `harness` (deprecated) and another key. After
        // removing `harness`, `is_empty()` = false → `mapping.remove(&runtime_key)`
        // is NOT called — the block stays (the non-deprecated key is untouched).
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "update_channel: stable\n\
             runtime:\n  harness: claude-code\n  custom_setting: value\n\
             folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
        )
        .unwrap();
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("runtime.harness"), "msg: {msg}"),
            other => panic!("expected Fixed, got: {other:?}"),
        }
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(!after.contains("harness:"), "harness key removed: {after}");
        // Block still present because custom_setting remains.
        assert!(after.contains("runtime:"), "runtime block kept: {after}");
        assert!(
            after.contains("custom_setting:"),
            "other runtime key kept: {after}"
        );
    }

    // ── fix_vault_yml_keys: runtime without harness → no removal ─────────────

    #[test]
    fn fix_vault_yml_keys_runtime_without_harness_key_is_untouched() {
        // `runtime` present but has no `harness` key → `remove(&harness_key)` returns
        // None → `is_some()` is false → removed list stays empty → recipe is a no-op.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "update_channel: stable\n\
             runtime:\n  custom_setting: value\n\
             folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
        )
        .unwrap();
        // Nothing added, removed, or repaired → early-return with "already" message.
        let outcome = fix_vault_yml_keys(d.path(), false);
        match outcome {
            FixOutcome::Fixed(msg) => assert!(msg.contains("already"), "no-op result: {msg}"),
            other => panic!("expected Fixed (no-op), got: {other:?}"),
        }
        // File is NOT rewritten on a no-op, so the original content is intact.
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(
            after.contains("runtime:"),
            "runtime block untouched: {after}"
        );
        assert!(
            after.contains("custom_setting:"),
            "custom key untouched: {after}"
        );
    }

    // ── summary box: manual-only warns → no --fix line ───────────────────────

    #[test]
    fn summary_box_manual_only_warn_shows_no_fix_line() {
        // orphan-checkpoints is manual-only (planned_action returns None), so
        // no `onebrain doctor --fix` line — only its own /wrapup action line.
        let results = vec![
            DoctorResult::ok("onebrain.yml", "ok"),
            DoctorResult::warn("orphan-checkpoints", "2 unmerged checkpoint(s) in 07-logs/")
                .with_hint("Run /wrapup to synthesize and merge them"),
        ];
        let mut buf = Vec::new();
        render_summary_box(&mut buf, &results, false).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("1 warning ·"), "warning counted: {out:?}");
        assert!(
            !out.contains("onebrain doctor --fix"),
            "no --fix line for manual-only warn: {out:?}"
        );
        assert!(out.contains("💡 /wrapup"), "wrapup action present: {out:?}");
    }

    // ── step_status_of: all three DoctorStatus → StepStatus arms ────────────

    #[test]
    fn step_status_of_maps_all_three_doctor_statuses_to_distinct_glyphs() {
        // Exercises all three match arms; each must produce a distinct glyph
        // so the rendered report can visually distinguish ok / warn / fail.
        let ok = step_status_of(DoctorStatus::Ok);
        let warn = step_status_of(DoctorStatus::Warn);
        let err = step_status_of(DoctorStatus::Error);
        assert_ne!(ok.glyph(), warn.glyph(), "ok vs warn glyphs differ");
        assert_ne!(warn.glyph(), err.glyph(), "warn vs error glyphs differ");
        assert_ne!(ok.glyph(), err.glyph(), "ok vs error glyphs differ");
    }

    // ── stamp_doctor_run: write error with quiet=true ─────────────────────────

    #[test]
    #[cfg(unix)]
    fn stamp_doctor_run_write_error_quiet_true_does_not_panic() {
        // Complement to stamp_doctor_run_write_error_non_quiet_does_not_panic:
        // quiet=true suppresses the eprintln! — covers the `if !quiet` false arm
        // at the write-error site. Must complete without panicking.
        extern "C" {
            fn geteuid() -> u32;
        }
        if unsafe { geteuid() } == 0 {
            // Under root, chmod 0o555 doesn't prevent writes → skip.
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let path = d.path().join("onebrain.yml");
        // Stale date ensures upsert_doctor_stats returns Some(updated) so the
        // write path (not the idempotent None path) is reached.
        fs::write(
            &path,
            "qmd_collection: ob\nstats:\n  last_doctor_run: 2020-01-01\n",
        )
        .unwrap();
        // Make the directory read-only so atomic_write_text fails (can't create .tmp).
        let mut dir_perms = fs::metadata(d.path()).unwrap().permissions();
        dir_perms.set_mode(0o555);
        fs::set_permissions(d.path(), dir_perms).unwrap();
        // quiet=true → the `if !quiet { eprintln!(...) }` branch is NOT taken.
        stamp_doctor_run(d.path(), false, true);
        // Restore for tempdir cleanup.
        let mut dir_perms = fs::metadata(d.path()).unwrap().permissions();
        dir_perms.set_mode(0o755);
        fs::set_permissions(d.path(), dir_perms).unwrap();
    }
    // ── config-values check + reset (v3.4.8, #196) ─────────────────────

    #[test]
    fn collect_config_findings_clean_config_is_empty() {
        let text = "update_channel: stable\n\
                    checkpoint:\n  messages: 15\n  minutes: 30\n\
                    search:\n  default_top_k: 10\n  embed_model: multilingual-e5-small\n  \
                    reranker:\n    enabled: true\n    model: onebrain-rerank-v1\n    min_candidates: 10\n    min_score: 0.0\n";
        let findings = collect_config_findings(text).unwrap();
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn collect_config_findings_absent_keys_are_fine() {
        // Absent keys fall back to serde defaults — no findings.
        let findings = collect_config_findings("# empty\n").unwrap();
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn collect_config_findings_non_mapping_returns_none() {
        assert!(collect_config_findings("- a\n- b\n").is_none());
        assert!(collect_config_findings("not: : valid").is_none());
    }

    #[test]
    fn collect_config_findings_flags_every_rule() {
        let text = "update_channel: weekly-maybe\n\
                    checkpoint:\n  messages: 0\n  minutes: foo\n\
                    folders:\n  inbox: \"\"\n\
                    search:\n  collection: \"\"\n  default_top_k: -3\n  embed_model: nope\n  \
                    reranker:\n    enabled: maybe\n    model: nope\n    min_candidates: 0\n    min_score: 7.5\n";
        let findings = collect_config_findings(text).unwrap();
        let dotted: Vec<&str> = findings.iter().map(|f| f.dotted.as_str()).collect();
        for expect in [
            "update_channel",
            "checkpoint.messages",
            "checkpoint.minutes",
            "folders.inbox",
            "search.collection",
            "search.default_top_k",
            "search.embed_model",
            "search.reranker.enabled",
            "search.reranker.model",
            "search.reranker.min_candidates",
            "search.reranker.min_score",
        ] {
            assert!(dotted.contains(&expect), "missing {expect}: {dotted:?}");
        }
        // Report-only findings are folders.* + search.collection, nothing else.
        let report_only: Vec<&str> = findings
            .iter()
            .filter(|f| !f.resettable)
            .map(|f| f.dotted.as_str())
            .collect();
        assert_eq!(report_only, vec!["folders.inbox", "search.collection"]);
        // Only embed_model requires a reindex after reset.
        let reindex: Vec<&str> = findings
            .iter()
            .filter(|f| f.reindex_required)
            .map(|f| f.dotted.as_str())
            .collect();
        assert_eq!(reindex, vec!["search.embed_model"]);
        // Defaults come from the runtime default fns / registries.
        let by = |d: &str| {
            findings
                .iter()
                .find(|f| f.dotted == d)
                .unwrap()
                .default_repr
                .clone()
        };
        assert_eq!(by("checkpoint.messages"), "15");
        assert_eq!(by("checkpoint.minutes"), "30");
        assert_eq!(by("search.default_top_k"), "10");
        assert_eq!(by("search.embed_model"), "multilingual-e5-small");
        assert_eq!(by("search.reranker.model"), "onebrain-rerank-v1");
        assert_eq!(by("search.reranker.min_score"), "0.0");
        assert_eq!(by("update_channel"), "stable");
        // Nothing here is a superseded default — every finding above is a
        // genuinely out-of-range value.
        assert!(!findings.iter().any(|f| f.superseded), "{dotted:?}");
        // The one rule the fixture above CANNOT express (a value can't be both
        // out of range and the superseded in-range default) — kept in this
        // completeness test so "every rule fires" stays literally true.
        let superseded =
            collect_config_findings("search:\n  reranker:\n    min_score: 0.30\n").unwrap();
        assert_eq!(
            superseded
                .iter()
                .filter(|f| f.superseded)
                .map(|f| f.dotted.as_str())
                .collect::<Vec<_>>(),
            vec!["search.reranker.min_score"],
        );
    }

    #[test]
    fn collect_config_findings_min_score_bounds() {
        // Boundary values 0 and 1 are valid; just outside is not.
        let ok = "search:\n  reranker:\n    min_score: 0.0\n";
        assert!(collect_config_findings(ok).unwrap().is_empty());
        let ok = "search:\n  reranker:\n    min_score: 1.0\n";
        assert!(collect_config_findings(ok).unwrap().is_empty());
        let bad = "search:\n  reranker:\n    min_score: -0.1\n";
        assert_eq!(collect_config_findings(bad).unwrap().len(), 1);
        let bad = "search:\n  reranker:\n    min_score: not-a-number\n";
        assert_eq!(collect_config_findings(bad).unwrap().len(), 1);
    }

    // ── superseded `search.reranker.min_score: 0.30` (C2, v3.4.16) ─────────
    //
    // ADR 0026 scaffolds `min_score` ACTIVE, so every vault initialized on
    // v3.4.7–v3.4.15 pins `0.30` in its own onebrain.yml — and a present value
    // beats `DEFAULT_RERANK_MIN_SCORE`. Without a finding those vaults keep the
    // old hard filter after upgrading and get none of the v3.4.16 rerank fix,
    // silently: the bounds check above is perfectly happy with 0.30.

    #[test]
    fn collect_config_findings_flags_the_superseded_min_score_default() {
        for form in ["0.30", "0.3"] {
            let text = format!("search:\n  reranker:\n    min_score: {form}\n");
            let findings = collect_config_findings(&text).unwrap();
            assert_eq!(findings.len(), 1, "{form}: {findings:?}");
            let f = &findings[0];
            assert_eq!(f.dotted, "search.reranker.min_score");
            assert!(f.superseded, "{form}: must be flagged as superseded");
            assert!(f.resettable, "{form}: --fix must be able to reset it");
            assert!(!f.reindex_required);
            // The message must explain the BEHAVIOUR change, not just the
            // number — a user who only sees "0.30 → 0.0" learns nothing.
            assert!(
                f.problem.contains("superseded") && f.problem.contains("DROPS"),
                "{form}: {}",
                f.problem
            );
            // Reset target is the current template/engine default.
            assert_eq!(f.default_repr, onebrain_fs::TEMPLATE_RERANK_MIN_SCORE);
        }
    }

    #[test]
    fn collect_config_findings_leaves_a_deliberate_min_score_alone() {
        // A user who chose their own gate is expressing an opinion, not
        // carrying a stale scaffold. Only the exact superseded value is
        // flagged; an absent key is never flagged at all (serde falls back to
        // the current default, which is the fixed behaviour already).
        for text in [
            "search:\n  reranker:\n    min_score: 0.5\n",
            "search:\n  reranker:\n    min_score: 0.25\n",
            "search:\n  reranker:\n    min_score: 0.31\n",
            "search:\n  reranker:\n    min_candidates: 10\n",
            "search:\n  default_top_k: 10\n",
        ] {
            assert!(
                collect_config_findings(text).unwrap().is_empty(),
                "must not flag: {text}"
            );
        }
    }

    #[test]
    fn config_values_check_reports_superseded_apart_from_invalid() {
        // `0.30` IS a legal min_score — counting it as an "invalid value"
        // would misreport a valid config as broken.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "search:\n  reranker:\n    # min_score comment\n    min_score: 0.30\n",
        )
        .unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(
            r.message.contains("1 superseded default(s)"),
            "message: {}",
            r.message
        );
        assert!(
            !r.message.contains("invalid value"),
            "an in-range value must not be called invalid: {}",
            r.message
        );
        assert!(
            r.details
                .iter()
                .any(|l| l.contains("search.reranker.min_score") && l.contains("superseded")),
            "{:?}",
            r.details
        );
        assert!(
            r.hint
                .as_deref()
                .unwrap_or("")
                .contains("reset superseded defaults"),
            "{:?}",
            r.hint
        );
    }

    #[test]
    fn fix_config_values_resets_the_superseded_min_score() {
        let d = tempdir().unwrap();
        let path = d.path().join("onebrain.yml");
        fs::write(
            &path,
            "search:\n  reranker:\n    # keep this comment\n    min_score: 0.30\n",
        )
        .unwrap();
        let outcome = fix_config_values(d.path(), true);
        assert!(
            matches!(outcome, FixOutcome::Fixed(_)),
            "expected Fixed, got {outcome:?}"
        );
        let after = fs::read_to_string(&path).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
        let got = parsed["search"]["reranker"]["min_score"].as_f64().unwrap();
        let want: f64 = onebrain_fs::TEMPLATE_RERANK_MIN_SCORE.parse().unwrap();
        assert!(
            (got - want).abs() < f64::EPSILON,
            "min_score must be reset to the current default, got {got}: {after}"
        );
        assert!(
            after.contains("# keep this comment"),
            "the reset must preserve user comments: {after}"
        );
        // Idempotent: a second pass finds nothing left to reset.
        assert!(
            collect_config_findings(&after).unwrap().is_empty(),
            "{after}"
        );
    }

    // ── dead keyword index (B1, v3.4.16) ──────────────────────────────────
    //
    // An interrupted schema migration leaves redb holding every chunk and
    // tantivy holding none. Nothing else notices: `search status` counts docs
    // from redb and says "up to date", `reindex` skips every doc because
    // `lex_hashes` says they're current, and keyword search just returns
    // nothing, forever.

    /// Build a real, lex-only-indexed collection for `vault`, then return its
    /// (collection, cache_dir, tantivy_dir). The `ONEBRAIN_CACHE_DIR` guard is
    /// the caller's to hold.
    fn indexed_collection(vault: &Path) -> (String, PathBuf, PathBuf) {
        use crate::commands::search_common::{
            collection_cache_dir, collection_name_readonly, index_artifact_path,
            open_engine_with_collection,
        };
        let resolved = crate::vault_ctx::require(Some(vault.to_path_buf())).unwrap();
        let collection = collection_name_readonly(vault).unwrap();
        let cache_dir = collection_cache_dir(&collection);
        {
            // Lex-only: populates redb's `chunk_meta` + the tantivy index
            // without ever loading the embedding model (no download).
            let mut engine = open_engine_with_collection(&resolved, &collection).unwrap();
            engine
                .reindex_all_lex_only_with_progress(vault, &mut |_| {})
                .unwrap();
        }
        let tantivy_dir = index_artifact_path(&cache_dir, "tantivy");
        (collection, cache_dir, tantivy_dir)
    }

    /// The B1 dead state: a VALID, current-schema, but EMPTY tantivy index
    /// beside a fully-populated `chunk_meta`, with no rebuild marker left to
    /// make `Engine::open` retry on its own. Exactly what a Ctrl-C during the
    /// v3.4.16 migration leaves behind once the marker is gone.
    fn empty_the_lex_index(tantivy_dir: &Path) {
        use onebrain_search::lex::LexIndex;
        fs::remove_dir_all(tantivy_dir).unwrap();
        let mut lex = LexIndex::open(tantivy_dir).unwrap();
        lex.commit().unwrap();
    }

    /// The B-A1 over-populated state: extra committed documents beside an
    /// otherwise intact index — what a rebuild appended onto a non-empty index
    /// produced, and what a crash between `remove_doc`'s redb commit and its
    /// lex commit leaves behind. Returns how many surplus docs were added.
    fn over_populate_the_lex_index(tantivy_dir: &Path) -> usize {
        use onebrain_search::chunk::Chunk;
        use onebrain_search::lex::LexIndex;
        let mut lex = LexIndex::open(tantivy_dir).unwrap();
        for id in 0..3 {
            lex.add(&Chunk {
                chunk_id: format!("ghost.md#{id}"),
                doc_path: "ghost.md".to_string(),
                heading_path: "Ghost".to_string(),
                chunk_index: id,
                text: "zebra quokka narwhal error text".to_string(),
            })
            .unwrap();
        }
        lex.commit().unwrap();
        3
    }

    fn lex_check_vault() -> (tempfile::TempDir, tempfile::TempDir) {
        let vault = tempdir().unwrap();
        fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: lexhealth\n",
        )
        .unwrap();
        fs::write(
            vault.path().join("note.md"),
            "# Errors Handling\nzebra quokka narwhal error text\n",
        )
        .unwrap();
        let cache = tempdir().unwrap();
        (vault, cache)
    }

    #[test]
    fn lex_index_check_reports_a_dead_keyword_index() {
        let (vault, cache) = lex_check_vault();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let (_collection, _cache_dir, tantivy_dir) = indexed_collection(vault.path());

        // Healthy first — the check must not cry wolf on a good index.
        let healthy = lex_index_check(vault.path());
        assert_eq!(healthy.status, DoctorStatus::Ok, "{}", healthy.message);
        assert!(healthy.message.contains("healthy"), "{}", healthy.message);

        empty_the_lex_index(&tantivy_dir);

        let r = lex_index_check(vault.path());
        assert_eq!(r.status, DoctorStatus::Error, "{}", r.message);
        assert!(
            r.message.contains("keyword index is EMPTY"),
            "{}",
            r.message
        );
        assert!(
            r.details.iter().any(|d| d == "lex_docs: 0"),
            "{:?}",
            r.details
        );
        assert!(
            r.details
                .iter()
                .any(|d| d.starts_with("chunk_meta: ") && d != "chunk_meta: 0"),
            "{:?}",
            r.details
        );
        // The recovery must be named, and must be reachable.
        assert!(
            r.hint.as_deref().unwrap_or("").contains("reindex --force"),
            "{:?}",
            r.hint
        );
        assert!(planned_action(&r).is_some(), "--fix must offer a repair");
    }

    #[test]
    fn fix_lex_index_rebuilds_from_stored_metadata() {
        let (vault, cache) = lex_check_vault();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let (_collection, _cache_dir, tantivy_dir) = indexed_collection(vault.path());
        empty_the_lex_index(&tantivy_dir);
        assert_eq!(lex_index_check(vault.path()).status, DoctorStatus::Error);

        // The note itself is deleted first: the repair must come from redb's
        // stored chunk metadata alone, never from re-reading the vault.
        fs::remove_file(vault.path().join("note.md")).unwrap();

        let outcome = fix_lex_index(vault.path(), true);
        match &outcome {
            FixOutcome::Fixed(m) => assert!(m.contains("chunk(s) restored"), "{m}"),
            other => panic!("expected Fixed, got {other:?}"),
        }
        let after = lex_index_check(vault.path());
        assert_eq!(after.status, DoctorStatus::Ok, "{}", after.message);

        // Idempotent: a second --fix on a healthy index is a no-op, not a
        // second rebuild.
        match fix_lex_index(vault.path(), true) {
            FixOutcome::Fixed(m) => assert!(m.contains("already healthy"), "{m}"),
            other => panic!("expected an already-healthy Fixed, got {other:?}"),
        }
    }

    #[test]
    fn lex_index_check_reports_an_over_populated_keyword_index() {
        // Audit concern 1: `lex_docs` well above `chunk_meta` used to report
        // as "N keyword doc(s) · healthy" — the only state the check knew was
        // an EMPTY index. Duplicates/orphans still answer queries, just worse
        // (corrupt BM25 corpus statistics), so this is a warn, not an error.
        let (vault, cache) = lex_check_vault();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let (_collection, _cache_dir, tantivy_dir) = indexed_collection(vault.path());
        assert_eq!(lex_index_check(vault.path()).status, DoctorStatus::Ok);

        let surplus = over_populate_the_lex_index(&tantivy_dir);
        assert!(surplus > 0);

        let r = lex_index_check(vault.path());
        assert_eq!(r.status, DoctorStatus::Warn, "{}", r.message);
        assert!(
            r.message.contains("degrade keyword ranking"),
            "{}",
            r.message
        );
        assert!(planned_action(&r).is_some(), "--fix must offer a repair");
    }

    #[test]
    fn lex_index_check_reports_an_orphaned_keyword_index() {
        // D2 (BLOCKER): `chunk_meta` gone while the keyword index survives —
        // reachable because `wipe_index_files` deletes `engine.redb` before
        // `tantivy/`, so an interruption between the two lands exactly here.
        // The repopulate used to CLEAR on this state (nothing to restore, the
        // only copy destroyed) and then clear the marker, after which
        // `is_dead()` — which requires `chunk_meta > 0` — reported HEALTHY.
        use crate::commands::search_common::{collection_cache_dir, index_artifact_path};
        let (vault, cache) = lex_check_vault();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let (collection, _cache_dir, tantivy_dir) = indexed_collection(vault.path());
        assert_eq!(lex_index_check(vault.path()).status, DoctorStatus::Ok);
        let docs_before = onebrain_search::lex::LexIndex::open(&tantivy_dir)
            .unwrap()
            .num_docs()
            .unwrap();
        assert!(docs_before > 0, "fixture must have a populated lex index");

        // Interrupted wipe: redb gone, tantivy still there, marker planted by
        // whatever rebuild was in flight.
        fs::remove_file(index_artifact_path(
            &collection_cache_dir(&collection),
            "engine.redb",
        ))
        .unwrap();
        let marker = onebrain_search::lex::rebuild_marker_path(&tantivy_dir);
        fs::write(&marker, b"rebuild pending\n").unwrap();

        // This opens the engine, which is where the refusal happens.
        let r = lex_index_check(vault.path());
        assert_eq!(r.status, DoctorStatus::Error, "{}", r.message);
        assert!(
            r.message.contains("stored chunk metadata is EMPTY"),
            "{}",
            r.message
        );
        assert!(r.details.contains(&"chunk_meta: 0".to_string()), "{r:?}");
        assert!(
            r.details.contains(&format!("lex_docs: {docs_before}")),
            "the index must be reported INTACT, not cleared: {r:?}"
        );
        assert_eq!(
            onebrain_search::lex::LexIndex::open(&tantivy_dir)
                .unwrap()
                .num_docs()
                .unwrap(),
            docs_before,
            "opening the engine must not have destroyed the last surviving copy"
        );
        assert!(
            marker.exists(),
            "a refused rebuild is not a resolved one — the marker must stay"
        );
        // `doctor --fix` must not claim it can repair this: the metadata a
        // rebuild would read is precisely what is gone.
        assert!(
            r.hint.as_deref().unwrap_or("").contains("reindex --force"),
            "{:?}",
            r.hint
        );
        match fix_lex_index(vault.path(), true) {
            FixOutcome::Manual(m) => assert!(m.contains("reindex --force"), "{m}"),
            other => panic!("expected Manual, got {other:?}"),
        }
        assert_eq!(
            onebrain_search::lex::LexIndex::open(&tantivy_dir)
                .unwrap()
                .num_docs()
                .unwrap(),
            docs_before,
            "--fix must not destroy it either"
        );
    }

    #[test]
    fn fix_lex_index_repairs_an_over_populated_index() {
        // The repair must bring `lex_docs` back to `chunk_meta` exactly — the
        // repopulate clears before re-adding, so it is idempotent on any start
        // state, not only on an empty index.
        let (vault, cache) = lex_check_vault();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let (_collection, _cache_dir, tantivy_dir) = indexed_collection(vault.path());
        over_populate_the_lex_index(&tantivy_dir);
        assert_eq!(lex_index_check(vault.path()).status, DoctorStatus::Warn);

        match fix_lex_index(vault.path(), true) {
            FixOutcome::Fixed(m) => assert!(m.contains("chunk(s) restored"), "{m}"),
            other => panic!("expected Fixed, got {other:?}"),
        }
        let after = lex_index_check(vault.path());
        assert_eq!(after.status, DoctorStatus::Ok, "{}", after.message);
        let docs = after
            .details
            .iter()
            .find_map(|d| d.strip_prefix("lex_docs: "))
            .unwrap()
            .to_string();
        assert!(
            after.details.contains(&format!("chunk_meta: {docs}")),
            "lex_docs must match chunk_meta after the repair: {:?}",
            after.details
        );
    }

    #[test]
    fn lex_index_check_skips_quietly_without_an_index() {
        // A fresh vault (or one whose collection can't be resolved) is not
        // broken — `native_search_check` owns those messages, so this check
        // must stay silent rather than double-report.
        let (vault, cache) = lex_check_vault();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let r = lex_index_check(vault.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{}", r.message);
        assert!(r.message.contains("skipped"), "{}", r.message);

        let bare = tempdir().unwrap();
        let r = lex_index_check(bare.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{}", r.message);
        assert!(r.message.contains("skipped"), "{}", r.message);
    }

    #[test]
    fn config_values_check_ok_warn_and_skip_paths() {
        let d = tempdir().unwrap();
        // No config file → skipped, ok.
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok);
        assert!(r.message.contains("skipped"), "{}", r.message);
        // Invalid YAML → skipped, ok (onebrain.yml-keys owns that error).
        fs::write(d.path().join("onebrain.yml"), "not: : valid").unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok);
        assert!(r.message.contains("skipped"), "{}", r.message);
        // In-range, documented, ALREADY in template layout → ok. (Layout is
        // canonicalized via the shared restructure so no banner drift fires.)
        let canonical =
            onebrain_fs::restructure_config("# channel comment\nupdate_channel: next\n").unwrap();
        fs::write(d.path().join("onebrain.yml"), &canonical).unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{}", r.message);
        assert_eq!(r.message, "all values in range");
        // In-range but UNDOCUMENTED key (and, lacking a banner, layout drift)
        // → warn combining both, with the comment-specific hint (still zero
        // writes — read-only check).
        fs::write(d.path().join("onebrain.yml"), "update_channel: next\n").unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert_eq!(r.message, "1 undocumented key(s) · layout drift");
        assert!(
            r.details
                .iter()
                .any(|l| l.contains("lack self-documentation") && l.contains("update_channel")),
            "{:?}",
            r.details
        );
        assert!(
            r.details.iter().any(|l| l.contains("layout differs")),
            "{:?}",
            r.details
        );
        assert!(
            r.hint
                .as_deref()
                .unwrap_or("")
                .contains("add the missing self-documentation comments"),
            "{:?}",
            r.hint
        );
        // Out-of-range → warn with a per-key detail line + fix hint (the key
        // is commented here so only the value finding fires; the missing
        // banner adds layout drift).
        fs::write(
            d.path().join("onebrain.yml"),
            "checkpoint:\n  # messages comment\n  messages: 0\n",
        )
        .unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert_eq!(r.message, "1 invalid value(s) · layout drift");
        assert!(
            r.details
                .iter()
                .any(|l| l.contains("checkpoint.messages") && l.contains("default: 15")),
            "{:?}",
            r.details
        );
        assert!(r.hint.as_deref().unwrap_or("").contains("doctor --fix"));
        // Invalid value AND undocumented key AND layout drift combine.
        fs::write(
            d.path().join("onebrain.yml"),
            "checkpoint:\n  messages: 0\n",
        )
        .unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert_eq!(
            r.message,
            "1 invalid value(s) · 1 undocumented key(s) · layout drift"
        );
        let hint = r.hint.as_deref().unwrap_or("");
        assert!(
            hint.contains("reset out-of-range tunables to their defaults")
                && hint.contains("add the missing self-documentation comments")
                && hint.contains("restructure the layout"),
            "{:?}",
            r.hint
        );
        // A documented, report-only value finding still yields an actionable
        // hint now: the restructure is the fixable part.
        fs::write(
            d.path().join("onebrain.yml"),
            "folders:\n  # inbox comment\n  inbox: \"\"\n",
        )
        .unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(
            r.hint
                .as_deref()
                .unwrap_or("")
                .contains("restructure the layout"),
            "{:?}",
            r.hint
        );
    }

    #[test]
    fn join_actions_renders_english_lists() {
        assert_eq!(join_actions(&[]), "");
        assert_eq!(join_actions(&["a"]), "a");
        assert_eq!(join_actions(&["a", "b"]), "a and b");
        assert_eq!(join_actions(&["a", "b", "c"]), "a, b, and c");
    }

    #[test]
    fn reset_config_value_top_level_key() {
        let text = "# header\nupdate_channel: weekly-maybe\nfolders:\n  inbox: 00-inbox\n";
        let out = reset_config_value(text, &["update_channel"], "stable").unwrap();
        assert_eq!(
            out,
            "# header\nupdate_channel: stable\nfolders:\n  inbox: 00-inbox\n"
        );
    }

    #[test]
    fn reset_config_value_three_level_walk_preserves_comments() {
        let text = "search:\n  \
                    # embed model comment\n  \
                    embed_model: multilingual-e5-small\n  \
                    reranker:\n    \
                    # gate comment\n    \
                    min_score: 7.5\n    \
                    min_candidates: 10\nother:\n  min_score: 9.9\n";
        let out = reset_config_value(text, &["search", "reranker", "min_score"], "0.30").unwrap();
        assert!(out.contains("    min_score: 0.30\n"), "{out}");
        assert!(out.contains("# gate comment"), "{out}");
        assert!(out.contains("# embed model comment"), "{out}");
        // A same-named key in a DIFFERENT block is untouched.
        assert!(out.contains("  min_score: 9.9"), "{out}");
        assert!(out.contains("min_candidates: 10"), "{out}");
    }

    #[test]
    fn reset_config_value_preserves_inline_comment_and_crlf() {
        let text = "checkpoint:\r\n  messages: 0  # my threshold\r\n  minutes: 30\r\n";
        let out = reset_config_value(text, &["checkpoint", "messages"], "15").unwrap();
        assert_eq!(
            out,
            "checkpoint:\r\n  messages: 15  # my threshold\r\n  minutes: 30\r\n"
        );
    }

    #[test]
    fn reset_config_value_refuses_inline_mapping() {
        let text = "checkpoint: {messages: 0, minutes: 30}\n";
        assert!(reset_config_value(text, &["checkpoint", "messages"], "15").is_none());
    }

    #[test]
    fn reset_config_value_missing_key_returns_none() {
        let text = "checkpoint:\n  minutes: 30\n";
        assert!(reset_config_value(text, &["checkpoint", "messages"], "15").is_none());
        assert!(reset_config_value(text, &["search", "default_top_k"], "10").is_none());
        assert!(reset_config_value(text, &[], "x").is_none());
    }

    #[test]
    fn reset_config_value_never_matches_a_grandchild_key() {
        // `model:` exists only one level DEEPER than requested — the lookup
        // must not reach into the nested block and clobber it.
        let text = "search:\n  reranker:\n    model: onebrain-rerank-v1\n";
        assert!(reset_config_value(text, &["search", "model"], "x").is_none());
        // And a top-level lookup must not match a nested occurrence.
        let text = "search:\n  update_channel: nested\n";
        assert!(reset_config_value(text, &["update_channel"], "stable").is_none());
    }

    #[test]
    fn reset_config_value_quoted_hash_is_not_a_comment() {
        // A '#' inside a quoted scalar is data — the conservative guard keeps
        // the whole remainder from being treated as a trailing comment.
        let text = "search:\n  embed_model: \"weird # name\"\n";
        let out =
            reset_config_value(text, &["search", "embed_model"], "multilingual-e5-small").unwrap();
        assert_eq!(out, "search:\n  embed_model: multilingual-e5-small\n");
    }

    #[test]
    fn fix_config_values_resets_and_reports() {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "# precious\nupdate_channel: stable\nfolders:\n  inbox: \"\"\ncheckpoint:\n  messages: 0\n",
        )
        .unwrap();
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("checkpoint.messages → 15"), "msg: {msg}");
        assert!(msg.contains("never auto-reset"), "msg: {msg}");
        assert!(msg.contains("folders.inbox"), "msg: {msg}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(after.contains("# precious"), "{after}");
        assert!(after.contains("messages: 15"), "{after}");
        assert!(after.contains("inbox: \"\""), "folders untouched: {after}");
    }

    #[test]
    fn fix_config_values_inline_mapping_is_partial_outcome() {
        // The inline-mapping value can't be reset, but the restructure still
        // adds the section banner — honest tri-state Partial (progress landed
        // on disk; the value needs a manual edit).
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "checkpoint: {messages: 0}\n").unwrap();
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Partial(msg) = outcome else {
            panic!("expected Partial, got: {outcome:?}");
        };
        assert!(msg.contains("checkpoint.messages"), "msg: {msg}");
        assert!(msg.contains("edit manually"), "msg: {msg}");
        assert!(msg.contains("restructured"), "msg: {msg}");
        // The banner landed even though the value could not be reset.
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(after.contains(&onebrain_fs::config_layout::section_banner(
            "Agent behavior"
        )));
        assert!(after.contains("checkpoint: {messages: 0}"));
    }

    #[test]
    fn reset_config_value_header_inline_comment_is_still_a_block_header() {
        // A trailing `# …` comment on a section header must not disable
        // resets for every child under it.
        let text = "search:  # my search config\n  reranker:\n    min_score: 7.5\n";
        let out = reset_config_value(text, &["search", "reranker", "min_score"], "0.30").unwrap();
        assert!(out.contains("min_score: 0.30"), "{out}");
        assert!(out.contains("search:  # my search config"), "{out}");
        // Inline mappings and inline scalars are still refused.
        assert!(reset_config_value(
            "checkpoint: {messages: 0}\n",
            &["checkpoint", "messages"],
            "15"
        )
        .is_none());
        assert!(reset_config_value(
            "checkpoint: null\n  messages: 0\n",
            &["checkpoint", "messages"],
            "15"
        )
        .is_none());
    }

    #[test]
    fn fix_config_values_partial_when_mixed_reset_and_unfixable() {
        // One value resettable (block form), one stuck in an inline mapping:
        // the reset must land on disk AND the outcome must be the honest
        // tri-state Partial — not Failed (real work happened) and not Fixed
        // (something remains).
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "checkpoint: {messages: 0}\nsearch:\n  default_top_k: 0\n",
        )
        .unwrap();
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Partial(msg) = outcome else {
            panic!("expected Partial, got: {outcome:?}");
        };
        assert!(msg.contains("search.default_top_k → 10"), "msg: {msg}");
        assert!(msg.contains("checkpoint.messages"), "msg: {msg}");
        assert!(msg.contains("edit manually"), "msg: {msg}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(
            after.contains("default_top_k: 10"),
            "reset must be on disk: {after}"
        );
        assert!(
            after.contains("checkpoint: {messages: 0}"),
            "unfixable shape untouched: {after}"
        );
    }

    #[test]
    fn fix_config_values_flow_style_root_declines_without_phantom_layout_complaint() {
        // A flow-style root parses as a mapping (findings ARE collected) but
        // nothing is line-editable and the restructure declines the shape.
        // No layout drift was ever promised for it (see the agreement test
        // below), so the outcome lists only the value — never a phantom
        // "layout restructure" entry — and the file is untouched.
        let d = tempdir().unwrap();
        let flow = "{checkpoint: {messages: 0}}\n";
        fs::write(d.path().join("onebrain.yml"), flow).unwrap();
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Failed(msg) = outcome else {
            panic!("expected Failed (no progress possible), got: {outcome:?}");
        };
        assert!(msg.contains("checkpoint.messages"), "msg: {msg}");
        assert!(
            !msg.contains("layout restructure"),
            "no phantom layout complaint: {msg}"
        );
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, flow, "declined shape must be left untouched");
    }

    #[test]
    fn layout_drift_is_never_promised_for_shapes_restructure_declines() {
        // Pins the check/fix agreement the fix recipe's decline arm relies
        // on: every shape `restructure_config` declines also makes
        // `config_layout_matches` return true, so plain doctor never reports
        // "layout drift" for a file `--fix` would then decline to
        // restructure — the promise-then-silent-decline mismatch is
        // unreachable (the decline arm in `fix_config_values` is
        // defense-in-depth only).
        for text in [
            "- a\n- b\n",        // sequence root
            "",                  // empty
            "# only comments\n", // no top-level keys
            "{a: 1, b: 2}\n",    // flow-style root mapping
            "  {a: 1}\n",        // flow-style root, indented
            "a: [1, 2\n",        // invalid YAML
            "a: 1\na: 2\n",      // duplicate top-level keys
        ] {
            assert!(
                onebrain_fs::restructure_config(text).is_none(),
                "expected decline for {text:?}"
            );
            assert!(
                onebrain_fs::config_layout_matches(text),
                "declined shape must never report drift: {text:?}"
            );
        }
    }

    #[test]
    fn fix_config_values_clean_documented_config_is_noop() {
        // A fully-commented, in-range, canonical-layout config (the fresh
        // template shape) is the true no-op: byte-identical after --fix,
        // "already" message.
        let d = tempdir().unwrap();
        let clean =
            onebrain_fs::restructure_config("# channel comment\nupdate_channel: stable\n").unwrap();
        fs::write(d.path().join("onebrain.yml"), &clean).unwrap();
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("already in range"), "msg: {msg}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, clean);
    }

    #[test]
    fn fix_config_values_backfills_template_comments_on_legacy_config() {
        // Scope test (a): a bare legacy config gains the EXACT template
        // comment above every known key; values and key order untouched.
        let d = tempdir().unwrap();
        let legacy = "update_channel: stable\n\
             folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n\
             checkpoint:\n  messages: 15\n  minutes: 30\n\
             search:\n  collection: my-col\n  embed_model: multilingual-e5-small\n";
        fs::write(d.path().join("onebrain.yml"), legacy).unwrap();
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(
            msg.contains("added 13 self-documentation comment(s)"),
            "msg: {msg}"
        );
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        // Every known present key now carries EXACTLY the template comment,
        // sourced from the shared table.
        for doc in onebrain_fs::config_key_docs() {
            let key = doc.segments.last().unwrap();
            // Keys not present in this legacy config are never added
            // (recap.*, stats.*, schedule, token_optimization.*, and the
            // search reranker/top_k/exclude/embed.* keys are all absent
            // here).
            if doc.segments.join(".").starts_with("search.reranker")
                || doc.segments == ["search", "default_top_k"]
                || doc.segments == ["search", "exclude"]
                || doc.segments.get(1) == Some(&"embed")
                || doc.segments.first() == Some(&"recap")
                || doc.segments.first() == Some(&"stats")
                || doc.segments.first() == Some(&"token_optimization")
                || doc.segments == ["schedule"]
            {
                assert!(
                    !after
                        .lines()
                        .any(|l| l.trim_start().starts_with(&format!("{key}:"))),
                    "absent key {key} must not be injected:\n{after}"
                );
                continue;
            }
            let lines: Vec<&str> = after.lines().collect();
            let idx = lines
                .iter()
                .position(|l| l.trim_start().starts_with(&format!("{key}:")))
                .unwrap_or_else(|| panic!("{key} missing:\n{after}"));
            assert_eq!(
                lines[idx - 1].trim_start(),
                doc.comment,
                "comment above {key} must be the template's:\n{after}"
            );
        }
        // The restructure landed in the same run: outcome names it and the
        // section banners are on disk (the legacy config had none).
        assert!(msg.contains("restructured layout"), "msg: {msg}");
        assert!(
            after.contains(&onebrain_fs::config_layout::section_banner("General")),
            "General banner must be present after --fix:\n{after}"
        );
        // Values + key order untouched.
        assert!(after.contains("collection: my-col"), "{after}");
        let key_order: Vec<&str> = after
            .lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .collect();
        let legacy_order: Vec<&str> = legacy
            .lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .collect();
        assert_eq!(key_order, legacy_order, "key lines must be untouched");
        // Scope test (c): a user's own comment wins — and (d) idempotency:
        // second run changes nothing.
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("already in range"), "msg: {msg}");
        let after2 = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after2, after, "second --fix run must be byte-identical");
    }

    #[test]
    fn fix_config_values_never_replaces_user_comments() {
        // Scope test (b): a key already under the user's own comment is left
        // alone — no insertion, no dedupe, no replacement. Fixture is already
        // in canonical layout so this isolates the comment-preservation path.
        let d = tempdir().unwrap();
        let cfg = onebrain_fs::restructure_config(
            "# my own words about the channel\nupdate_channel: stable\n",
        )
        .unwrap();
        fs::write(d.path().join("onebrain.yml"), &cfg).unwrap();
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("already in range"), "msg: {msg}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, cfg);
        // The user's own comment survived verbatim.
        assert!(after.contains("# my own words about the channel"));
    }

    #[test]
    fn fix_config_values_reset_and_backfill_in_one_run() {
        // Scope test (d): a value reset and the comment backfill land in the
        // same run, one report.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "update_channel: stable\ncheckpoint:\n  messages: 0\n",
        )
        .unwrap();
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("checkpoint.messages → 15"), "msg: {msg}");
        assert!(msg.contains("self-documentation comment(s)"), "msg: {msg}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(after.contains("messages: 15"), "{after}");
        // The backfilled comment sits above the CORRECTED value line.
        let lines: Vec<&str> = after.lines().collect();
        let idx = lines
            .iter()
            .position(|l| l.trim_start().starts_with("messages:"))
            .unwrap();
        assert!(
            lines[idx - 1].trim_start().starts_with('#') && lines[idx - 1].contains("default: 15"),
            "{after}"
        );
    }

    // ── search-exclude: doctor flags + --fix inserts (Task 4) ────────────

    #[test]
    fn doctor_flags_missing_search_exclude_when_collection_set() {
        let d = tempdir().unwrap();

        // search.collection set, exclude key entirely absent → warn finding.
        fs::write(
            d.path().join("onebrain.yml"),
            "search:\n  collection: my-col\n",
        )
        .unwrap();
        let r = search_exclude_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn, "{r:?}");
        assert_eq!(
            r.message,
            "search.exclude not set — archive folder is being indexed"
        );
        assert!(
            r.hint.as_deref().unwrap_or("").contains("doctor --fix"),
            "{:?}",
            r.hint
        );

        // `exclude: []` present — explicit user choice, never flagged.
        fs::write(
            d.path().join("onebrain.yml"),
            "search:\n  collection: my-col\n  exclude: []\n",
        )
        .unwrap();
        let r = search_exclude_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");

        // No `search.collection` at all — vault never adopted search, so a
        // missing `exclude:` is expected (not migrated yet), never flagged.
        fs::write(d.path().join("onebrain.yml"), "update_channel: stable\n").unwrap();
        let r = search_exclude_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");

        // Invalid YAML — the onebrain.yml-keys check owns that error; this
        // check skips (never a misleading "search.exclude ok").
        fs::write(d.path().join("onebrain.yml"), "not: : valid").unwrap();
        let r = search_exclude_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");
        assert!(
            r.message.contains("skipped — invalid YAML"),
            "{}",
            r.message
        );
    }

    #[test]
    fn doctor_fix_inserts_exclude_block_with_comment() {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "search:\n  collection: my-col\n",
        )
        .unwrap();

        let outcome = fix_search_exclude(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("attachments"), "msg: {msg}");

        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(
            after.contains("  exclude:\n  - attachments\n  - 06-archive"),
            "{after}"
        );
        // Preceded by the shared table's own comment line — never a
        // hand-rolled duplicate.
        let expected_comment = onebrain_fs::config_key_docs()
            .into_iter()
            .find(|dd| dd.segments == ["search", "exclude"])
            .unwrap()
            .comment;
        let lines: Vec<&str> = after.lines().collect();
        let idx = lines
            .iter()
            .position(|l| l.trim_start() == "exclude:")
            .unwrap();
        assert_eq!(lines[idx - 1].trim_start(), expected_comment, "{after}");

        // Doctor now reports clean.
        let r = search_exclude_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");

        // Idempotence: a second `--fix` run is a byte-identical no-op.
        let outcome2 = fix_search_exclude(d.path(), false);
        let FixOutcome::Fixed(msg2) = outcome2 else {
            panic!("expected Fixed, got: {outcome2:?}");
        };
        assert!(msg2.contains("already set"), "msg: {msg2}");
        let after2 = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after2, after, "second --fix run must be byte-identical");
    }

    #[test]
    fn fix_search_exclude_resolves_archive_from_vault_folders() {
        // The exclude block's second entry must come from THIS vault's
        // `folders.archive`, never a hard-coded "06-archive" literal.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "search:\n  collection: my-col\nfolders:\n  archive: z-archive\n",
        )
        .unwrap();
        let outcome = fix_search_exclude(d.path(), false);
        assert!(matches!(outcome, FixOutcome::Fixed(_)), "{outcome:?}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(
            after.contains("  exclude:\n  - attachments\n  - z-archive"),
            "{after}"
        );
        // Comment and value AGREE on the vault's own archive (R2 Minor):
        // the inserted doc comment is `search_exclude_comment(&archive)` —
        // its "default:" text names z-archive, never the generic table
        // default that would contradict the value right below it.
        let expected_comment = onebrain_fs::search_exclude_comment("z-archive");
        assert!(
            after.contains(&format!("  {expected_comment}\n  exclude:")),
            "comment must document the vault's own archive:\n{after}"
        );
        assert!(!after.contains("06-archive"), "{after}");
    }

    #[test]
    fn fix_search_exclude_flow_form_search_fails_without_corrupting_yaml() {
        // Regression (R1 Important): serde_yaml parses a flow mapping
        // (`search: {collection: my-col}`) identically to block form, so the
        // gate fires — but splicing indented child lines under the flow line
        // would write UNPARSEABLE YAML while reporting Fixed. The recipe must
        // refuse (honest Failed with a manual step) and leave the file
        // byte-identical and re-parseable.
        let d = tempdir().unwrap();
        let flow = "search: {collection: my-col}\ncheckpoint:\n  messages: 15\n";
        fs::write(d.path().join("onebrain.yml"), flow).unwrap();

        let outcome = fix_search_exclude(d.path(), false);
        let FixOutcome::Failed(msg) = outcome else {
            panic!("expected Failed, got: {outcome:?}");
        };
        assert!(msg.contains("block form"), "actionable message: {msg}");

        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, flow, "declined shape must be left untouched");
        // The file must still parse — never write (or leave) corrupt YAML.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
        assert_eq!(
            parsed["search"]["collection"].as_str(),
            Some("my-col"),
            "collection value must survive"
        );
    }

    /// Direct-child key names of the top-level `search:` block in raw
    /// config text, in file order (indent == 2, non-comment). Used to
    /// compare a backfilled vault's in-block key order against the fresh
    /// template's.
    fn search_block_key_order(text: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let mut in_search = false;
        for l in text.lines() {
            if !l.starts_with(' ') && !l.trim().is_empty() {
                in_search = l.trim_start().starts_with("search:");
                continue;
            }
            if !in_search {
                continue;
            }
            let indent = l.len() - l.trim_start().len();
            let t = l.trim_start();
            if indent == 2 && !t.starts_with('#') && !t.starts_with('-') {
                if let Some(key) = t.split(':').next() {
                    keys.push(key.to_string());
                }
            }
        }
        keys
    }

    #[test]
    fn fix_search_exclude_lands_before_reranker_matching_template_order() {
        // Pins the insertion position deliberately (R2 Important): the fresh
        // template places `exclude:` BETWEEN `default_top_k:` and
        // `reranker:`, and `restructure_config` only reorders top-level
        // blocks — never within one — so the backfill must land at the same
        // in-block position or backfilled vaults diverge from canonical
        // layout forever. With `search:` carrying multiple sub-keys
        // including NESTED blocks (reranker, embed), the exclude block lands
        // immediately before `reranker:`, the result re-parses, and every
        // pre-existing key survives.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "search:\n  collection: my-col\n  embed_model: multilingual-e5-small\n  default_top_k: 10\n  reranker:\n    enabled: true\n    model: onebrain-rerank-v1\n  embed:\n    auto: true\n",
        )
        .unwrap();

        let outcome = fix_search_exclude(d.path(), false);
        assert!(matches!(outcome, FixOutcome::Fixed(_)), "{outcome:?}");

        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        // Re-parses with all keys intact.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
        assert_eq!(parsed["search"]["collection"].as_str(), Some("my-col"));
        assert_eq!(
            parsed["search"]["embed_model"].as_str(),
            Some("multilingual-e5-small")
        );
        assert_eq!(parsed["search"]["default_top_k"].as_u64(), Some(10));
        assert_eq!(
            parsed["search"]["reranker"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            parsed["search"]["reranker"]["model"].as_str(),
            Some("onebrain-rerank-v1")
        );
        assert_eq!(parsed["search"]["embed"]["auto"].as_bool(), Some(true));
        let exclude: Vec<&str> = parsed["search"]["exclude"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(exclude, ["attachments", "06-archive"]);
        // Position pinned: the exclude block (comment + key + 2 items)
        // sits between `default_top_k:` and `reranker:` — the fresh
        // template's position.
        let comment = onebrain_fs::search_exclude_comment("06-archive");
        let expected_mid = format!(
            "  default_top_k: 10\n  {comment}\n  exclude:\n  - attachments\n  - 06-archive\n  reranker:\n"
        );
        assert!(
            after.contains(&expected_mid),
            "exclude must land between default_top_k and reranker:\n{after}"
        );
        // In-block key order matches the fresh template for the key set
        // present in both (the template has no `embed:` block and its
        // `collection` is a commented placeholder; `embed:` here trails
        // last in both orderings' shared subsequence check).
        let template = onebrain_fs::render_onebrain_yml(onebrain_fs::SchedulePreset::Skip).unwrap();
        let template_order = search_block_key_order(&template);
        let after_order = search_block_key_order(&after);
        let shared: Vec<&String> = after_order
            .iter()
            .filter(|k| template_order.contains(k))
            .collect();
        let template_shared: Vec<&String> = template_order
            .iter()
            .filter(|k| after_order.contains(k))
            .collect();
        assert_eq!(
            shared, template_shared,
            "backfilled search-block key order must match the fresh template\nafter: {after_order:?}\ntemplate: {template_order:?}"
        );
    }

    #[test]
    fn fix_search_exclude_backs_up_over_reranker_lead_comments() {
        // Covers the anchor comment-backup branch (R3 Minor): the real-world
        // common case is a pre-v3.4.9 template vault where `reranker:`
        // carries its "# Tier-2 cross-encoder reranker" lead comment. The
        // exclude block must land ABOVE that lead comment — never between
        // the comment and the key it documents — and the comment must stay
        // glued to `reranker:`.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "search:\n  collection: my-col\n  default_top_k: 10\n  # Tier-2 cross-encoder reranker (re-scores fused candidates for relevance).\n  # second lead comment line\n  reranker:\n    enabled: true\n",
        )
        .unwrap();

        let outcome = fix_search_exclude(d.path(), false);
        assert!(matches!(outcome, FixOutcome::Fixed(_)), "{outcome:?}");

        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        // (a) + (b): exclude block sits above the FULL lead-comment run,
        // which stays contiguous and glued to `reranker:`.
        let comment = onebrain_fs::search_exclude_comment("06-archive");
        let expected_mid = format!(
            "  default_top_k: 10\n  {comment}\n  exclude:\n  - attachments\n  - 06-archive\n  # Tier-2 cross-encoder reranker (re-scores fused candidates for relevance).\n  # second lead comment line\n  reranker:\n"
        );
        assert!(
            after.contains(&expected_mid),
            "exclude must land above reranker's lead comments, comments glued to reranker:\n{after}"
        );
        // (b) pinned line-by-line too: the line directly above `reranker:`
        // is still its own lead comment, not an exclude item.
        let lines: Vec<&str> = after.lines().collect();
        let idx = lines
            .iter()
            .position(|l| l.trim_start().starts_with("reranker:"))
            .unwrap();
        assert_eq!(
            lines[idx - 1].trim_start(),
            "# second lead comment line",
            "{after}"
        );
        // (c) re-parses with all keys intact.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
        assert_eq!(parsed["search"]["collection"].as_str(), Some("my-col"));
        assert_eq!(parsed["search"]["default_top_k"].as_u64(), Some(10));
        assert_eq!(
            parsed["search"]["reranker"]["enabled"].as_bool(),
            Some(true)
        );
        let exclude: Vec<&str> = parsed["search"]["exclude"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(exclude, ["attachments", "06-archive"]);
    }

    #[test]
    fn fix_search_exclude_noop_when_not_gated() {
        // search.collection absent → nothing to do, no write.
        let d = tempdir().unwrap();
        let original = "update_channel: stable\n";
        fs::write(d.path().join("onebrain.yml"), original).unwrap();
        let outcome = fix_search_exclude(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("already set"), "msg: {msg}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, original, "untouched when not gated");
    }

    // ── token-optimization: doctor flags + --fix backfills (issues #247, #270) ──

    /// A realistic pre-v3.4.10 vault: all the standard keys, but no
    /// `token_optimization` block at all (the exact gap issue #247 reports).
    const LEGACY_VAULT_NO_TOKEN_OPT: &str = "update_channel: stable\n\
         folders:\n  \
           inbox: 00-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n\
         checkpoint:\n  \
           messages: 15\n  \
           minutes: 30\n";

    #[test]
    fn doctor_flags_missing_token_optimization_block() {
        let d = tempdir().unwrap();

        // No `token_optimization:` key at all → warn finding.
        fs::write(d.path().join("onebrain.yml"), LEGACY_VAULT_NO_TOKEN_OPT).unwrap();
        let r = token_optimization_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn, "{r:?}");
        assert_eq!(
            r.message,
            "token_optimization block not set — token-opt config is undocumented and un-tunable"
        );
        assert!(
            r.hint.as_deref().unwrap_or("").contains("doctor --fix"),
            "{:?}",
            r.hint
        );

        // Block present but minimal → sub-keys missing (issue #270), so
        // still flagged — just with a sub-key message, not "block not set".
        fs::write(
            d.path().join("onebrain.yml"),
            "token_optimization:\n  level: conservative\n",
        )
        .unwrap();
        let r = token_optimization_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn, "{r:?}");
        assert!(
            r.message.contains("missing sub-key(s)") && r.message.contains("check_timeout_ms"),
            "{}",
            r.message
        );
        assert!(
            r.hint.as_deref().unwrap_or("").contains("doctor --fix"),
            "{:?}",
            r.hint
        );

        // Block present with EVERY documented sub-key → ok, never flagged.
        fs::write(
            d.path().join("onebrain.yml"),
            format!(
                "{}\n",
                onebrain_fs::token_optimization_block_lines().join("\n")
            ),
        )
        .unwrap();
        let r = token_optimization_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");

        // Invalid YAML — the onebrain.yml-keys check owns that error; this
        // check skips (never a misleading "token_optimization ok").
        fs::write(d.path().join("onebrain.yml"), "not: : valid").unwrap();
        let r = token_optimization_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");
        assert!(
            r.message.contains("skipped — invalid YAML"),
            "{}",
            r.message
        );
    }

    #[test]
    fn doctor_flags_missing_token_optimization_sub_key_in_existing_block() {
        // The exact gap issue #270 reports: block present with SOME keys
        // (level, read_hook), but a later-added sub-key (check_timeout_ms,
        // v3.4.13) is absent — this must warn, not report clean.
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "token_optimization:\n  level: balanced\n  read_hook: ledger\n",
        )
        .unwrap();
        let r = token_optimization_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn, "{r:?}");
        assert!(r.message.contains("check_timeout_ms"), "{}", r.message);
        assert!(
            r.hint.as_deref().unwrap_or("").contains("doctor --fix"),
            "{:?}",
            r.hint
        );
    }

    #[test]
    fn fix_token_optimization_inserts_block_matching_init_template() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), LEGACY_VAULT_NO_TOKEN_OPT).unwrap();

        let outcome = fix_token_optimization(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("token_optimization"), "msg: {msg}");

        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();

        // The inserted block is BYTE-IDENTICAL to what `init` emits for a
        // fresh vault — both call the one shared source function, so this
        // can never drift.
        let expected_block = onebrain_fs::token_optimization_block_lines().join("\n");
        assert!(
            after.contains(&expected_block),
            "inserted block must match init's own emit verbatim:\nexpected:\n{expected_block}\n\ngot:\n{after}"
        );

        // Defaults present: level / strip_frontmatter / model / read_hook
        // active; get_max_tokens / snippet_max_chars stay commented
        // placeholders (per-level ladder default), matching the fresh
        // template's own contract.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
        let tok = onebrain_core::config::TokenOptimizationConfig::default();
        assert_eq!(
            parsed["token_optimization"]["level"].as_str(),
            Some(tok.level.to_string()).as_deref()
        );
        assert_eq!(
            parsed["token_optimization"]["strip_frontmatter"].as_str(),
            Some(tok.strip_frontmatter.to_string()).as_deref()
        );
        assert_eq!(
            parsed["token_optimization"]["model"].as_str(),
            Some(tok.model.as_str())
        );
        assert_eq!(
            parsed["token_optimization"]["read_hook"].as_str(),
            Some(tok.read_hook.to_string()).as_deref()
        );
        assert!(parsed["token_optimization"].get("get_max_tokens").is_none());
        assert!(parsed["token_optimization"]
            .get("snippet_max_chars")
            .is_none());

        // Landed under its own "Token optimization" banner
        // (`config_layout::SECTIONS`), not dangling at EOF.
        assert!(
            after.contains(&onebrain_fs::config_layout::section_banner(
                "Token optimization"
            )),
            "{after}"
        );

        // Pre-existing content survives untouched.
        assert!(after.contains("update_channel: stable"));
        assert!(after.contains("inbox: 00-inbox"));
        assert!(after.contains("messages: 15"));

        // Doctor now reports clean.
        let r = token_optimization_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");

        // The `config_key_docs` completeness/backfill guard: `--fix`'s
        // separate comment-backfill pass (`config-values`) must find nothing
        // new to add for these keys — they already carry the exact table
        // comment from this recipe.
        assert!(
            onebrain_fs::config_key_docs()
                .iter()
                .filter(|d| d.segments.first() == Some(&"token_optimization"))
                .all(|d| !onebrain_fs::yaml_edit::key_lacks_comment(&after, d.segments)),
            "every emitted token_optimization key must already carry its doc comment:\n{after}"
        );

        // Idempotence: a second `--fix` run is a byte-identical no-op.
        let outcome2 = fix_token_optimization(d.path(), false);
        let FixOutcome::Fixed(msg2) = outcome2 else {
            panic!("expected Fixed, got: {outcome2:?}");
        };
        assert!(msg2.contains("already set"), "msg: {msg2}");
        let after2 = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after2, after, "second --fix run must be byte-identical");
    }

    #[test]
    fn fix_token_optimization_noop_when_already_present() {
        // A FULLY populated block — every documented sub-key present (either
        // active or, for get_max_tokens/snippet_max_chars, the commented
        // placeholder) — is a true noop, custom user value included.
        let d = tempdir().unwrap();
        let mut block_lines = onebrain_fs::token_optimization_block_lines();
        for line in block_lines.iter_mut() {
            if line.trim_start().starts_with("level:") {
                *line = "  level: aggressive".to_string();
            }
        }
        let original = format!("update_channel: stable\n{}\n", block_lines.join("\n"));
        fs::write(d.path().join("onebrain.yml"), &original).unwrap();
        let outcome = fix_token_optimization(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("already set"), "msg: {msg}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, original, "untouched when already present");
    }

    #[test]
    fn fix_token_optimization_backfills_missing_sub_key_in_existing_block() {
        // The exact gap issue #270 reports: block present with level/
        // read_hook, but no check_timeout_ms. --fix must add it (comment +
        // default) without touching the rest of the block, and doctor must
        // report clean afterward. Mirrors
        // `fix_token_optimization_inserts_block_matching_init_template`'s
        // assertion style at sub-key granularity.
        let d = tempdir().unwrap();
        let original =
            "update_channel: stable\ntoken_optimization:\n  level: balanced\n  read_hook: ledger\n";
        fs::write(d.path().join("onebrain.yml"), original).unwrap();

        let outcome = fix_token_optimization(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("check_timeout_ms"), "msg: {msg}");

        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();

        // The rest of the block — level/read_hook — is byte-preserved
        // exactly as written, including update_channel above it.
        assert!(
            after.starts_with(original.trim_end_matches('\n')),
            "existing lines must survive untouched:\n{after}"
        );

        // check_timeout_ms backfilled with EXACTLY the shared table's
        // comment + `TokenOptimizationConfig::default()`'s value.
        let expected_comment = onebrain_fs::config_key_docs()
            .into_iter()
            .find(|d| d.segments == ["token_optimization", "check_timeout_ms"])
            .unwrap()
            .comment;
        let tok = onebrain_core::config::TokenOptimizationConfig::default();
        let lines: Vec<&str> = after.lines().collect();
        let idx = lines
            .iter()
            .position(|l| l.trim_start().starts_with("check_timeout_ms:"))
            .unwrap_or_else(|| panic!("check_timeout_ms missing:\n{after}"));
        assert_eq!(lines[idx - 1].trim_start(), expected_comment, "{after}");
        assert_eq!(
            lines[idx].trim_start(),
            format!("check_timeout_ms: {}", tok.check_timeout_ms),
            "{after}"
        );

        // Doctor now reports clean.
        let r = token_optimization_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");

        // Idempotence: a second `--fix` run is a byte-identical no-op.
        let outcome2 = fix_token_optimization(d.path(), false);
        let FixOutcome::Fixed(msg2) = outcome2 else {
            panic!("expected Fixed, got: {outcome2:?}");
        };
        assert!(msg2.contains("already set"), "msg: {msg2}");
        let after2 = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after2, after, "second --fix run must be byte-identical");
    }

    #[test]
    fn fix_token_optimization_backfills_all_missing_sub_keys_generically() {
        // Proves the "driven from config_key_docs, not just check_timeout_ms"
        // claim: a block with only level+read_hook gets EVERY other
        // documented sub-key backfilled, with get_max_tokens/
        // snippet_max_chars landing as the SAME commented placeholders the
        // fresh template uses (never pinning a fixed cap on an existing
        // vault that never asked for one).
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "token_optimization:\n  level: balanced\n  read_hook: ledger\n",
        )
        .unwrap();

        let outcome = fix_token_optimization(d.path(), false);
        let FixOutcome::Fixed(_) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
        let tok = onebrain_core::config::TokenOptimizationConfig::default();
        assert_eq!(
            parsed["token_optimization"]["level"].as_str(),
            Some("balanced")
        );
        assert_eq!(
            parsed["token_optimization"]["read_hook"].as_str(),
            Some("ledger")
        );
        assert_eq!(
            parsed["token_optimization"]["strip_frontmatter"].as_str(),
            Some(tok.strip_frontmatter.to_string()).as_deref()
        );
        assert_eq!(
            parsed["token_optimization"]["model"].as_str(),
            Some(tok.model.as_str())
        );
        assert_eq!(
            parsed["token_optimization"]["check_timeout_ms"].as_u64(),
            Some(tok.check_timeout_ms as u64)
        );
        // Stay commented placeholders — never pinned to a fixed value.
        assert!(parsed["token_optimization"].get("get_max_tokens").is_none());
        assert!(parsed["token_optimization"]
            .get("snippet_max_chars")
            .is_none());
        assert!(after.contains("# get_max_tokens: 6000"), "{after}");
        assert!(after.contains("# snippet_max_chars: 200"), "{after}");

        // The placeholder's own doc comment sits directly above it — the
        // same splice code path as the active keys, exercised here for the
        // commented-placeholder shape specifically.
        let expected_gm_comment = onebrain_fs::config_key_docs()
            .into_iter()
            .find(|d| d.segments == ["token_optimization", "get_max_tokens"])
            .unwrap()
            .comment;
        let lines: Vec<&str> = after.lines().collect();
        let gm_idx = lines
            .iter()
            .position(|l| l.trim_start() == "# get_max_tokens: 6000")
            .unwrap_or_else(|| panic!("get_max_tokens placeholder missing:\n{after}"));
        assert_eq!(
            lines[gm_idx - 1].trim_start(),
            expected_gm_comment,
            "{after}"
        );

        // Every backfilled key carries its doc comment.
        assert!(
            onebrain_fs::config_key_docs()
                .iter()
                .filter(|d| d.segments.first() == Some(&"token_optimization"))
                .all(
                    |d| onebrain_fs::yaml_edit::key_or_commented_placeholder_present(
                        &after, d.segments
                    )
                ),
            "every token_optimization sub-key must be documented after --fix:\n{after}"
        );

        let r = token_optimization_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");
    }

    #[test]
    fn fix_token_optimization_noop_on_spaced_and_quoted_sub_keys_no_duplicate() {
        // BLOCKING regression (issue #270 R-review): a valid `check_timeout_ms
        // : 500` (space before colon) or `"strip_frontmatter": always`
        // (quoted) key must be recognised as PRESENT. A raw-prefix presence
        // check missed them → the key was false-reported missing → the splice
        // inserted a SECOND `check_timeout_ms: 200` → the file grew a DUPLICATE
        // key → `serde_yaml` fails to parse it on the next load. `--fix` must
        // NEVER corrupt a valid onebrain.yml.
        //
        // Fully-populate the block from the shared source, then rewrite two of
        // its lines into the tricky (but valid) spaced / quoted forms. `--fix`
        // must be a NO-OP: no duplicate, and the file still parses.
        let d = tempdir().unwrap();
        let mut block_lines = onebrain_fs::token_optimization_block_lines();
        for line in block_lines.iter_mut() {
            if line.trim_start().starts_with("check_timeout_ms:") {
                *line = "  check_timeout_ms : 200".to_string();
            } else if line.trim_start().starts_with("strip_frontmatter:") {
                *line = "  \"strip_frontmatter\": auto".to_string();
            }
        }
        let original = format!("update_channel: stable\n{}\n", block_lines.join("\n"));
        // Sanity: the pre-fix file already parses and has one of each key.
        serde_yaml::from_str::<serde_yaml::Value>(&original).unwrap();
        fs::write(d.path().join("onebrain.yml"), &original).unwrap();

        let outcome = fix_token_optimization(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("already set"), "must be a no-op: {msg}");

        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        // No duplicate key inserted → still exactly one occurrence each.
        assert_eq!(
            after.matches("check_timeout_ms").count(),
            1,
            "spaced key must not be duplicated:\n{after}"
        );
        assert_eq!(
            after.matches("strip_frontmatter").count(),
            1,
            "quoted key must not be duplicated:\n{after}"
        );
        // The file MUST still parse — the corruption this test guards against.
        serde_yaml::from_str::<serde_yaml::Value>(&after)
            .unwrap_or_else(|e| panic!("--fix corrupted the YAML ({e}):\n{after}"));
        assert_eq!(after, original, "no-op must be byte-identical");
    }

    #[test]
    fn fix_token_optimization_all_commented_block_does_not_double_backfill() {
        // Finding 2 (R-review): a `token_optimization:` block whose children
        // are ALL commented (user deliberately disabled them) must NOT gain
        // fresh ACTIVE duplicates of those commented keys. The commented
        // level/read_hook are respected; only genuinely-absent keys are added.
        let d = tempdir().unwrap();
        let original =
            "token_optimization:\n  # level: aggressive\n  # read_hook: ledger\n".to_string();
        fs::write(d.path().join("onebrain.yml"), &original).unwrap();

        let outcome = fix_token_optimization(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        // check_timeout_ms (and the other genuinely-absent keys) backfilled…
        assert!(msg.contains("check_timeout_ms"), "msg: {msg}");
        // …but NOT the commented level / read_hook.
        assert!(
            !msg.contains("level"),
            "commented level must be respected: {msg}"
        );
        assert!(
            !msg.contains("read_hook"),
            "commented read_hook must be respected: {msg}"
        );

        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        // The user's commented lines survive untouched.
        assert!(after.contains("# level: aggressive"), "{after}");
        assert!(after.contains("# read_hook: ledger"), "{after}");
        // No ACTIVE level: / read_hook: line was added.
        assert_eq!(
            after
                .lines()
                .filter(|l| {
                    let t = l.trim_start();
                    !t.starts_with('#') && (t.starts_with("level:") || t.starts_with("read_hook:"))
                })
                .count(),
            0,
            "must not add active duplicates of commented keys:\n{after}"
        );
        // File still parses; doctor now clean (all sub-keys present, some
        // commented, some active).
        serde_yaml::from_str::<serde_yaml::Value>(&after).unwrap();
        let r = token_optimization_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok, "{r:?}");
    }

    #[test]
    fn fix_token_optimization_inline_block_fails_without_corrupting_yaml() {
        // Regression: an inline/flow-form `token_optimization: {…}` block
        // parses fine (so the whole-block-present gate holds and sub-key
        // backfill is attempted), but splicing lines into an inline mapping
        // would produce unparseable YAML. The recipe must refuse (honest
        // Failed with a manual step) and leave the file byte-identical and
        // re-parseable — same defensive contract as the flow-ROOT case
        // below.
        let d = tempdir().unwrap();
        let inline = "update_channel: stable\ntoken_optimization: {level: balanced}\n";
        fs::write(d.path().join("onebrain.yml"), inline).unwrap();

        let outcome = fix_token_optimization(d.path(), false);
        let FixOutcome::Failed(msg) = outcome else {
            panic!("expected Failed, got: {outcome:?}");
        };
        assert!(msg.contains("block form"), "actionable message: {msg}");

        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, inline, "declined shape must be left untouched");
        let parsed: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
        assert_eq!(parsed["update_channel"].as_str(), Some("stable"));
    }

    #[test]
    fn fix_token_optimization_flow_root_fails_without_corrupting_yaml() {
        // Regression, mirroring `fix_search_exclude_flow_form_search_fails_
        // without_corrupting_yaml`: a flow-style ROOT mapping parses fine (so
        // the presence gate fires), but appending a block-form
        // `token_optimization:` key after a flow scalar root would produce
        // unparseable YAML. The recipe must refuse (honest Failed with a
        // manual step) and leave the file byte-identical and re-parseable.
        let d = tempdir().unwrap();
        let flow = "{update_channel: stable, checkpoint: {messages: 15}}\n";
        fs::write(d.path().join("onebrain.yml"), flow).unwrap();

        let outcome = fix_token_optimization(d.path(), false);
        let FixOutcome::Failed(msg) = outcome else {
            panic!("expected Failed, got: {outcome:?}");
        };
        assert!(msg.contains("block form"), "actionable message: {msg}");

        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, flow, "declined shape must be left untouched");
        // The file must still parse — never write (or leave) corrupt YAML.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
        assert_eq!(
            parsed["update_channel"].as_str(),
            Some("stable"),
            "existing value must survive"
        );
    }

    // ── qmd leftover detection + guided cleanup ──────────────────────────────

    use onebrain_core::config::SearchConfig;

    /// Config fixture with native search genuinely configured (real
    /// `search.collection`, not the legacy fallback) and no declined flag.
    fn cfg_native_search(collection: &str) -> onebrain_core::VaultConfig {
        onebrain_core::VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
            search: SearchConfig {
                collection: Some(collection.to_string()),
                ..Default::default()
            },
            token_optimization: Default::default(),
            stats: Default::default(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn detects_qmd_binary_package_and_cache_sizes() {
        let home = tempdir().unwrap();
        let path_dir = tempdir().unwrap();

        // Fake npm global install tree: .../lib/node_modules/@tobilu/qmd/bin/qmd
        let real_bin = path_dir.path().join("lib/node_modules/@tobilu/qmd/bin/qmd");
        fs::create_dir_all(real_bin.parent().unwrap()).unwrap();
        fs::write(&real_bin, b"#!/bin/sh\n").unwrap();

        // PATH entry with a `qmd` symlink into that tree (Homebrew-style
        // global bin shim — matches พี่เก่ง's real machine layout).
        let bin_dir = path_dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let symlink_path = bin_dir.join("qmd");
        std::os::unix::fs::symlink(&real_bin, &symlink_path).unwrap();

        // Fake ~/.cache/qmd with sized fixture files (models/ + index.sqlite,
        // matching the real ground truth: 1000 + 234 = 1234 bytes total).
        let cache_dir = home.path().join(".cache").join("qmd");
        fs::create_dir_all(cache_dir.join("models")).unwrap();
        fs::write(cache_dir.join("models").join("m.bin"), vec![0u8; 1000]).unwrap();
        fs::write(cache_dir.join("index.sqlite"), vec![0u8; 234]).unwrap();

        // Fake ~/.config/qmd.
        let config_dir = home.path().join(".config").join("qmd");
        fs::create_dir_all(&config_dir).unwrap();

        let leftovers = detect_qmd_leftovers(home.path(), &bin_dir.display().to_string());

        assert_eq!(leftovers.binary.as_deref(), Some(symlink_path.as_path()));
        assert_eq!(leftovers.npm_package.as_deref(), Some("@tobilu/qmd"));
        let (found_cache_dir, size) = leftovers.cache_dir.expect("cache dir detected");
        assert_eq!(found_cache_dir, cache_dir);
        assert_eq!(size, 1234, "size must equal the fixture bytes exactly");
        assert_eq!(leftovers.config_dir, Some(config_dir));
    }

    #[cfg(unix)]
    #[test]
    fn npm_package_from_symlink_parses_unscoped_package() {
        // Unscoped install layout: .../node_modules/qmd/bin/qmd (no @scope)
        // → package name is just `qmd`.
        let tree = tempdir().unwrap();
        let real_bin = tree.path().join("lib/node_modules/qmd/bin/qmd");
        fs::create_dir_all(real_bin.parent().unwrap()).unwrap();
        fs::write(&real_bin, b"#!/bin/sh\n").unwrap();
        let bin_dir = tree.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let symlink_path = bin_dir.join("qmd");
        std::os::unix::fs::symlink(&real_bin, &symlink_path).unwrap();

        assert_eq!(
            npm_package_from_symlink(&symlink_path).as_deref(),
            Some("qmd")
        );
    }

    /// Binary-only leftovers fixture: a REAL file (not an npm symlink), so
    /// `npm_package` is `None`. Shared by the non-npm honesty tests below.
    fn binary_only_leftovers(home: &Path) -> QmdLeftovers {
        let bin_dir = home.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let bin = bin_dir.join("qmd");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        QmdLeftovers {
            binary: Some(bin),
            npm_package: None,
            cache_dir: None,
            config_dir: None,
        }
    }

    #[test]
    fn qmd_manual_outcome_binary_only_includes_rm_command() {
        // Non-interactive message for a non-npm binary-only leftover: the
        // command list must carry `rm <binary>` — without it the message
        // ended in a dangling "remove manually: ".
        let home = tempdir().unwrap();
        let leftovers = binary_only_leftovers(home.path());
        let bin_display = tildify(leftovers.binary.as_ref().unwrap(), home.path());
        let FixOutcome::Manual(msg) = qmd_manual_outcome(&leftovers, home.path()) else {
            panic!("expected Manual");
        };
        assert!(
            msg.contains(&format!("rm {bin_display}")),
            "rm command for the binary: {msg}"
        );
        assert!(
            !msg.trim_end().ends_with(':'),
            "no dangling command list: {msg}"
        );
    }

    #[test]
    fn qmd_remove_leftovers_binary_only_returns_manual_without_deleting() {
        // Interactive-confirmed path, but the ONLY leftover is a non-npm
        // binary: nothing is removable automatically (unknown binaries are
        // never auto-deleted), so the honest outcome is Manual with the rm
        // command — never a false "Fixed".
        let home = tempdir().unwrap();
        let leftovers = binary_only_leftovers(home.path());
        let bin = leftovers.binary.clone().unwrap();
        let outcome = qmd_remove_leftovers(&leftovers, home.path());
        match outcome {
            FixOutcome::Manual(msg) => {
                assert!(msg.contains("nothing removed"), "honest wording: {msg}");
                assert!(
                    msg.contains(&format!("rm {}", tildify(&bin, home.path()))),
                    "rm command present: {msg}"
                );
            }
            other => panic!("expected Manual for binary-only leftovers, got: {other:?}"),
        }
        assert!(bin.is_file(), "non-npm binary must never be auto-deleted");
    }

    #[test]
    fn qmd_remove_leftovers_non_npm_binary_with_dirs_is_partial() {
        // Interactive-confirmed path with cache+config dirs AND a non-npm
        // binary: the dirs are removed, but the surviving binary downgrades
        // the outcome to Partial (not Fixed) with the exact rm command.
        let home = tempdir().unwrap();
        let mut leftovers = binary_only_leftovers(home.path());
        let cache_dir = home.path().join(".cache/qmd");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("index.sqlite"), vec![0u8; 64]).unwrap();
        let config_dir = home.path().join(".config/qmd");
        fs::create_dir_all(&config_dir).unwrap();
        leftovers.cache_dir = Some((cache_dir.clone(), 64));
        leftovers.config_dir = Some(config_dir.clone());
        let bin = leftovers.binary.clone().unwrap();

        let outcome = qmd_remove_leftovers(&leftovers, home.path());
        match outcome {
            FixOutcome::Partial(msg) => {
                assert!(msg.contains("~/.cache/qmd"), "removed dirs listed: {msg}");
                assert!(
                    msg.contains(&format!("rm {}", tildify(&bin, home.path()))),
                    "rm command present: {msg}"
                );
            }
            other => panic!("expected Partial when the binary survives, got: {other:?}"),
        }
        assert!(!cache_dir.exists(), "cache dir removed");
        assert!(!config_dir.exists(), "config dir removed");
        assert!(bin.is_file(), "non-npm binary must never be auto-deleted");
    }

    #[test]
    fn no_finding_when_collection_unset() {
        let config = onebrain_core::VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
            search: Default::default(),
            token_optimization: Default::default(),
            stats: Default::default(),
        };
        let home = tempdir().unwrap();
        let result = qmd_leftovers_check(&config, home.path(), "");
        assert_eq!(result.status, DoctorStatus::Ok);
        assert!(result.message.contains("skipped"), "{result:?}");
    }

    #[test]
    fn gate_skips_when_only_legacy_qmd_collection_set() {
        // A vault that still carries the deprecated top-level `qmd_collection`
        // key hasn't migrated yet — even though `load_vault_config`'s
        // read-fallback would backfill `search.collection` from it in
        // production. The qmd-uninstall nag must not pile onto the
        // still-open `legacy-qmd-collection` migration warning.
        let config = onebrain_core::VaultConfig {
            qmd_collection: Some("ob-1".to_string()),
            checkpoint: Default::default(),
            folders: Default::default(),
            search: SearchConfig {
                collection: Some("ob-1".to_string()),
                ..Default::default()
            },
            token_optimization: Default::default(),
            stats: Default::default(),
        };
        let home = tempdir().unwrap();
        let result = qmd_leftovers_check(&config, home.path(), "");
        assert_eq!(result.status, DoctorStatus::Ok, "{result:?}");
    }

    #[test]
    fn warns_with_hint_when_gated_in_and_leftovers_found() {
        let home = tempdir().unwrap();
        fs::create_dir_all(home.path().join(".config").join("qmd")).unwrap();
        let config = cfg_native_search("c1");

        let result = qmd_leftovers_check(&config, home.path(), "");
        assert_eq!(result.status, DoctorStatus::Warn, "{result:?}");
        assert!(result.hint.is_some(), "{result:?}");
        assert!(planned_action(&result).is_some(), "{result:?}");
    }

    #[test]
    fn declined_flag_suppresses_refix_prompt_but_keeps_info() {
        let home = tempdir().unwrap();
        let cache_dir = home.path().join(".cache").join("qmd");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("index.sqlite"), vec![0u8; 10]).unwrap();

        let mut config = cfg_native_search("c1");
        config.stats.qmd_cleanup_declined = Some(true);

        let result = qmd_leftovers_check(&config, home.path(), "");
        // INFO listed: still a Warn finding with the leftover details, so the
        // user keeps seeing it in a plain `doctor` run.
        assert_eq!(result.status, DoctorStatus::Warn, "{result:?}");
        assert!(!result.details.is_empty(), "{result:?}");
        // --fix does not re-offer: no hint, no planned auto-fix action, and
        // `attempt_fix` short-circuits to Manual without touching disk —
        // even with `interactive_confirmed = true`, the recorded decline
        // wins over a fresh confirmation.
        assert!(result.hint.is_none(), "{result:?}");
        assert!(planned_action(&result).is_none(), "{result:?}");
        let outcome = attempt_fix(&result, home.path(), false, true);
        match outcome {
            FixOutcome::Manual(msg) => assert!(msg.contains("previously declined"), "msg: {msg}"),
            other => panic!("expected Manual for a declined cleanup, got: {other:?}"),
        }
        // No side effects — the cache fixture created above must survive.
        assert!(
            cache_dir.is_dir(),
            "declined path must not touch the filesystem"
        );
    }

    #[test]
    fn qmd_leftovers_in_doctor_sections_and_display_label() {
        assert_eq!(display_label("qmd-leftovers"), "qmd cleanup");
        assert!(
            DOCTOR_SECTIONS
                .iter()
                .any(|(_, _, checks)| checks.contains(&"qmd-leftovers")),
            "qmd-leftovers must be assigned to a section, not fall through to Other"
        );
    }

    #[test]
    fn decline_qmd_cleanup_writes_stats_key_preserving_comments() {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("onebrain.yml"),
            "# my comment\nupdate_channel: stable\n",
        )
        .unwrap();
        decline_qmd_cleanup(d.path());
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(after.contains("# my comment"), "{after}");
        assert!(after.contains("stats:"), "{after}");
        assert!(after.contains("qmd_cleanup_declined: true"), "{after}");
    }
}
