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
            search: Default::default(),
        }
    });

    let mut results = all_checks(vault_root.as_path(), &config);
    if !want_structured {
        // Under `--fix` the verdict footer is deferred until after the fix pass
        // (one final footer, not a redundant before-and-after pair); a plain
        // run prints it inline with the report.
        emit_text_report(&results, mode, quiet, !fix)?;
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
                } else if confirm_fix(auto.len(), false, yes) {
                    let outcomes: Vec<(String, FixOutcome)> = auto
                        .iter()
                        .map(|r| (r.check.clone(), attempt_fix(r, vault_root.as_path(), false)))
                        .collect();
                    any_recipe_failed = outcomes
                        .iter()
                        .any(|(_, o)| matches!(o, FixOutcome::Failed(_) | FixOutcome::Partial(_)));
                    print_fix_summary(&outcomes);
                    results = all_checks(vault_root.as_path(), &config);
                } else {
                    println!("\nNo changes made.");
                }
            }
            // Single deferred verdict footer — the pre-fix report omitted its
            // own footer so this is the only one the user sees.
            write_summary_footer(&mut std::io::stdout(), &results)?;
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
    results.push(native_search_check(vault_root));
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
    if findings.is_empty() && undocumented.is_empty() {
        return DoctorResult::ok(CONFIG_VALUES_CHECK, "all values in range");
    }
    let mut message_parts: Vec<String> = Vec::new();
    let mut details: Vec<String> = findings.iter().map(ConfigFinding::detail_line).collect();
    if !findings.is_empty() {
        message_parts.push(format!("{} invalid value(s)", findings.len()));
    }
    if !undocumented.is_empty() {
        message_parts.push(format!("{} undocumented key(s)", undocumented.len()));
        details.push(format!(
            "{} key(s) lack self-documentation comments — doctor --fix will add them: {}",
            undocumented.len(),
            undocumented.join(", ")
        ));
    }
    let any_resettable = findings.iter().any(|f| f.resettable);
    let mut r =
        DoctorResult::warn(CONFIG_VALUES_CHECK, message_parts.join(" · ")).with_details(details);
    let hint = match (any_resettable, !undocumented.is_empty()) {
        (true, true) => Some(
            "Run onebrain doctor --fix to reset out-of-range tunables and add the missing self-documentation comments",
        ),
        (true, false) => {
            Some("Run onebrain doctor --fix to reset out-of-range tunables to their defaults")
        }
        (false, true) => {
            Some("Run onebrain doctor --fix to add the missing self-documentation comments")
        }
        (false, false) => None,
    };
    if let Some(h) = hint {
        r = r.with_hint(h);
    }
    r
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

/// Native-search index check (`check = "search"`). Read-only and
/// download-free: it resolves the collection, checks whether the on-disk index
/// exists, and — only if it does — opens the engine (lazy embedder, so
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

    // Index exists → open the engine (lazy embedder, no download) and read
    // status. A hard open/status failure is advisory, not fatal. The open
    // goes through `open_engine_with_collection` (NOT `open_engine`): the
    // latter resolves via `collection_for`, which PERSISTS a generated
    // collection name through a comment-destroying serde rewrite when the
    // `search.collection` key is absent — doctor's read path must never
    // write the config.
    let (last_indexed_at, doc_count, pending) =
        match open_engine_with_collection(&resolved, &collection) {
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
/// `result.check` (and, where a check name covers multiple sub-conditions,
/// on the message content too). Hidden hints that say "Run onebrain doctor
/// --fix to ..." are silently rewritten to a non-circular message when no
/// recipe maps.
fn attempt_fix(result: &DoctorResult, vault_root: &Path, json: bool) -> FixOutcome {
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
        "config-values" => {
            Some("reset out-of-range values to defaults · add missing self-documentation comments")
        }
        "claude-settings" => Some("remove the stale marketplace entry"),
        "plugin-cache" => Some("remove the stale plugin cache"),
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

/// Recipe — `legacy-qmd-collection` warning means the vault's config still
/// carries a deprecated top-level `qmd_collection` key (v3.3 and earlier).
/// Migrate it: if `search.collection` is absent, set it to the
/// `qmd_collection` value; then remove the legacy key. If `search.collection`
/// is already set, don't overwrite it — just drop the legacy key.
///
/// Writes via the established config-mutation pattern (parse → mutate → backup
/// → atomic write), preserving every other key. YAML comments are not
/// preserved (serde_yaml re-serializes from the parsed model), matching
/// `fix_vault_yml_keys`; the Fixed message notes this.
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
    let mut yaml: serde_yaml::Value = match serde_yaml::from_str(&text) {
        Ok(v) => v,
        Err(e) => return FixOutcome::Failed(format!("parse {filename}: {e}")),
    };
    let mapping = match yaml.as_mapping_mut() {
        Some(m) => m,
        None => return FixOutcome::Failed(format!("{filename} root is not a mapping")),
    };

    let qmd_key = serde_yaml::Value::String("qmd_collection".to_string());
    let Some(legacy_value) = mapping.remove(&qmd_key) else {
        // Already migrated (or never present) — idempotent no-op.
        return FixOutcome::Fixed(format!(
            "{filename}: no qmd_collection key — nothing to migrate"
        ));
    };
    let legacy_str = legacy_value.as_str().map(str::to_string);

    // Decide whether to seed `search.collection` from the legacy value: only
    // when `search.collection` isn't already set (never overwrite the user's
    // current value).
    let search_key = serde_yaml::Value::String("search".to_string());
    let collection_key = serde_yaml::Value::String("collection".to_string());
    let search_collection_set = mapping
        .get(&search_key)
        .and_then(|v| v.as_mapping())
        .map(|s| s.contains_key(&collection_key))
        .unwrap_or(false);

    let mut seeded = false;
    if !search_collection_set {
        if let Some(value) = &legacy_str {
            // Ensure `search` is a mapping, then set `collection`.
            let needs_replace = match mapping.get(&search_key) {
                Some(v) => !v.is_mapping(),
                None => true,
            };
            if needs_replace {
                mapping.insert(
                    search_key.clone(),
                    serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
                );
            }
            let search = mapping
                .get_mut(&search_key)
                .and_then(|v| v.as_mapping_mut())
                .expect("search key ensured to be a mapping");
            search.insert(collection_key, serde_yaml::Value::String(value.clone()));
            seeded = true;
        }
    }

    let serialized = match serde_yaml::to_string(&yaml) {
        Ok(s) => s,
        Err(e) => return FixOutcome::Failed(format!("serialize {filename}: {e}")),
    };
    // Defense-in-depth: back up before the re-serializing write (drops
    // comments). Hard precondition — no write without a backup.
    if let Err(e) = onebrain_fs::backup_config_file(&path) {
        return FixOutcome::Failed(format!("backup {filename} before write: {e}"));
    }
    if let Err(e) = onebrain_fs::atomic_write_text(&path, &serialized) {
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
    FixOutcome::Fixed(format!("{action} (note: YAML comments not preserved)"))
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
/// v3.4.8 — its recipe resets them via the comment-preserving line editor,
/// so they must NOT be repaired here (this recipe's serde re-serialization
/// would destroy the file's comments first). YAML comments are not preserved
/// when this recipe does write (serde_yaml re-serializes from the parsed
/// model) — the Fixed message calls this out so the user knows what changed
/// besides the keys.
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

    if added.is_empty() && removed.is_empty() {
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
    if let Err(e) = onebrain_fs::atomic_write_text(&path, &serialized) {
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
    if findings.is_empty() && undocumented_keys(&text).is_empty() {
        return FixOutcome::Fixed(format!(
            "{filename}: all values already in range and documented"
        ));
    }

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

    if !resets.is_empty() || !comments_added.is_empty() {
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
        // Honest tri-state: real resets landed on disk AND something remains
        // → Partial (distinct glyph, still a non-zero exit); nothing landed
        // → Failed.
        return if resets.is_empty() {
            FixOutcome::Failed(parts.join(" · "))
        } else {
            FixOutcome::Partial(parts.join(" · "))
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
            if let Err(e) = onebrain_fs::atomic_write_text(&path, &updated) {
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
    ("📊", "Index & state", &["orphan-checkpoints", "search"]),
];

/// Short, scannable display label for a check name (matches the approved
/// layout). Unknown checks fall back to their raw name so a future check
/// still renders something sensible before its label is added here.
fn display_label(check: &str) -> &str {
    match check {
        "onebrain.yml" => "onebrain.yml",
        "onebrain.yml-keys" => "schema",
        "config-values" => "config values",
        "vault-config-migration" => "config migration",
        "legacy-qmd-collection" => "qmd_collection",
        "folders" => "folders",
        "plugin-files" => "plugin files",
        "plugin-cache" => "plugin cache",
        "settings-hooks" => "hooks",
        "claude-settings" => "claude settings",
        "orphan-checkpoints" => "checkpoints",
        "search" => "search",
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

    for (emoji, header, checks) in DOCTOR_SECTIONS {
        let mut steps = Vec::new();
        for name in checks {
            if let Some(r) = results.iter().find(|r| r.check == *name) {
                steps.push(to_step(r));
                placed.insert(*name);
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
        .map(to_step)
        .collect();
    if !leftovers.is_empty() {
        sections.push(Section::with_emoji("❓", "Other", leftovers));
    }

    sections
}

/// Render the summary footer: a rule, the verdict line (overall glyph, the
/// ok/warn/fail counts, and the total), plus an optional `--fix` next-action
/// hint when there are repairable issues. Plain grouped-convention lines — no
/// framing rules:
///
/// ```text
/// ⚠️  11 ok · 1 warning · 0 fail · 12 checks
/// 💡  Run onebrain doctor --fix to auto-repair
/// ```
///
/// The verdict emoji follows the row-glyph semantics (fail dominates, then
/// warn, then ok): ✅ / ⚠️ / ❌.
fn write_summary_footer<W: Write>(w: &mut W, results: &[DoctorResult]) -> Result<()> {
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

    // Overall verdict emoji: fail dominates, then warn, then ok.
    let verdict = if errors > 0 {
        "❌"
    } else if warnings > 0 {
        "⚠️"
    } else {
        "✅"
    };

    // "fail" has no plural form; "warning" does.
    let warnings_word = if warnings == 1 { "warning" } else { "warnings" };

    writeln!(w)?;
    writeln!(
        w,
        "{verdict}  {passing} ok · {warnings} {warnings_word} · {errors} fail · {total} checks"
    )?;
    // `--fix` next-action — shown only when at least one issue has an
    // automated recipe. A manual-only remainder (e.g. a pending search
    // reindex) gets no pointer, since `--fix` can't repair it.
    let any_auto_fixable = results.iter().any(|r| {
        matches!(r.status, DoctorStatus::Warn | DoctorStatus::Error) && planned_action(r).is_some()
    });
    if any_auto_fixable {
        writeln!(w, "💡  Run onebrain doctor --fix to auto-repair")?;
    }
    Ok(())
}

/// Render the full grouped report to `w` via a [`ProgressRenderer`].
///
/// Grouped-status convention throughout: no framed header, no rules — the
/// report opens directly with the first emoji-headed section, body rows are
/// four-space indented with their verdict glyphs, and the footer is plain
/// convention lines.
///
/// `animate` drives the gating seam: when true the renderer paints the
/// braille spinner + paced reveal (colour TTY, non-quiet, text mode); when
/// false it emits only the static lines (deterministic tests). `color` is the
/// resolved colour bit (row/hint styling).
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
    color: bool,
    animate: bool,
    show_footer: bool,
) -> Result<()> {
    use crate::output::ProgressRenderer;
    let sections = build_sections(results);
    {
        // force_static = !animate.
        let mut renderer = ProgressRenderer::with_writer(&mut w, !animate, color);
        // Grouped-convention body rows: four-space indent under the emoji
        // section headers (hints indent three columns deeper).
        renderer.set_row_indent("    ");
        for section in &sections {
            renderer.render_section(section)?;
        }
    }
    if show_footer {
        write_summary_footer(&mut w, results)?;
    }
    Ok(())
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
    mode: &OutputMode,
    quiet: bool,
    show_footer: bool,
) -> Result<()> {
    use crate::output::{is_color_text, should_animate};
    use std::io::IsTerminal;
    // Compute the gating decision directly — no throwaway renderer round-trip.
    let animate = should_animate(mode, std::io::stdout().is_terminal(), quiet);
    let color = is_color_text(mode);
    render_grouped_report(std::io::stdout(), results, color, animate, show_footer)
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
        render_grouped_report(&mut buf, results, color, false, true).unwrap();
        String::from_utf8(buf).unwrap()
    }

    // ── Section grouping ─────────────────────────────────────────────────

    #[test]
    fn build_sections_assigns_each_check_to_its_section() {
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
        assert_eq!(
            by_header["Config"],
            vec![
                "onebrain.yml",
                "schema",
                "config migration",
                "qmd_collection"
            ]
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
        let out = render_static_report(&sample_results(), false);
        // Grouped convention: no framed header, no rules — the report opens
        // with the first emoji-headed section.
        assert!(!out.contains("OneBrain Doctor"), "no framed title: {out:?}");
        assert!(!out.contains("──"), "no rules anywhere: {out:?}");
        // Emoji section headers in the `{emoji}  {Title}` shape.
        for header in [
            "⚙️  Config",
            "📁  Vault structure",
            "🔌  Integration",
            "📊  Index & state",
        ] {
            assert!(out.contains(header), "section {header}: {out:?}");
        }
        // Four-space-indented rows: verdict glyph + label + detail.
        assert!(out.contains("    ✓ onebrain.yml"), "ok line: {out:?}");
        assert!(out.contains("    ✓ schema"), "schema line: {out:?}");
        // Warn glyph + label + the indented hint line.
        assert!(out.contains("    ⚠ hooks"), "warn line: {out:?}");
        assert!(
            out.contains("       └ onebrain doctor --fix"),
            "warn hint line: {out:?}"
        );
        assert!(
            out.contains("       └ onebrain search reindex"),
            "search hint line: {out:?}"
        );
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
        let out = render_static_report(&sample_results(), true);
        assert!(!out.contains('\r'), "static must not redraw: {out:?}");
        for f in crate::output::SPINNER_FRAMES {
            assert!(!out.contains(f), "static must not paint spinner: {out:?}");
        }
    }

    // ── Summary footer: counts + verdict + --fix action ──────────────────

    #[test]
    fn footer_counts_and_warn_verdict_with_fix_action() {
        let out = render_static_report(&sample_results(), false);
        // Plain convention footer: verdict emoji + counts + total on one line.
        assert!(
            out.contains("⚠️  8 ok · 2 warnings · 0 fail · 10 checks"),
            "footer line: {out:?}"
        );
        // Fixable issues → --fix next-action hint shown.
        assert!(
            out.contains("💡  Run onebrain doctor --fix to auto-repair"),
            "fix action: {out:?}"
        );
    }

    #[test]
    fn footer_is_plain_convention_lines_no_rules() {
        // The grouped convention has no framing rules anywhere: the footer is
        // the verdict line (+ optional 💡 hint), preceded by one blank line.
        let out = render_static_report(&sample_results(), false);
        assert!(
            !out.lines().any(|l| l.chars().any(|c| c == '─')),
            "no rule lines: {out:?}"
        );
        let verdict = out
            .lines()
            .find(|l| l.contains("ok ·") && l.contains("checks"))
            .expect("verdict line");
        assert!(
            verdict.starts_with("⚠️  "),
            "verdict emoji + two spaces: {verdict:?}"
        );
        assert!(
            verdict.trim_end().ends_with("10 checks"),
            "total on the same line: {verdict:?}"
        );
    }

    #[test]
    fn footer_all_ok_shows_check_verdict_and_no_fix_action() {
        let results: Vec<DoctorResult> = sample_results()
            .into_iter()
            .map(|r| DoctorResult::ok(r.check, "ok"))
            .collect();
        let out = render_static_report(&results, false);
        assert!(
            out.contains("✅  10 ok · 0 warnings · 0 fail · 10 checks"),
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
        let mut results = sample_results();
        // Index 4 is the `folders` check (Config now carries an extra
        // legacy-qmd-collection row before it).
        results[4] =
            DoctorResult::error("folders", "0/8 present").with_hint("onebrain init --force");
        let out = render_static_report(&results, false);
        assert!(out.contains("✗ folders"), "fail line: {out:?}");
        assert!(
            out.contains("❌  7 ok · 2 warnings · 1 fail · 10 checks"),
            "fail verdict footer: {out:?}"
        );
        assert!(
            out.contains("💡  Run onebrain doctor --fix to auto-repair"),
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
        render_grouped_report(&mut buf, &results, false, false, true).unwrap();
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
        let outcome = attempt_fix(&r, d.path(), false);
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
        let outcome = attempt_fix(&r, d.path(), false);
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
        let outcome = attempt_fix(&r, d.path(), false);
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

    // ── footer: "1 warning" singular form ────────────────────────────────────

    #[test]
    fn footer_uses_singular_warning_form() {
        // Exactly 1 warning → "1 warning" not "1 warnings".
        let results = vec![
            DoctorResult::ok("onebrain.yml", "ok"),
            DoctorResult::warn("settings-hooks", "dup"),
        ];
        let mut buf = Vec::new();
        write_summary_footer(&mut buf, &results).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("1 warning") && !out.contains("1 warnings"),
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
        let outcome = attempt_fix(&r, d.path(), false);
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
        let outcome = attempt_fix(&r, d.path(), false);
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

    // ── write_summary_footer: manual-only warns → no fix pointer ─────────────

    #[test]
    fn write_summary_footer_manual_only_warn_shows_no_fix_action() {
        // orphan-checkpoints is manual-only (planned_action returns None).
        // any_auto_fixable = false even though there is a Warn → no fix pointer.
        let results = vec![
            DoctorResult::ok("onebrain.yml", "ok"),
            DoctorResult::warn("orphan-checkpoints", "2 orphans").with_hint("run /wrapup"),
        ];
        let mut buf = Vec::new();
        write_summary_footer(&mut buf, &results).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("1 warning"),
            "warning counted in footer: {out:?}"
        );
        assert!(
            !out.contains("to auto-repair"),
            "no fix pointer for manual-only warn: {out:?}"
        );
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
                    reranker:\n    enabled: true\n    model: onebrain-rerank-v1\n    min_candidates: 10\n    min_score: 0.30\n";
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
        assert_eq!(by("search.reranker.min_score"), "0.30");
        assert_eq!(by("update_channel"), "stable");
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
        // In-range, documented values → ok. (An in-range key WITHOUT a doc
        // comment is the undocumented-keys warn — covered below.)
        fs::write(
            d.path().join("onebrain.yml"),
            "# channel comment\nupdate_channel: next\n",
        )
        .unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Ok);
        assert_eq!(r.message, "all values in range");
        // In-range but UNDOCUMENTED key → warn with the backfill detail +
        // comment-specific hint (still zero writes — read-only check).
        fs::write(d.path().join("onebrain.yml"), "update_channel: next\n").unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert_eq!(r.message, "1 undocumented key(s)");
        assert!(
            r.details
                .iter()
                .any(|l| l.contains("lack self-documentation") && l.contains("update_channel")),
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
        // is commented here so only the value finding fires).
        fs::write(
            d.path().join("onebrain.yml"),
            "checkpoint:\n  # messages comment\n  messages: 0\n",
        )
        .unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert_eq!(r.message, "1 invalid value(s)");
        assert!(
            r.details
                .iter()
                .any(|l| l.contains("checkpoint.messages") && l.contains("default: 15")),
            "{:?}",
            r.details
        );
        assert!(r.hint.as_deref().unwrap_or("").contains("doctor --fix"));
        // Invalid value AND undocumented key combine into one message.
        fs::write(
            d.path().join("onebrain.yml"),
            "checkpoint:\n  messages: 0\n",
        )
        .unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert_eq!(r.message, "1 invalid value(s) · 1 undocumented key(s)");
        assert!(
            r.hint
                .as_deref()
                .unwrap_or("")
                .contains("reset out-of-range tunables and add the missing"),
            "{:?}",
            r.hint
        );
        // Report-only findings (documented key) carry no reset wording — but
        // an undocumented report-only key still gets the comment hint.
        fs::write(
            d.path().join("onebrain.yml"),
            "folders:\n  # inbox comment\n  inbox: \"\"\n",
        )
        .unwrap();
        let r = config_values_check(d.path());
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.hint.is_none(), "{:?}", r.hint);
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
    fn fix_config_values_inline_mapping_is_failed_outcome() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("onebrain.yml"), "checkpoint: {messages: 0}\n").unwrap();
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Failed(msg) = outcome else {
            panic!("expected Failed, got: {outcome:?}");
        };
        assert!(msg.contains("checkpoint.messages"), "msg: {msg}");
        assert!(msg.contains("edit manually"), "msg: {msg}");
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
    fn fix_config_values_clean_documented_config_is_noop() {
        // A fully-commented, in-range config (the fresh template) is the true
        // no-op: byte-identical after --fix, "already" message.
        let d = tempdir().unwrap();
        let clean = "# channel comment\nupdate_channel: stable\n";
        fs::write(d.path().join("onebrain.yml"), clean).unwrap();
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
            // Keys not present in this legacy config are never added.
            if doc.segments.join(".").starts_with("search.reranker")
                || doc.segments == ["search", "default_top_k"]
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
        // alone — no insertion, no dedupe, no replacement.
        let d = tempdir().unwrap();
        let cfg = "# my own words about the channel\nupdate_channel: stable\n";
        fs::write(d.path().join("onebrain.yml"), cfg).unwrap();
        let outcome = fix_config_values(d.path(), false);
        let FixOutcome::Fixed(msg) = outcome else {
            panic!("expected Fixed, got: {outcome:?}");
        };
        assert!(msg.contains("already in range"), "msg: {msg}");
        let after = fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, cfg);
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
}
